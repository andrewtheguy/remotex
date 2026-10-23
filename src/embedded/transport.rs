//! The private transport between the control plane and one worker.
//!
//! On Unix it is `<instance>/gateway.sock`, `0600` in a `0700` directory. On
//! Windows it is a named pipe, `\\.\pipe\zdesk-<random>`, whose DACL admits this
//! user alone and which refuses clients from other machines. Either way the worker
//! binds it before its handshake names it, and the control plane connects to what
//! the handshake says and to nothing else.
//!
//! Both are byte streams carrying HTTP, so the router in [`super::manager`] hands
//! a browser's connection to either without knowing which it is.

use std::path::Path;

use crate::config::ListenAddr;

#[cfg(unix)]
pub use unix::{WorkerListener, WorkerStream, check_endpoint, connect, endpoint};
#[cfg(windows)]
pub use windows::{WorkerListener, WorkerStream, check_endpoint, connect, endpoint};

/// The endpoint as the resolved config's listen address.
pub fn listen_addr(endpoint: &str) -> ListenAddr {
    #[cfg(unix)]
    return ListenAddr::Unix(std::path::PathBuf::from(endpoint));
    #[cfg(windows)]
    return ListenAddr::Pipe(endpoint.to_owned());
}

/// Make `path` a directory only this user can open: `0700` on Unix, an owner-only
/// DACL on Windows. The instance directories hold the targets' credentials.
pub fn make_private(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use anyhow::Context as _;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot make {} private", path.display()))
    }
    #[cfg(windows)]
    super::owner_only::protect_directory(path)
}

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};

    use anyhow::Context as _;

    use super::super::Claim;

    pub type WorkerStream = tokio::net::UnixStream;

    /// `<dir>/gateway.sock`: in the instance directory, so the directory's mode
    /// guards it.
    pub fn endpoint(dir: &Path) -> String {
        dir.join("gateway.sock").to_string_lossy().into_owned()
    }

    /// Whether a handshake names the socket this instance's worker binds.
    pub fn check_endpoint(dir: &Path, endpoint: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            Path::new(endpoint) == dir.join("gateway.sock"),
            "gateway returned the wrong socket path"
        );
        Ok(())
    }

    pub async fn connect(endpoint: &str) -> std::io::Result<WorkerStream> {
        tokio::net::UnixStream::connect(endpoint).await
    }

    /// The bound socket, removed from the directory when the worker stops.
    pub struct WorkerListener {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    }

    impl WorkerListener {
        pub fn bind(endpoint: &str, _claim: &Claim) -> anyhow::Result<Self> {
            let path = PathBuf::from(endpoint);
            let listener = bind_instance_socket(&path)?;
            match listener
                .set_nonblocking(true)
                .and_then(|()| tokio::net::UnixListener::from_std(listener))
            {
                Ok(listener) => Ok(Self { listener, path }),
                Err(error) => {
                    let _ = std::fs::remove_file(&path);
                    Err(error).context("cannot hand the listening socket to the runtime")
                }
            }
        }
    }

    impl axum::serve::Listener for WorkerListener {
        type Io = tokio::net::UnixStream;
        type Addr = tokio::net::unix::SocketAddr;

        fn accept(&mut self) -> impl Future<Output = (Self::Io, Self::Addr)> + Send {
            axum::serve::Listener::accept(&mut self.listener)
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            self.listener.local_addr()
        }
    }

    impl Drop for WorkerListener {
        fn drop(&mut self) {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("cannot remove {}: {error}", self.path.display()),
            }
        }
    }

    /// Bound with the instance claimed, so a socket already at `path` is a
    /// leftover of a gateway that died: no other can be serving it.
    fn bind_instance_socket(path: &Path) -> anyhow::Result<std::os::unix::net::UnixListener> {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;

        // The directory before the socket. There is no bind that takes a mode, so the
        // socket exists at whatever the umask says for the few microseconds before the
        // `0600` below — and behind that window is a gateway that asks for no login at
        // all. A `0700` parent makes it unreachable rather than merely short. The
        // supervisor already creates instance directories this way; a directory made
        // by hand, or before that was true, is brought up to it here.
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        super::make_private(dir)?;

        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot remove stale socket {}", path.display()));
            }
        }
        let listener = UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot make {} private", path.display()))?;
        Ok(listener)
    }
}

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
    use tokio::sync::mpsc;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    use super::super::Claim;
    use super::super::owner_only::SecurityDescriptor;

    pub type WorkerStream = NamedPipeClient;

    const PIPE_PREFIX: &str = r"\\.\pipe\zdesk-";

    /// How many instances of the pipe wait for a client at once: a listen
    /// backlog. A page load opens several connections together, and each takes an
    /// instance until its replacement is made; a client that finds none waiting
    /// retries for [`BUSY_PATIENCE`].
    const BACKLOG: usize = 4;

    /// How long a client keeps asking a pipe whose every instance is busy.
    const BUSY_PATIENCE: Duration = Duration::from_secs(2);

    /// A fresh `\\.\pipe\zdesk-<random>` per launch.
    ///
    /// Random rather than derived from the instance, because pipe names are one
    /// namespace for the whole machine and any user may create one: a name another
    /// user can predict is a name they can take first. The worker creates the pipe
    /// as its first instance, so the name it prints is one it holds.
    pub fn endpoint(_dir: &Path) -> String {
        format!("{PIPE_PREFIX}{}", uuid::Uuid::new_v4().simple())
    }

    /// Whether a handshake names a pipe a worker could have created.
    pub fn check_endpoint(_dir: &Path, endpoint: &str) -> anyhow::Result<()> {
        let suffix = endpoint.strip_prefix(PIPE_PREFIX);
        anyhow::ensure!(
            suffix.is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "gateway returned a pipe name that is not a worker's: {endpoint:?}"
        );
        Ok(())
    }

    /// Open the pipe, waiting out a moment when every instance is taken.
    pub async fn connect(endpoint: &str) -> std::io::Result<WorkerStream> {
        let deadline = Instant::now() + BUSY_PATIENCE;
        loop {
            match ClientOptions::new().open(endpoint) {
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && Instant::now() < deadline => {}
                result => return result,
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The pipe, with up to [`BACKLOG`] instances waiting for clients.
    ///
    /// Each waiting instance is a task that, once a client connects, creates the
    /// instance to take its place before anything it does can wait, so its slot is
    /// listening again whenever it pauses to hand the connection over. A slot whose
    /// replacement cannot be created hands the connection over first and stays
    /// empty while it retries. Dropping the listener ends the tasks and closes the
    /// pipe.
    pub struct WorkerListener {
        name: String,
        accepted: mpsc::Receiver<NamedPipeServer>,
        _instances: tokio::task::JoinSet<()>,
    }

    impl WorkerListener {
        pub fn bind(endpoint: &str, _claim: &Claim) -> anyhow::Result<Self> {
            let security = Arc::new(
                SecurityDescriptor::pipe().context("cannot build the pipe's owner-only descriptor")?,
            );
            let (sender, accepted) = mpsc::channel(BACKLOG);
            let mut instances = tokio::task::JoinSet::new();
            for index in 0..BACKLOG {
                let server = create(endpoint, &security, index == 0)
                    .with_context(|| format!("cannot create the pipe {endpoint}"))?;
                instances.spawn(wait_for_clients(
                    server,
                    endpoint.to_owned(),
                    security.clone(),
                    sender.clone(),
                ));
            }
            Ok(Self {
                name: endpoint.to_owned(),
                accepted,
                _instances: instances,
            })
        }
    }

    impl axum::serve::Listener for WorkerListener {
        type Io = NamedPipeServer;
        type Addr = String;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            match self.accepted.recv().await {
                Some(server) => (server, self.name.clone()),
                // Every instance task has ended, which only a panic in one can do.
                None => std::future::pending().await,
            }
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            Ok(self.name.clone())
        }
    }

    /// One instance of the pipe, created with the owner-only DACL.
    ///
    /// `first` claims the name: creating the first instance of a pipe that already
    /// exists fails, where a later instance would join whoever made it.
    fn create(name: &str, security: &SecurityDescriptor, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut attributes = security.attributes();
        // SAFETY: the attributes point at a descriptor that outlives the call, which
        // copies what it needs.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
        }
    }

    async fn wait_for_clients(
        first: NamedPipeServer,
        name: String,
        security: Arc<SecurityDescriptor>,
        accepted: mpsc::Sender<NamedPipeServer>,
    ) {
        let mut waiting = Some(first);
        loop {
            let server = match waiting.take() {
                Some(server) => server,
                None => replacement(&name, &security).await,
            };
            let connected = server.connect().await;
            // Synchronously, before the send below can wait on a busy accept loop.
            waiting = match create(&name, &security, false) {
                Ok(next) => Some(next),
                Err(error) => {
                    log::warn!("cannot create another instance of {name}: {error}");
                    None
                }
            };
            match connected {
                Ok(()) => {
                    if accepted.send(server).await.is_err() {
                        return;
                    }
                }
                // A client that went away before it was connected. The instance is
                // spent either way.
                Err(error) => log::debug!("a client left {name} before it was served: {error}"),
            }
        }
    }

    /// An instance for a slot whose last replacement could not be created,
    /// retried once a second.
    async fn replacement(name: &str, security: &SecurityDescriptor) -> NamedPipeServer {
        loop {
            match create(name, security, false) {
                Ok(server) => return server,
                Err(error) => {
                    log::warn!("cannot create another instance of {name}: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}
