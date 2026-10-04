//! The native multi-instance control plane.
//!
//! One TUI owns one public loopback port and one subprocess per running instance.
//! The subprocesses keep their endpoints private — see [`super::transport`]; this
//! process routes raw HTTP connections by `Host`, so ordinary requests and
//! WebSocket upgrades follow exactly the same path without reimplementing either
//! protocol.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use futures_util::StreamExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

use super::{Handshake, transport};
use crate::config::{
    DEFAULT_BRANDING, DEFAULT_SIZE, Protocol, TargetConfig,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_GRACE: Duration = Duration::from_millis(1500);
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_HEAD: usize = 64 * 1024;
const MASTER_HOST: &str = "remotex.localhost";
/// The binary's version, so the header answers what `--version` would without
/// leaving the TUI. Every instance is a worker of this same binary.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A first launch's complete instance config: no server block and no pretend
/// target. The TUI creates this atomically before adding the instance to its list.
pub const INSTANCE_TEMPLATE: &str = r#"# A remotex local instance.
#
# There is no [server] block. The TUI control plane owns the shared loopback
# listener, this instance's subdomain, its private endpoint, and its launch
# token. Only [branding] and [[targets]] belong here.

# [branding]
# text = "remotex"
# logo = "/path/to/logo.png"

# [[targets]]
# name = "work"
# protocol = "rdp"
# host = "192.168.1.20"
# username = "andrew"
# password = "…"
# domain = "CORP"

# [[targets]]
# name = "pi"
# protocol = "vnc"
# host = "192.168.1.30"
# vnc_password = "…"
"#;

/// Inputs resolved by the CLI before terminal state is changed.
#[derive(Clone, Debug)]
pub struct TuiOptions {
    pub port: u16,
    pub instances_dir: PathBuf,
}

/// The platform's private application-data directory for local instances.
pub fn default_instances_dir() -> anyhow::Result<PathBuf> {
    // Local rather than roaming: the configs hold credentials, and a roaming
    // profile would copy them to every machine the account signs in to.
    #[cfg(windows)]
    {
        let data = std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .context("LOCALAPPDATA is not set; pass --instances-dir")?;
        Ok(PathBuf::from(data).join("remotex").join("instances"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::home_dir().context("cannot find the home directory; pass --instances-dir")?;
        Ok(home.join("Library/Application Support/remotex/instances"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(data).join("remotex/instances"));
        }
        let home = std::env::home_dir().context("cannot find the home directory; pass --instances-dir")?;
        Ok(home.join(".local/share/remotex/instances"))
    }
}

/// Run the terminal UI and its shared-port router until `q` or a shutdown signal.
pub async fn run_tui(options: TuiOptions) -> anyhow::Result<()> {
    let binary = std::env::current_exe().context("cannot locate the remotex executable")?;
    let mut supervisor = Supervisor::open(options.instances_dir.clone(), binary).await?;
    let router = SharedPort::bind(options.port, supervisor.routes()).await?;

    let mut terminal = TerminalSession::enter()?;
    let mut screen = Screen::default();
    let mut events = EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let mut selected = 0usize;
    let mut view = View::List;
    let mut message = format!(
        "control plane listening on http://{MASTER_HOST}:{}",
        router.port()
    );

    loop {
        let instances = supervisor.instances();
        selected = selected.min(instances.len().saturating_sub(1));
        screen.draw(
            render(&options, router.port(), &instances, selected, &view, &message)?,
            &mut std::io::stdout().lock(),
        )?;

        tokio::select! {
            _ = ticks.tick() => {
                if let Some(ended) = supervisor.poll_exits().await? {
                    message = ended;
                }
            }
            result = &mut interrupt => {
                result.context("cannot listen for Ctrl+C")?;
                break;
            }
            event = events.next() => {
                let Some(event) = event else {
                    anyhow::bail!("terminal input ended");
                };
                let event = event.context("cannot read terminal input")?;
                let Event::Key(key) = event else { continue };
                if key.kind != KeyEventKind::Press { continue; }

                if let View::Naming(name) = &mut view {
                    match key.code {
                        KeyCode::Esc => {
                            view = View::List;
                            message = "instance creation cancelled".to_owned();
                        }
                        KeyCode::Backspace => {
                            name.pop();
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            name.push(character);
                        }
                        KeyCode::Enter => {
                            let requested = std::mem::take(name);
                            match supervisor.create(&requested).await {
                                Ok(index) => {
                                    selected = index;
                                    view = View::List;
                                    message = format!("created {requested}; edit its config, then start it");
                                }
                                Err(error) => message = format!("cannot create instance: {error:#}"),
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // The specs screen is a reader, so it takes only the keys that
                // move within it or leave it: acting on an instance is the list's,
                // and a stop pressed over a page of text is one nobody aimed.
                if let View::Specs { lines, offset, .. } = &mut view {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => {
                            *offset = (*offset + 1).min(lines.len().saturating_sub(1));
                        }
                        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => view = View::List,
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(instances.len().saturating_sub(1));
                    }
                    KeyCode::Char('n') => view = View::Naming(String::new()),
                    KeyCode::Char('R') => {
                        supervisor.rescan().await?;
                        message = "rescanned the instances directory".to_owned();
                    }
                    KeyCode::Char('a') => {
                        let names: Vec<_> = supervisor.instances().into_iter().map(|i| i.name).collect();
                        let mut failed = false;
                        for name in names {
                            if let Err(error) = supervisor.start(&name).await {
                                message = format!("{name}: {error:#}");
                                failed = true;
                            }
                        }
                        if !failed {
                            message = "started every stopped instance".to_owned();
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(instance) = instances.get(selected) {
                            let name = instance.name.clone();
                            message = if instance.status == InstanceStatus::Running {
                                format!("{name} is already running; x stops it")
                            } else {
                                match supervisor.start(&name).await {
                                    Ok(()) => format!("started {name}"),
                                    Err(error) => format!("{name}: {error:#}"),
                                }
                            };
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(instance) = instances.get(selected) {
                            view = View::Specs {
                                name: instance.name.clone(),
                                lines: describe_instance(instance, router.port()),
                                offset: 0,
                            };
                        }
                    }
                    KeyCode::Char('x') => {
                        if let Some(instance) = instances.get(selected) {
                            let name = instance.name.clone();
                            message = match supervisor.stop(&name).await {
                                Ok(()) => format!("stopped {name}"),
                                Err(error) => format!("{name}: {error:#}"),
                            };
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(instance) = instances.get(selected) {
                            let name = instance.name.clone();
                            message = match supervisor.restart(&name).await {
                                Ok(()) => format!("restarted {name}"),
                                Err(error) => format!("{name}: {error:#}"),
                            };
                        }
                    }
                    KeyCode::Char('o') => {
                        if let Some(instance) = instances.get(selected) {
                            let url = instance_url(&instance.name, router.port());
                            message = if instance.status != InstanceStatus::Running {
                                format!("{} is {}; s starts it", instance.name, instance.status.label())
                            } else {
                                match open_browser(&url) {
                                    Ok(()) => format!("opening {url}"),
                                    Err(error) => format!("cannot open {url}: {error:#}"),
                                }
                            };
                        }
                    }
                    KeyCode::Char('e') => {
                        if let Some(instance) = instances.get(selected) {
                            let name = instance.name.clone();
                            let path = instance.config_path();
                            terminal.suspend()?;
                            let edited = edit_config(&path).await;
                            terminal.resume()?;
                            screen.invalidate();
                            events = EventStream::new();
                            message = match edited.and_then(|()| validate_config(&path)) {
                                Ok(()) => format!("{name} config is valid; restart it to apply changes"),
                                Err(error) => format!("{name} config: {error:#}"),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    terminal.suspend()?;
    supervisor.shutdown().await;
    drop(router);
    println!("remotex: every instance stopped");
    Ok(())
}

/// Opens the instance config in the operator's editor and waits for it.
///
/// `VISUAL` and `EDITOR` hold a shell command line, not a program path: `subl -w`
/// and `code --wait` are ordinary values, and every tool that honours these
/// variables lets the shell split them. So the value is handed to the platform's
/// shell with the path kept out of the words it splits.
async fn edit_config(path: &Path) -> anyhow::Result<()> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| String::from(DEFAULT_EDITOR));
    let status = editor_command(&editor, path)
        .status()
        .await
        .with_context(|| format!("cannot start editor {editor}"))?;
    anyhow::ensure!(status.success(), "editor {editor} exited with {status}");
    Ok(())
}

#[cfg(unix)]
const DEFAULT_EDITOR: &str = "vi";
#[cfg(windows)]
const DEFAULT_EDITOR: &str = "notepad";

/// `sh -c`, with the path as a positional argument.
#[cfg(unix)]
fn editor_command(editor: &str, path: &Path) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!("{editor} \"$@\"")).arg("sh").arg(path);
    command
}

/// `cmd /c`, with the path quoted after the editor's own words.
///
/// The line is handed over raw, because cmd does not split its command line the
/// way a program's `argv` is split and an escaped argument would reach it with
/// its escapes. `/s` strips exactly the outer pair of quotes, leaving
/// `<editor> "%REMOTEX_EDIT_PATH%"`. The path arrives in that variable rather than
/// in the line, because cmd expands `%NAME%` inside quotes too and a directory may
/// be called `%TEMP%`; what a variable expands to is not expanded again, and a
/// Windows path cannot hold a `"` to break out of its quotes. `/v:off` keeps a `!`
/// literal whatever the registry says about delayed expansion, and `/d` skips any
/// AutoRun it sets.
#[cfg(windows)]
fn editor_command(editor: &str, path: &Path) -> Command {
    let mut command = Command::new("cmd.exe");
    command
        .env("REMOTEX_EDIT_PATH", path)
        .raw_arg(format!("/d /v:off /s /c \"{editor} \"%REMOTEX_EDIT_PATH%\"\""));
    command
}

/// The instance's origin on the shared port. Its `/` seeds the login cookie, so
/// a browser sent here lands in the SPA already signed in.
fn instance_url(name: &str, port: u16) -> String {
    format!("http://{name}.{MASTER_HOST}:{port}")
}

/// Hands `url` to the desktop's default browser without waiting for it: some
/// launchers stay until the browser exits. None of them gets the terminal, which
/// the TUI is drawing on.
fn open_browser(url: &str) -> anyhow::Result<()> {
    browser_command(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start the browser launcher")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn browser_command(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn browser_command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

/// `start`, whose first quoted word is a window title, so the empty one keeps
/// the URL from being taken for it.
#[cfg(windows)]
fn browser_command(url: &str) -> Command {
    let mut command = Command::new("cmd.exe");
    command.raw_arg(format!("/d /c start \"\" \"{url}\""));
    command
}

fn validate_config(path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    super::check(&text)
}

struct TerminalSession {
    active: bool,
}

impl TerminalSession {
    fn enter() -> anyhow::Result<Self> {
        anyhow::ensure!(std::io::IsTerminal::is_terminal(&std::io::stdin()), "the TUI needs a terminal");
        anyhow::ensure!(std::io::IsTerminal::is_terminal(&std::io::stdout()), "the TUI needs a terminal");
        terminal::enable_raw_mode().context("cannot enable terminal raw mode")?;
        if let Err(error) = execute!(std::io::stdout(), EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("cannot enter the alternate screen");
        }
        Ok(Self { active: true })
    }

    fn suspend(&mut self) -> anyhow::Result<()> {
        if self.active {
            execute!(std::io::stdout(), Show, LeaveAlternateScreen)
                .context("cannot leave the alternate screen")?;
            terminal::disable_raw_mode().context("cannot restore terminal input")?;
            self.active = false;
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        if !self.active {
            terminal::enable_raw_mode().context("cannot enable terminal raw mode")?;
            execute!(std::io::stdout(), EnterAlternateScreen, Hide)
                .context("cannot enter the alternate screen")?;
            self.active = true;
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }
}

/// The frame currently on the terminal, so an identical one is not written again.
///
/// The loop wakes on a 250 ms tick whether or not anything moved, and a frame
/// begins by erasing the display. Repainting that unchanged frame four times a
/// second is what stops a terminal that anchors a selection to the text under it —
/// VS Code's — from letting anyone select a URL off this screen: the selection is
/// dropped by the next tick's erase. So the frame is built into a buffer and
/// compared, and an idle control plane writes nothing at all.
#[derive(Default)]
struct Screen {
    shown: Vec<u8>,
}

impl Screen {
    fn draw(&mut self, frame: Vec<u8>, out: &mut impl Write) -> anyhow::Result<()> {
        if frame == self.shown {
            return Ok(());
        }
        out.write_all(&frame).context("cannot write to the terminal")?;
        out.flush().context("cannot write to the terminal")?;
        self.shown = frame;
        Ok(())
    }

    /// Forget what is on screen, for when something else has been writing to it.
    fn invalidate(&mut self) {
        self.shown.clear();
    }
}

/// What the screen is showing. One value rather than a flag per overlay, because
/// the list, the name prompt and the specs page each take the keyboard whole.
enum View {
    List,
    /// A new instance's name, as it is being typed.
    Naming(String),
    /// One instance's specs, as read when the page was opened, scrolled by
    /// `offset` lines.
    Specs {
        name: String,
        lines: Vec<String>,
        offset: usize,
    },
}

fn render(
    options: &TuiOptions,
    port: u16,
    instances: &[InstanceInfo],
    selected: usize,
    view: &View,
    message: &str,
) -> anyhow::Result<Vec<u8>> {
    let (width, height) = terminal::size().context("cannot read terminal size")?;
    let mut frame = Vec::new();
    queue!(frame, MoveTo(0, 0), Clear(ClearType::All))?;
    if let View::Specs { name, lines, offset } = view {
        render_specs(&mut frame, width, height, name, lines, *offset)?;
        return Ok(frame);
    }
    line(
        &mut frame,
        0,
        width,
        &format!("remotex {VERSION} local control plane"),
        Some(Color::Cyan),
        true,
    )?;
    line(
        &mut frame,
        2,
        width,
        &format!("master   http://{MASTER_HOST}:{port}"),
        None,
        false,
    )?;
    line(
        &mut frame,
        3,
        width,
        &format!("instances {}", options.instances_dir.display()),
        None,
        false,
    )?;
    line(
        &mut frame,
        5,
        width,
        "  instance              state      URL",
        Some(Color::DarkGrey),
        false,
    )?;

    let available = height.saturating_sub(10) as usize;
    let start = if selected >= available && available > 0 {
        selected + 1 - available
    } else {
        0
    };
    for (row, (index, instance)) in instances.iter().enumerate().skip(start).take(available).enumerate() {
        let marker = if index == selected { '›' } else { ' ' };
        let text = format!(
            "{marker} {:<20} {:<10} {}",
            instance.name,
            instance.status.label(),
            instance_url(&instance.name, port)
        );
        line(
            &mut frame,
            6 + row as u16,
            width,
            &text,
            (index == selected).then_some(Color::Yellow),
            index == selected,
        )?;
    }
    if instances.is_empty() {
        line(&mut frame, 6, width, "  no instances — press n to create one", Some(Color::Yellow), false)?;
    }

    let footer = height.saturating_sub(3);
    if let View::Naming(name) = view {
        line(
            &mut frame,
            footer,
            width,
            &format!("new instance name: {name}_"),
            Some(Color::Yellow),
            true,
        )?;
        line(&mut frame, footer + 1, width, "Enter create · Esc cancel", Some(Color::DarkGrey), false)?;
    } else {
        line(
            &mut frame,
            footer,
            width,
            "↑↓ select · Enter specs · s start · x stop · r restart · a start all · o open · n new · e edit · R rescan · q quit",
            Some(Color::DarkGrey),
            false,
        )?;
        let detail = instances
            .get(selected)
            .and_then(|instance| instance.detail.as_deref())
            .unwrap_or(message);
        line(&mut frame, footer + 1, width, detail, None, false)?;
    }
    Ok(frame)
}

fn render_specs(
    frame: &mut Vec<u8>,
    width: u16,
    height: u16,
    name: &str,
    lines: &[String],
    offset: usize,
) -> anyhow::Result<()> {
    line(frame, 0, width, &format!("instance {name}"), Some(Color::Cyan), true)?;
    let available = height.saturating_sub(4) as usize;
    for (row, text) in lines.iter().skip(offset).take(available).enumerate() {
        line(frame, 2 + row as u16, width, text, None, false)?;
    }
    let more = if offset + available < lines.len() { " · more below" } else { "" };
    line(
        frame,
        height.saturating_sub(1),
        width,
        &format!("↑↓ scroll · Esc back{more}"),
        Some(Color::DarkGrey),
        false,
    )?;
    Ok(())
}

/// Everything known about one instance, as the specs page's lines: its own state
/// and paths, then what its config says each target will do.
///
/// Read from the file when the page is opened rather than from the child process,
/// because there is nothing to ask a child — the gateway parses this config at
/// launch and keeps no channel back. So for a running instance this is the config
/// it *would* start on, which is exactly what somebody who has just edited it
/// wants to check, and the same reason `e` says to restart.
fn describe_instance(instance: &InstanceInfo, port: u16) -> Vec<String> {
    let mut lines = vec![
        spec("state", instance.status.label()),
        spec("url", &instance_url(&instance.name, port)),
        spec("config", &instance.config_path().display().to_string()),
        spec("log", &instance.log_path().display().to_string()),
    ];
    if let Some(detail) = &instance.detail {
        lines.push(spec("detail", detail));
    }

    let file = match super::Instance::new(&instance.dir).load() {
        Ok(file) => file,
        Err(error) => {
            lines.push(String::new());
            lines.push(format!("this config will not start: {error:#}"));
            return lines;
        }
    };
    let branding = file
        .branding
        .as_ref()
        .and_then(|branding| branding.text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_BRANDING);
    lines.push(spec("shown as", branding));
    lines.push(spec("targets", &file.targets.len().to_string()));

    if file.targets.is_empty() {
        lines.push(String::new());
        lines.push("no targets yet — press e to add one".to_owned());
    }
    for target in &file.targets {
        lines.push(String::new());
        lines.push(format!("target {}", target.name));
        lines.extend(target_specs(target));
    }
    lines
}

/// One `[[targets]]` profile as sentences: what this target will do, not which
/// keys were written. Credentials are reported as present, never printed — this
/// page is on somebody's screen, and the passwords are the reason the instance
/// directory is `0700`.
fn target_specs(target: &TargetConfig) -> Vec<String> {
    let mut lines = Vec::new();
    let protocol = match target.subtype {
        None => target.protocol.name().to_owned(),
        Some(subtype) => format!("{} {}", target.protocol.name(), subtype.name()),
    };
    lines.push(spec("protocol", &protocol));
    lines.push(spec("address", &format!("{}:{}", target.host, target.port)));

    let mut credentials = Vec::new();
    if !target.username.is_empty() {
        credentials.push(format!("user {}", target.username));
    }
    if let Some(domain) = target.domain.as_deref().filter(|domain| !domain.is_empty()) {
        credentials.push(format!("domain {domain}"));
    }
    if !target.password.is_empty() {
        credentials.push("account password set".to_owned());
    }
    if !target.vnc_password.is_empty() {
        credentials.push("vnc password set".to_owned());
    }
    if credentials.is_empty() {
        credentials.push("none configured".to_owned());
    }
    lines.push(spec("sign-in", &credentials.join(", ")));

    lines.push(spec("size", &describe_size(target)));
    lines.push(spec("picker", &describe_offers(target)));

    if target.protocol == Protocol::Rdp {
        lines.push(spec("security", "nla — the credentials are checked before the session"));
        lines.push(spec(
            "graphics",
            if target.egfx() {
                "egfx pipeline; a resize is a graphics reset"
            } else {
                "bitmap updates at the opening size"
            },
        ));
    }

    // Beside the clipboard rather than inside the block above: sound is the
    // protocol's own question, and it is VNC that answers it today.
    lines.push(spec("audio", &describe_audio(target)));
    lines.push(spec("render", &target.render_summary()));
    lines
}

/// The size a session on this target has: the one it keeps, and the window where
/// the picker offers following it.
fn describe_size(target: &TargetConfig) -> String {
    if !target.sized() {
        return "the Mac's displays as they are".to_owned();
    }
    let (w, h) = DEFAULT_SIZE;
    match (target.size, target.offers().resize) {
        (Some((w, h)), true) => format!("{w}×{h} points, or the client's window"),
        (Some((w, h)), false) => format!("{w}×{h} points"),
        (None, true) => format!("the client's window, or {w}×{h} points on a phone"),
        (None, false) => format!("{w}×{h} points, the default"),
    }
}

/// What whoever starts a session on this target chooses at the picker, which is
/// its type's to offer and not a key of the file.
fn describe_offers(target: &TargetConfig) -> String {
    let offers = target.offers();
    let mut choices = Vec::new();
    if offers.resize {
        choices.push("resize".to_owned());
    }
    if offers.audio {
        choices.push("sound".to_owned());
    }
    if let Some(passthrough) = offers.passthrough {
        choices.push(format!("{} passed through", passthrough.stream()));
    }
    if offers.placement {
        choices.push("where the second display sits".to_owned());
    }
    if choices.is_empty() {
        "nothing to choose".to_owned()
    } else {
        format!("offers {}", choices.join(", "))
    }
}

/// A target's audio as it will sound on the wire, in a session that takes it as
/// Opus. Kilobits because the config speaks kilobits, and the codec named the way
/// `ServerMsg::AudioFormat` names it. Lossless is the picker's other format and
/// has no key to describe.
fn describe_audio(target: &TargetConfig) -> String {
    if target.media_stream() {
        return "the Mac's AAC-ELD, passed through".to_owned();
    }
    if !target.offers().audio {
        return "none".to_owned();
    }
    let plan = target.audio_plan();
    let ceiling = plan.bitrate_bps / 1000;
    let floor = sound_opus::walk::BITRATE_FLOOR as i32;
    if plan.adaptive && floor < plan.bitrate_bps {
        format!("opus ≤{ceiling} kbit/s, adaptive down to {} kbit/s, or flac", floor / 1000)
    } else {
        // A walk with nothing under it, or none: the rate is the rate.
        format!("opus at {ceiling} kbit/s, or flac")
    }
}

/// One `label   value` row of a specs page, indented under its heading.
fn spec(label: &str, value: &str) -> String {
    format!("  {label:<10} {value}")
}

fn line(
    stdout: &mut impl Write,
    row: u16,
    width: u16,
    text: &str,
    color: Option<Color>,
    bold: bool,
) -> std::io::Result<()> {
    let clipped: String = text.chars().take(width as usize).collect();
    queue!(stdout, MoveTo(0, row))?;
    if let Some(color) = color {
        queue!(stdout, SetForegroundColor(color))?;
    }
    if bold {
        queue!(stdout, SetAttribute(Attribute::Bold))?;
    }
    queue!(stdout, Print(clipped), SetAttribute(Attribute::Reset), ResetColor)
}

/// Stable state shown by both the TUI and the master landing page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceStatus {
    Stopped,
    Starting,
    Running,
    /// The gateway ended cleanly without this manager asking: exit code 0, or a
    /// SIGINT/SIGTERM/SIGHUP from outside. Nothing about it failed.
    Exited,
    /// The gateway could not start, exited with a non-zero code, or died on a
    /// crash signal.
    Failed,
}

impl InstanceStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }
}

/// One row of the manager's public state, with no launch token or endpoint.
#[derive(Clone, Debug)]
pub struct InstanceInfo {
    pub name: String,
    pub status: InstanceStatus,
    /// The instance directory, which is where every path this instance has comes
    /// from — see [`super::Instance`] for the ones the gateway itself uses.
    pub dir: PathBuf,
    pub detail: Option<String>,
}

impl InstanceInfo {
    /// The one file a user edits.
    pub fn config_path(&self) -> PathBuf {
        super::Instance::new(&self.dir).config_path()
    }

    /// Where a gateway that ended said why.
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("gateway.log")
    }
}

enum InstanceState {
    Stopped,
    Starting,
    /// Boxed because a Windows child process carries its handles and job state
    /// inline, which makes this variant many times the size of the others.
    Running(Box<RunningGateway>),
    /// A clean exit this manager did not request; the string says how it ended.
    Exited(String),
    Failed(String),
}

struct ManagedInstance {
    name: String,
    dir: PathBuf,
    state: InstanceState,
}

struct RunningGateway {
    child: Child,
    endpoint: String,
    token: String,
}

/// Owns every instance subprocess. Dropping the process closes every child's
/// liveness pipe; [`Self::shutdown`] is the polite path on top of that guarantee.
pub struct Supervisor {
    root: PathBuf,
    binary: PathBuf,
    instances: Vec<ManagedInstance>,
    routes: RouteTable,
}

impl Supervisor {
    pub async fn open(root: PathBuf, binary: PathBuf) -> anyhow::Result<Self> {
        create_private_dir(&root)?;
        let mut manager = Self {
            root,
            binary,
            instances: Vec::new(),
            routes: RouteTable::default(),
        };
        manager.rescan().await?;
        Ok(manager)
    }

    pub fn routes(&self) -> RouteTable {
        self.routes.clone()
    }

    pub fn instances(&self) -> Vec<InstanceInfo> {
        self.instances
            .iter()
            .map(|instance| {
                let (status, detail) = match &instance.state {
                    InstanceState::Stopped => (InstanceStatus::Stopped, None),
                    InstanceState::Starting => (InstanceStatus::Starting, None),
                    InstanceState::Running(_) => (InstanceStatus::Running, None),
                    InstanceState::Exited(how) => (InstanceStatus::Exited, Some(how.clone())),
                    InstanceState::Failed(error) => (InstanceStatus::Failed, Some(error.clone())),
                };
                InstanceInfo {
                    name: instance.name.clone(),
                    status,
                    dir: instance.dir.clone(),
                    detail,
                }
            })
            .collect()
    }

    pub async fn rescan(&mut self) -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        for entry in std::fs::read_dir(&self.root)
            .with_context(|| format!("cannot list {}", self.root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if valid_instance_name(&name).is_ok() {
                names.insert(name);
            }
        }

        let mut previous: BTreeMap<_, _> = std::mem::take(&mut self.instances)
            .into_iter()
            .map(|instance| (instance.name.clone(), instance))
            .collect();
        for name in names {
            if let Some(instance) = previous.remove(&name) {
                self.instances.push(instance);
            } else {
                let dir = self.root.join(&name);
                // A directory made or copied in by hand keeps whatever access it came
                // with until it is made private here, the files already in it included.
                transport::make_private(&dir)?;
                bootstrap_config(&dir)?;
                self.instances.push(ManagedInstance {
                    name,
                    dir,
                    state: InstanceState::Stopped,
                });
            }
        }
        // A running process stays manageable even if its directory was renamed or
        // removed behind the TUI. Stopped vanished entries simply leave the list.
        self.instances.extend(previous.into_values().filter(|instance| {
            matches!(instance.state, InstanceState::Running(_) | InstanceState::Starting)
        }));
        self.instances.sort_by(|a, b| a.name.cmp(&b.name));
        self.publish().await;
        Ok(())
    }

    pub async fn create(&mut self, name: &str) -> anyhow::Result<usize> {
        valid_instance_name(name)?;
        let dir = self.root.join(name);
        std::fs::create_dir(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        transport::make_private(&dir)?;
        if let Err(error) = bootstrap_config(&dir) {
            let _ = std::fs::remove_dir(&dir);
            return Err(error);
        }
        self.rescan().await?;
        self.instances
            .iter()
            .position(|instance| instance.name == name)
            .context("the new instance did not appear after rescan")
    }

    pub async fn start(&mut self, name: &str) -> anyhow::Result<()> {
        let index = self.index(name)?;
        if matches!(
            self.instances[index].state,
            InstanceState::Running(_) | InstanceState::Starting
        ) {
            return Ok(());
        }
        let dir = self.instances[index].dir.clone();
        let instance = super::Instance::new(&dir);
        validate_config(&instance.config_path())
            .with_context(|| format!("instance {name:?} has an invalid config"))?;
        // Asked here as well as by the worker, which holds the claim, so an
        // instance another control plane is serving says so instead of ending in a
        // handshake that never comes. A start that races past this one still meets
        // the worker's.
        drop(instance.claim().await?);
        self.instances[index].state = InstanceState::Starting;
        self.publish().await;

        match spawn_gateway(&self.binary, &dir).await {
            Ok(gateway) => {
                self.instances[index].state = InstanceState::Running(Box::new(gateway));
                self.publish().await;
                Ok(())
            }
            Err(error) => {
                // The state carries a rendering because the TUI prints it; the
                // caller gets the error itself, cause chain intact.
                self.instances[index].state = InstanceState::Failed(format!("{error:#}"));
                self.publish().await;
                Err(error)
            }
        }
    }

    pub async fn stop(&mut self, name: &str) -> anyhow::Result<()> {
        let index = self.index(name)?;
        let state = std::mem::replace(&mut self.instances[index].state, InstanceState::Stopped);
        if let InstanceState::Running(gateway) = state {
            stop_gateway(*gateway).await;
        }
        self.publish().await;
        Ok(())
    }

    pub async fn restart(&mut self, name: &str) -> anyhow::Result<()> {
        self.stop(name).await?;
        self.start(name).await
    }

    /// Notice gateways that ended without being asked. Returns a one-line
    /// account of the last one for the status line, or `None` when nothing
    /// changed.
    pub async fn poll_exits(&mut self) -> anyhow::Result<Option<String>> {
        let mut ended = None;
        for instance in &mut self.instances {
            let InstanceState::Running(gateway) = &mut instance.state else {
                continue;
            };
            if let Some(status) = gateway.child.try_wait().context("cannot inspect gateway child")? {
                let exit = ExitKind::of(status);
                let detail = format!("{exit}; see {}", instance.dir.join("gateway.log").display());
                ended = Some(format!("{}: {exit}", instance.name));
                instance.state = if exit.is_clean() {
                    InstanceState::Exited(detail)
                } else {
                    InstanceState::Failed(detail)
                };
            }
        }
        if ended.is_some() {
            self.publish().await;
        }
        Ok(ended)
    }

    pub async fn shutdown(&mut self) {
        for instance in &mut self.instances {
            let state = std::mem::replace(&mut instance.state, InstanceState::Stopped);
            if let InstanceState::Running(gateway) = state {
                stop_gateway(*gateway).await;
            }
        }
        self.publish().await;
    }

    fn index(&self, name: &str) -> anyhow::Result<usize> {
        self.instances
            .iter()
            .position(|instance| instance.name == name)
            .with_context(|| format!("unknown instance {name:?}"))
    }

    async fn publish(&self) {
        let mut published = BTreeMap::new();
        for instance in &self.instances {
            let (status, target) = match &instance.state {
                InstanceState::Stopped => (InstanceStatus::Stopped, None),
                InstanceState::Starting => (InstanceStatus::Starting, None),
                InstanceState::Exited(_) => (InstanceStatus::Exited, None),
                InstanceState::Failed(_) => (InstanceStatus::Failed, None),
                InstanceState::Running(gateway) => (
                    InstanceStatus::Running,
                    Some(RouteTarget {
                        endpoint: gateway.endpoint.clone(),
                        token: gateway.token.clone(),
                    }),
                ),
            };
            published.insert(instance.name.clone(), PublishedInstance { status, target });
        }
        *self.routes.inner.write().await = published;
    }
}

/// How a gateway this manager did not stop came to end.
///
/// The gateway exits 0 on SIGINT or SIGTERM after logging "shutdown signal
/// received", and on its stdin closing; a `pkill` aimed at some other remotex
/// therefore shows up here as a clean exit, not a crash. Only a non-zero code
/// or a crash is a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitKind {
    /// Exit code 0.
    Clean,
    /// Exit code other than 0.
    Code(i32),
    /// Ended from outside on purpose: SIGINT, SIGTERM or SIGHUP on Unix, and on
    /// Windows the status of a process a Ctrl+C or Ctrl+Break ended.
    Stopped(Termination),
    /// Anything else that ended it: SIGSEGV, SIGABRT, SIGKILL and so on, or on
    /// Windows an exception status such as an access violation or a fail-fast.
    Crashed(Termination),
}

/// What ended a gateway that did not exit by itself: a signal number on Unix, an
/// `NTSTATUS` on Windows.
#[cfg(unix)]
type Termination = i32;
#[cfg(windows)]
type Termination = u32;

impl ExitKind {
    #[cfg(unix)]
    fn of(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        match (status.code(), status.signal()) {
            (Some(0), _) => Self::Clean,
            (Some(code), _) => Self::Code(code),
            (None, Some(signal)) if [libc::SIGINT, libc::SIGTERM, libc::SIGHUP].contains(&signal) => {
                Self::Stopped(signal)
            }
            (None, Some(signal)) => Self::Crashed(signal),
            (None, None) => Self::Crashed(0),
        }
    }

    /// Windows has one number for all of it. An exit code with both severity bits
    /// set is an `NTSTATUS` error, which a process does not choose to exit with:
    /// the system ended it. A `TerminateProcess` from outside is not among them —
    /// it exits with whatever code its caller named — and reads as that code.
    #[cfg(windows)]
    fn of(status: std::process::ExitStatus) -> Self {
        match status.code().map(|code| code as u32) {
            Some(0) => Self::Clean,
            Some(status @ STATUS_CONTROL_C_EXIT) => Self::Stopped(status),
            Some(status) if status & 0xC000_0000 == 0xC000_0000 => Self::Crashed(status),
            Some(code) => Self::Code(code as i32),
            None => Self::Crashed(0),
        }
    }

    fn is_clean(self) -> bool {
        matches!(self, Self::Clean | Self::Stopped(_))
    }
}

#[cfg(unix)]
fn termination_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        _ => "an uncommon signal",
    }
}

#[cfg(windows)]
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

#[cfg(windows)]
fn termination_name(status: u32) -> &'static str {
    match status {
        0xC000_0005 => "STATUS_ACCESS_VIOLATION",
        0xC000_0017 => "STATUS_NO_MEMORY",
        0xC000_001D => "STATUS_ILLEGAL_INSTRUCTION",
        0xC000_0094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
        0xC000_00FD => "STATUS_STACK_OVERFLOW",
        STATUS_CONTROL_C_EXIT => "STATUS_CONTROL_C_EXIT",
        0xC000_0374 => "STATUS_HEAP_CORRUPTION",
        // What a Rust panic under `panic = "abort"` ends in: `abort` is a fail-fast.
        0xC000_0409 => "STATUS_STACK_BUFFER_OVERRUN, a fail-fast such as an abort",
        _ => "an uncommon status",
    }
}

impl std::fmt::Display for ExitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            #[cfg(unix)]
            Self::Clean => write!(
                f,
                "the gateway shut down cleanly (exit code 0) without this control plane asking; \
                 something else sent it SIGINT or SIGTERM, such as a pkill matching 'remotex serve'"
            ),
            // A worker has a console of its own on Windows, so no Ctrl+C typed
            // anywhere reaches it: there is no outside hand to name.
            #[cfg(windows)]
            Self::Clean => write!(
                f,
                "the gateway shut down cleanly (exit code 0) without this control plane asking"
            ),
            Self::Code(code) => write!(f, "the gateway failed with exit code {code}"),
            #[cfg(unix)]
            Self::Stopped(signal) => write!(
                f,
                "the gateway was stopped from outside by signal {signal} ({})",
                termination_name(signal)
            ),
            #[cfg(windows)]
            Self::Stopped(status) => write!(
                f,
                "the gateway was stopped from outside with status {status:#010X} ({})",
                termination_name(status)
            ),
            #[cfg(unix)]
            Self::Crashed(signal) => write!(
                f,
                "the gateway died on signal {signal} ({})",
                termination_name(signal)
            ),
            #[cfg(windows)]
            Self::Crashed(status) => write!(
                f,
                "the gateway died with status {status:#010X} ({})",
                termination_name(status)
            ),
        }
    }
}

async fn spawn_gateway(binary: &Path, dir: &Path) -> anyhow::Result<RunningGateway> {
    let log_path = dir.join("gateway.log");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    writeln!(log, "\n--- gateway launch ---")?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(binary);
    command
        .arg("serve-embedded")
        .arg("--instance-dir")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    // A console of its own, which nobody sees. Sharing this one would hand every
    // worker the Ctrl+C typed into an editor this TUI runs, and the close event of
    // the window — each a stop the TUI did not ask for. Closing the window still
    // stops them, by ending this process and so closing their stdin.
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start gateway for {}", dir.display()))?;
    let stdout = child.stdout.take().context("gateway stdout was not piped")?;
    let mut stdout = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    let bytes = tokio::time::timeout(HANDSHAKE_TIMEOUT, stdout.read_line(&mut line))
        .await
        .context("gateway did not print its handshake within 20 seconds")??;
    anyhow::ensure!(bytes != 0, "gateway exited before printing its handshake; see {}", log_path.display());
    let handshake: Handshake = serde_json::from_str(line.trim_end())
        .with_context(|| format!("gateway printed a malformed handshake: {line:?}"))?;
    transport::check_endpoint(dir, &handshake.endpoint)?;
    anyhow::ensure!(!handshake.token.is_empty(), "gateway returned an empty token");
    Ok(RunningGateway {
        child,
        endpoint: handshake.endpoint,
        token: handshake.token,
    })
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

async fn stop_gateway(mut gateway: RunningGateway) {
    drop(gateway.child.stdin.take());
    if tokio::time::timeout(STOP_GRACE, gateway.child.wait()).await.is_err() {
        let _ = gateway.child.start_kill();
        let _ = gateway.child.wait().await;
    }
}

fn valid_instance_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "the name is empty");
    anyhow::ensure!(name.len() <= 63, "the name is longer than one DNS label");
    anyhow::ensure!(
        name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "use lowercase ASCII letters, digits, and hyphens only"
    );
    anyhow::ensure!(!name.starts_with('-') && !name.ends_with('-'), "the name may not start or end with '-'");
    Ok(())
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    transport::make_private(path)
}

fn bootstrap_config(dir: &Path) -> anyhow::Result<()> {
    let path = dir.join("remotex.toml");
    if path.exists() {
        return Ok(());
    }
    let temporary = dir.join(format!("remotex.toml.{}.new", std::process::id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // On Windows the file takes the directory's owner-only DACL by inheritance.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("cannot create {}", temporary.display()))?;
        file.write_all(INSTANCE_TEMPLATE.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("cannot install {}", path.display()))
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

#[derive(Clone, Default)]
pub struct RouteTable {
    inner: Arc<RwLock<BTreeMap<String, PublishedInstance>>>,
}

#[derive(Clone)]
struct PublishedInstance {
    status: InstanceStatus,
    target: Option<RouteTarget>,
}

#[derive(Clone)]
struct RouteTarget {
    endpoint: String,
    token: String,
}

/// The one loopback port shared by the landing page and every instance origin.
pub struct SharedPort {
    port: u16,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SharedPort {
    /// Take both loopbacks at `port`, under `serve`'s binding policy.
    ///
    /// The port is the caller's and is never asked of the kernel: `remotex.localhost`
    /// and every instance subdomain are typed into a browser, and a port the operator
    /// did not choose is one nobody can type. That is why there is no ephemeral path
    /// here even for tests — a test picks a concrete free port the way `serve`'s do,
    /// so the code under test is the code that ships.
    ///
    /// [`crate::server::bind_all`] is the same all-or-nothing it applies to
    /// `localhost:52380`, and for the same reason one level up: the browser picks the
    /// family, so a master left holding `[::1]:port` from an earlier run would keep
    /// answering and keep routing to *its* instance workers, and which one a page
    /// reached would be the resolver's choice.
    pub async fn bind(port: u16, routes: RouteTable) -> anyhow::Result<Self> {
        anyhow::ensure!(port != 0, "the control plane needs a port a browser can be told");
        let addrs = [
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        ];
        let mut tasks = Vec::new();
        for listener in crate::server::bind_all(&addrs, &format!("{MASTER_HOST}:{port}"))? {
            listener
                .set_nonblocking(true)
                .context("cannot make a control-plane listener non-blocking")?;
            let listener = tokio::net::TcpListener::from_std(listener)
                .context("cannot hand the control-plane listener to the runtime")?;
            let routes = routes.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let Ok((stream, _peer)) = listener.accept().await else {
                        break;
                    };
                    let routes = routes.clone();
                    tokio::spawn(async move {
                        let _ = route_connection(stream, routes, port).await;
                    });
                }
            }));
        }
        Ok(Self { port, tasks })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for SharedPort {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn route_connection(
    mut client: tokio::net::TcpStream,
    routes: RouteTable,
    public_port: u16,
) -> anyhow::Result<()> {
    client.set_nodelay(true)?;
    let request = read_request_head(&mut client).await?;
    let Some(request) = request else {
        return Ok(());
    };
    let parsed = parse_request(&request)?;
    let host = hostname(&parsed.host);

    if host == MASTER_HOST || matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        let snapshot = routes.inner.read().await.clone();
        let body = landing_page(public_port, &snapshot);
        write_response(&mut client, "200 OK", "text/html; charset=utf-8", &body, &[]).await?;
        return Ok(());
    }

    let Some(name) = host.strip_suffix(&format!(".{MASTER_HOST}")) else {
        write_response(&mut client, "404 Not Found", "text/plain; charset=utf-8", "unknown remotex host\n", &[]).await?;
        return Ok(());
    };
    if valid_instance_name(name).is_err() {
        write_response(&mut client, "404 Not Found", "text/plain; charset=utf-8", "unknown remotex instance\n", &[]).await?;
        return Ok(());
    }
    let published = routes.inner.read().await.get(name).cloned();
    let Some(published) = published else {
        write_response(&mut client, "404 Not Found", "text/plain; charset=utf-8", "unknown remotex instance\n", &[]).await?;
        return Ok(());
    };
    let Some(target) = published.target else {
        let body = format!(
            "instance {name} is {}; start it from the remotex TUI\n",
            published.status.label()
        );
        write_response(&mut client, "503 Service Unavailable", "text/plain; charset=utf-8", &body, &[]).await?;
        return Ok(());
    };

    // Whoever asks is handed the token, and that is the threat model rather than a
    // gap in it: the boundary this control plane draws is the machine, not the
    // user. The listener is bound to `127.0.0.1` and `::1` alone, so nothing off
    // the machine can ask; the token stands in for a login the embedded gateway
    // does not have, and seeding it here is what lets one page load carry it to
    // `/api/*` and to every WebSocket upgrade, which a header cannot reach from
    // inside a document. What follows is that **any local user may drive any
    // instance** — including one who could not open the worker's owner-only
    // endpoint directly. This is a single-user desktop tool: do not run
    // `remotex tui` on a machine you share with people you would not give the
    // desktops to. A launch nonce would not change that, only make it a step
    // longer: the page it authenticates has to keep something the next request
    // presents, and the redirect is where that something is handed over.
    if !cookie_has_token(parsed.cookie.as_deref(), &target.token) {
        let cookie = format!(
            "remotex_session={}; HttpOnly; SameSite=Strict; Path=/",
            target.token
        );
        write_response(
            &mut client,
            "307 Temporary Redirect",
            "text/plain; charset=utf-8",
            "",
            &[("Location", parsed.target.as_str()), ("Set-Cookie", cookie.as_str())],
        )
        .await?;
        return Ok(());
    }

    let mut gateway = match transport::connect(&target.endpoint).await {
        Ok(stream) => stream,
        Err(error) => {
            write_response(
                &mut client,
                "502 Bad Gateway",
                "text/plain; charset=utf-8",
                &format!("instance gateway is unavailable: {error}\n"),
                &[],
            )
            .await?;
            return Ok(());
        }
    };
    gateway.write_all(&request).await?;
    tokio::io::copy_bidirectional(&mut client, &mut gateway).await?;
    Ok(())
}

struct ParsedRequest {
    host: String,
    target: String,
    cookie: Option<String>,
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Option<Vec<u8>>> {
    tokio::time::timeout(REQUEST_HEAD_TIMEOUT, async {
        let mut request = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok((!request.is_empty()).then_some(request));
            }
            request.extend_from_slice(&chunk[..read]);
            anyhow::ensure!(request.len() <= MAX_REQUEST_HEAD, "request headers exceed 64 KiB");
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return Ok(Some(request));
            }
        }
    })
    .await
    .context("request headers did not arrive within 10 seconds")?
}

fn parse_request(request: &[u8]) -> anyhow::Result<ParsedRequest> {
    let end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("incomplete HTTP request headers")?;
    let head = std::str::from_utf8(&request[..end]).context("HTTP headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("missing HTTP request line")?;
    let mut pieces = request_line.split_whitespace();
    let _method = pieces.next().context("missing HTTP method")?;
    let target = pieces.next().context("missing HTTP request target")?.to_owned();
    let version = pieces.next().context("missing HTTP version")?;
    anyhow::ensure!(version.starts_with("HTTP/"), "invalid HTTP version");
    anyhow::ensure!(target.starts_with('/'), "only origin-form HTTP requests are accepted");

    let mut host = None;
    let mut cookies = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            anyhow::bail!("malformed HTTP header");
        };
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("cookie") {
            cookies.push(value.trim());
        }
    }
    Ok(ParsedRequest {
        host: host.context("request has no Host header")?,
        target,
        cookie: (!cookies.is_empty()).then(|| cookies.join("; ")),
    })
}

fn hostname(host: &str) -> String {
    let lowercase = host.trim().to_ascii_lowercase();
    if lowercase.starts_with('[') {
        return lowercase
            .find(']')
            .map_or(lowercase.clone(), |end| lowercase[..=end].to_owned());
    }
    match lowercase.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name.to_owned(),
        _ => lowercase,
    }
}

/// Whether the request already carries this instance's launch token.
///
/// The value is folded rather than compared, through the one comparison the
/// gateway itself uses on the same secret: `==` on a token stops at the first
/// wrong byte, and this runs on a loopback port where the timing is not buried
/// under a network.
fn cookie_has_token(cookie: Option<&str>, expected: &str) -> bool {
    cookie.is_some_and(|cookies| {
        cookies.split(';').any(|pair| {
            pair.trim().split_once('=').is_some_and(|(name, value)| {
                name == "remotex_session" && crate::auth::secrets_match(expected, value)
            })
        })
    })
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

fn landing_page(port: u16, instances: &BTreeMap<String, PublishedInstance>) -> String {
    let mut rows = String::new();
    for (name, instance) in instances {
        let state = instance.status.label();
        if instance.status == InstanceStatus::Running {
            rows.push_str(&format!(
                "<li><a href=\"http://{name}.{MASTER_HOST}:{port}/\">{name}</a> <span>{state}</span></li>"
            ));
        } else {
            rows.push_str(&format!("<li>{name} <span>{state}</span></li>"));
        }
    }
    if rows.is_empty() {
        rows.push_str("<li>No instances. Press <kbd>n</kbd> in the TUI to create one.</li>");
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"2\"><meta name=\"viewport\" content=\"width=device-width\"><title>remotex instances</title><style>body{{font:16px system-ui;max-width:720px;margin:4rem auto;padding:0 1rem;background:#101014;color:#eee}}a{{color:#85b7ff}}span{{color:#999;margin-left:.5rem}}li{{margin:.8rem 0}}</style></head><body><h1>remotex instances</h1><p>Start and stop gateways in the terminal control plane.</p><ul>{rows}</ul></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tick is not a reason to touch the terminal. A repaint that changes
    /// nothing still erases the display, and a selection made over this screen
    /// does not survive that.
    #[test]
    fn an_unchanged_frame_is_not_written_again() {
        let mut screen = Screen::default();
        let mut out = Vec::new();

        screen.draw(b"first".to_vec(), &mut out).unwrap();
        assert_eq!(out, b"first");

        for _ in 0..4 {
            screen.draw(b"first".to_vec(), &mut out).unwrap();
        }
        assert_eq!(out, b"first", "four idle ticks wrote nothing");

        screen.draw(b"second".to_vec(), &mut out).unwrap();
        assert_eq!(out, b"firstsecond", "a frame that differs is written");

        // Something else — the editor — has had the screen since the last frame.
        screen.invalidate();
        screen.draw(b"second".to_vec(), &mut out).unwrap();
        assert_eq!(out, b"firstsecondsecond");
    }

    /// The specs page answers from the config the instance would start on, and
    /// what it says about credentials is never the credentials.
    #[test]
    fn the_specs_page_describes_an_instance_without_printing_its_passwords() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("work");
        std::fs::create_dir(&dir).unwrap();
        let instance = InstanceInfo {
            name: "work".to_owned(),
            status: InstanceStatus::Running,
            dir,
            detail: None,
        };
        std::fs::write(
            instance.config_path(),
            "[branding]\ntext = \"work laptop\"\n\n\
             [[targets]]\nname = \"win\"\nprotocol = \"rdp\"\nhost = \"192.168.1.20\"\n\
             username = \"andrew\"\npassword = \"hunter2\"\n\
             video_quality = 70\n\n\
             [[targets]]\nname = \"desk\"\nprotocol = \"vnc\"\nsubtype = \"wlshare\"\n\
             host = \"192.168.1.21\"\n",
        )
        .unwrap();

        let page = describe_instance(&instance, 52380).join("\n");
        assert!(page.contains(&spec("state", "running")), "{page}");
        assert!(page.contains("http://work.remotex.localhost:52380"), "{page}");
        assert!(page.contains(&spec("shown as", "work laptop")), "{page}");
        assert!(page.contains("target win"), "{page}");
        assert!(page.contains("192.168.1.20:3389"), "the standard port is filled in: {page}");
        assert!(page.contains("account password set"), "{page}");
        assert!(!page.contains("hunter2"), "a password does not reach the screen: {page}");
        assert!(
            page.contains("opus ≤96 kbit/s, adaptive down to 32 kbit/s"),
            "an unset dial is named at its defaults, walk included: {page}"
        );
        assert!(page.contains("video q70"), "the render plan describes itself: {page}");
        assert!(
            page.contains(&spec(
                "picker",
                "offers resize, sound, the host's graphics pipeline passed through"
            )),
            "what a session's starter chooses is the type's to offer: {page}"
        );
        assert!(
            page.lines().any(|line| line == spec("picker", "offers resize, sound")),
            "a wlshare target's picture is not a choice: {page}"
        );

        // A config the gateway would refuse says so, instead of a page of
        // defaults for a start that will not happen.
        std::fs::write(instance.config_path(), "[server]\n").unwrap();
        let page = describe_instance(&instance, 52380).join("\n");
        assert!(page.contains("will not start"), "{page}");
        assert!(page.contains("[server]"), "it names what is wrong: {page}");
    }

    /// A clean exit this manager did not request is not a failure, and the
    /// account of it says which it was, so a `pkill` aimed at some other remotex
    /// is not read as a crash.
    #[cfg(unix)]
    #[test]
    fn an_unrequested_exit_is_only_a_failure_when_the_gateway_says_so() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = |raw: i32| std::process::ExitStatus::from_raw(raw);

        let clean = ExitKind::of(status(0));
        assert_eq!(clean, ExitKind::Clean);
        assert!(clean.is_clean());
        assert!(clean.to_string().contains("exit code 0"), "{clean}");

        let code = ExitKind::of(status(2 << 8));
        assert_eq!(code, ExitKind::Code(2));
        assert!(!code.is_clean());
        assert!(code.to_string().contains("exit code 2"), "{code}");

        let term = ExitKind::of(status(libc::SIGTERM));
        assert_eq!(term, ExitKind::Stopped(libc::SIGTERM));
        assert!(term.is_clean());
        assert!(term.to_string().contains("SIGTERM"), "{term}");
        assert!(ExitKind::of(status(libc::SIGINT)).is_clean());
        assert!(ExitKind::of(status(libc::SIGHUP)).is_clean());

        let segv = ExitKind::of(status(libc::SIGSEGV));
        assert_eq!(segv, ExitKind::Crashed(libc::SIGSEGV));
        assert!(!segv.is_clean());
        assert!(segv.to_string().contains("SIGSEGV"), "{segv}");
        assert!(!ExitKind::of(status(libc::SIGKILL)).is_clean());
    }

    /// Windows says all of it with the exit code: a status the system ended the
    /// process with is a crash however it is spelled, and a code the gateway chose
    /// is that code — `TerminateProcess` included, which names one.
    #[cfg(windows)]
    #[test]
    fn an_unrequested_exit_is_only_a_failure_when_the_gateway_says_so() {
        use std::os::windows::process::ExitStatusExt as _;
        let status = |raw: u32| std::process::ExitStatus::from_raw(raw);

        let clean = ExitKind::of(status(0));
        assert_eq!(clean, ExitKind::Clean);
        assert!(clean.is_clean());
        assert!(clean.to_string().contains("exit code 0"), "{clean}");

        let code = ExitKind::of(status(2));
        assert_eq!(code, ExitKind::Code(2));
        assert!(!code.is_clean());
        assert!(code.to_string().contains("exit code 2"), "{code}");
        assert_eq!(ExitKind::of(status(1)), ExitKind::Code(1), "what TerminateProcess leaves");

        let interrupted = ExitKind::of(status(STATUS_CONTROL_C_EXIT));
        assert_eq!(interrupted, ExitKind::Stopped(STATUS_CONTROL_C_EXIT));
        assert!(interrupted.is_clean());
        assert!(interrupted.to_string().contains("0xC000013A"), "{interrupted}");

        let violation = ExitKind::of(status(0xC000_0005));
        assert_eq!(violation, ExitKind::Crashed(0xC000_0005));
        assert!(!violation.is_clean());
        assert!(violation.to_string().contains("STATUS_ACCESS_VIOLATION"), "{violation}");
        let abort = ExitKind::of(status(0xC000_0409));
        assert!(!abort.is_clean());
        assert!(abort.to_string().contains("abort"), "a panic's end is named: {abort}");
    }

    #[test]
    fn instance_names_are_exactly_one_lowercase_dns_label() {
        for valid in ["one", "work-2", "a"] {
            valid_instance_name(valid).unwrap();
        }
        for invalid in ["", "UPPER", "two.words", "-first", "last-", "has space"] {
            assert!(valid_instance_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn request_parsing_separates_host_target_and_cookie() {
        let parsed = parse_request(
            b"GET /api/targets?q=1 HTTP/1.1\r\nHost: Work.remotex.localhost:52380\r\nCookie: other=1; remotex_session=secret\r\n\r\n",
        )
        .unwrap();
        assert_eq!(hostname(&parsed.host), "work.remotex.localhost");
        assert_eq!(parsed.target, "/api/targets?q=1");
        assert!(cookie_has_token(parsed.cookie.as_deref(), "secret"));
        assert!(!cookie_has_token(parsed.cookie.as_deref(), "other"));
    }

    #[test]
    fn a_new_instance_gets_the_embedded_config_shape() {
        let temp = tempfile::tempdir().unwrap();
        bootstrap_config(temp.path()).unwrap();
        let text = std::fs::read_to_string(temp.path().join("remotex.toml")).unwrap();
        assert!(!text.lines().any(|line| line.trim() == "[server]"));
        super::super::check(&text).unwrap();
    }

    /// A path cmd would read as variables — `%TEMP%`, `!x!` — or as operators
    /// reaches the editor exactly as it is.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_editor_gets_the_path_exactly_as_it_is() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("%TEMP% & !PATH! ^ (x)");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("remotex.toml");
        std::fs::write(&path, "the-right-file").unwrap();

        let output = editor_command("type", &path).output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stdout.contains("the-right-file"), "{stdout}");
    }

    /// An instance something else serves — another control plane on the same
    /// directory, or a worker started by hand — is refused before any worker is
    /// spawned, and says why.
    #[tokio::test(start_paused = true)]
    async fn an_instance_another_gateway_holds_is_refused_before_spawning() {
        let root = tempfile::tempdir().unwrap();
        let mut supervisor = Supervisor::open(root.path().to_path_buf(), PathBuf::from("no-such-remotex"))
            .await
            .unwrap();
        supervisor.create("alpha").await.unwrap();
        let _held = super::super::Instance::new(root.path().join("alpha")).claim().await.unwrap();

        let error = supervisor.start("alpha").await.unwrap_err();
        assert!(format!("{error:#}").contains("already served by another gateway"), "{error:#}");
        assert_eq!(supervisor.instances()[0].status, InstanceStatus::Stopped);
    }

    #[tokio::test]
    async fn two_subdomains_share_one_port_and_reach_different_gateways() {
        let routes = RouteTable::default();
        let first = fake_gateway("one").await;
        let second = fake_gateway("two").await;
        routes.inner.write().await.extend([
            (
                "one".to_owned(),
                PublishedInstance {
                    status: InstanceStatus::Running,
                    target: Some(RouteTarget {
                        endpoint: first.0.clone(),
                        token: "token-one".to_owned(),
                    }),
                },
            ),
            (
                "two".to_owned(),
                PublishedInstance {
                    status: InstanceStatus::Running,
                    target: Some(RouteTarget {
                        endpoint: second.0.clone(),
                        token: "token-two".to_owned(),
                    }),
                },
            ),
        ]);
        let router = SharedPort::bind(free_port(), routes).await.unwrap();

        let first_response = request(
            router.port(),
            "one.remotex.localhost",
            Some("remotex_session=token-one"),
        )
        .await;
        let second_response = request(
            router.port(),
            "two.remotex.localhost",
            Some("remotex_session=token-two"),
        )
        .await;
        assert!(first_response.ends_with("one"), "{first_response}");
        assert!(second_response.ends_with("two"), "{second_response}");
        first.1.await.unwrap();
        second.1.await.unwrap();
    }

    #[tokio::test]
    async fn the_router_seeds_the_launch_cookie_before_proxying() {
        let routes = RouteTable::default();
        routes.inner.write().await.insert(
            "one".to_owned(),
            PublishedInstance {
                status: InstanceStatus::Running,
                target: Some(RouteTarget {
                    endpoint: transport::endpoint(Path::new("/no/such/instance")),
                    token: "launch-token".to_owned(),
                }),
            },
        );
        let router = SharedPort::bind(free_port(), routes).await.unwrap();
        let response = request(router.port(), "one.remotex.localhost", None).await;
        assert!(response.starts_with("HTTP/1.1 307 Temporary Redirect"), "{response}");
        assert!(response.contains("Set-Cookie: remotex_session=launch-token;"), "{response}");
        assert!(response.contains("Location: /"), "{response}");
    }

    /// A port nothing is listening on, found by taking one and letting it go —
    /// `free_port` in `src/server.rs`, for its reason. A test needs a port the rest
    /// of the machine is not using; the control plane needs a port its operator
    /// chose, and asking the kernel for one is not a thing it may do.
    fn free_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A master left holding one family of the shared port is not something to
    /// start beside: the browser picks the family, so half its page loads would
    /// reach the old process and its old instance workers.
    #[tokio::test]
    async fn a_port_half_taken_by_an_earlier_master_refuses_the_start() {
        let port = free_port();
        // The earlier master is simulated by holding one family, so a machine with
        // IPv6 off has no half to take: `bind_all` skips `::1` there and the start
        // it would refuse is one that legitimately succeeds. Any other bind error is
        // still the test failing.
        let squatter = match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => return,
            Err(error) => panic!("cannot hold the v6 half of {port}: {error}"),
        };

        let Err(error) = SharedPort::bind(port, RouteTable::default()).await else {
            panic!("the v6 half of this port is already served");
        };
        let text = format!("{error:#}");
        assert!(text.contains("already in use"), "{text}");
        assert!(text.contains(&port.to_string()), "it must name the port: {text}");

        // And the refusal took nothing: the v4 half it bound on the way through is
        // released, so a retry after stopping the other master works.
        drop(squatter);
        SharedPort::bind(port, RouteTable::default())
            .await
            .expect("the refused attempt must not have kept a socket");
    }

    /// The port is typed into a browser, so there is no code path that lets the
    /// kernel choose it — including the one the tests take.
    #[tokio::test]
    async fn an_ephemeral_port_is_not_something_the_control_plane_can_be_asked_for() {
        let Err(error) = SharedPort::bind(0, RouteTable::default()).await else {
            panic!("port 0 is a control plane nobody can be told how to reach");
        };
        assert!(format!("{error:#}").contains("a browser can be told"), "{error:#}");
    }

    /// A worker's endpoint answering one request with `body`, over the transport
    /// a real worker listens on.
    async fn fake_gateway(
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = transport::endpoint(directory.path());
        let claim = super::super::Instance::new(directory.path()).claim().await.unwrap();
        let mut listener = transport::WorkerListener::bind(&endpoint, &claim).unwrap();
        let task = tokio::spawn(async move {
            let _claim = claim;
            let (mut stream, _) = axum::serve::Listener::accept(&mut listener).await;
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (endpoint, task, directory)
    }

    async fn request(port: u16, host: &str, cookie: Option<&str>) -> String {
        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let cookie = cookie.map_or(String::new(), |cookie| format!("Cookie: {cookie}\r\n"));
        stream
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\n{cookie}Connection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }
}
