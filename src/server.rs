use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use log::warn;
use serde::{Deserialize, Serialize};
use tower::service_fn;
use tower_http::services::ServeDir;

use crate::{
    auth::{self, AuthSessions, GatewayAuth},
    config::AppConfig,
    error::{ApiResult, AppError},
    session::SessionManager,
    ws,
};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    /// The single session slot: claim here, attach over `/ws`.
    pub sessions: Arc<SessionManager>,
    /// Live auth sessions behind the login cookie. Only a
    /// [`GatewayAuth::Login`] gateway mints or validates one — an embedded
    /// gateway's client carries the launch token in that same cookie and there is
    /// no session to look up, so this stays empty there.
    pub auth: Arc<AuthSessions>,
}

/// A [`tokio::net::TcpListener`] whose accepted sockets have `TCP_NODELAY` set.
///
/// Every socket accepted here feeds an ack-gated window — tiles wait on `paintAck`, and a
/// segment Nagle holds back is that window stalled for a round trip on a link that was never
/// the problem. guacd sets the same flag on every accepted connection (`guacd/daemon.c`),
/// naming Nagle as the reason; the VNC-to-host socket here already does, and FreeRDP sets it
/// on its own transport. This closes the one hop that was left at the OS default.
///
/// A newtype rather than a `set_nodelay` at each accept site because there is no accept site
/// in this codebase: `axum::serve` owns the loop, and this is the seam it offers. The inner
/// listener's own [`axum::serve::Listener`] impl is what is delegated to, so its handling of
/// transient accept errors is kept rather than reimplemented.
pub struct NodelayListener(pub tokio::net::TcpListener);

impl axum::serve::Listener for NodelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr) = axum::serve::Listener::accept(&mut self.0).await;
        // Refused only by a socket that is already dying, whose next read will say
        // something better than a setsockopt errno — so noted, not fatal.
        if let Err(e) = stream.set_nodelay(true) {
            warn!("cannot set TCP_NODELAY for {addr}: {e}");
        }
        (stream, addr)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Build the axum router.
///
/// - `/api/auth/*` + `/api/health` — public: the login flow itself and the
///   liveness probe. On an embedded gateway `login` and `logout` answer 403
///   instead (see `no_login_handler`), while `status` stays real — the same SPA
///   runs there and asks it first.
/// - the rest of `/api/*`, `/ws`, and `/ws/audio` — refuse requests that do not
///   carry whatever this gateway's [`GatewayAuth`] asks for; unknown `/api/*`
///   paths return 404 rather than the SPA, so API clients get an honest error.
/// - `/ws` — the remote-desktop control and picture WebSocket.
/// - `/ws/audio` — the dedicated remote-audio WebSocket.
/// - fallback — the built SPA, served from `config.static_dir` on disk. Real
///   files are served by [`ServeDir`]; any unknown path returns `index.html`
///   with a 200 so client-side routes resolve (matching an SPA's expectations).
///   The static shell stays public — it renders the login screen and holds no
///   secrets; everything it talks to is behind the cookie. An embedded gateway
///   serves the same SPA out of `remotex.app`'s bundle.
pub fn router(config: AppConfig) -> Router {
    let sessions = Arc::new(SessionManager::new(config.targets.clone()));
    router_with_sessions(config, sessions)
}

/// [`router`] over a caller-supplied session slot.
///
/// The seam exists for one thing: the manual audio harness
/// ([`tests::serve_a_test_tone`]) needs the real router — SPA, login, and `/ws` — in
/// front of a scripted engine rather than a real RDP connect.
pub(crate) fn router_with_sessions(
    config: AppConfig,
    sessions: Arc<SessionManager>,
) -> Router {
    // Use `.fallback` (returns the fallback response as-is) rather than
    // `.not_found_service` (which forces a 404 status), so SPA routes get 200.
    let spa = {
        let static_dir = config.static_dir.clone();
        let index_path = static_dir.join("index.html");
        let spa_index = service_fn(move |_req| {
            let index_path = index_path.clone();
            async move {
                let response = match tokio::fs::read(&index_path).await {
                    Ok(bytes) => {
                        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes)
                            .into_response()
                    }
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                };
                Ok::<_, Infallible>(response)
            }
        });
        ServeDir::new(&static_dir).fallback(spa_index)
    };

    // Two shapes of the same three routes, and which one is registered is decided
    // here rather than inside the handlers. An embedded gateway *has* no login —
    // its client was given a token before it made its first request — so the
    // honest answer to one is a refusal, and a refusal is easier to read at the
    // router than a handler that begins by asking what kind of gateway it is on.
    //
    // `status` is the exception, and registering it either way is the point: the
    // page asks the same question on both, and there it answers yes because the
    // app already put the launch token in the cookie store.
    let auth_routes = match config.auth {
        GatewayAuth::Login(_) => Router::new()
            .route("/auth/login", post(login_handler))
            .route("/auth/logout", post(logout_handler)),
        GatewayAuth::Token(_) => Router::new()
            .route("/auth/login", post(no_login_handler))
            .route("/auth/logout", post(no_login_handler)),
    }
    .route("/auth/status", get(status_handler));

    let state = AppState {
        config,
        sessions,
        auth: Arc::new(AuthSessions::default()),
    };
    let require_auth = middleware::from_fn_with_state(state.clone(), require_auth);

    // Nested so unmatched `/api/*` paths hit this router's 404 fallback instead
    // of falling through to the SPA index.
    let api = Router::new()
        .route("/health", get(|| async { "ok" }))
        // Public: the login screen reads its branding before authenticating.
        .route("/config", get(config_handler))
        // Public for the same reason: the tab's icon is set the moment the page
        // loads, which is before anybody has typed a password.
        .route("/logo", get(logo_handler))
        .merge(auth_routes)
        .merge(
            Router::new()
                .route("/targets", get(targets_handler))
                .route("/session", post(claim_handler))
                .route_layer(require_auth.clone()),
        )
        .fallback(|| async { AppError::NotFound });

    let routed = Router::new()
        .nest("/api", api)
        // The auth check runs before the upgrade, so an unauthenticated
        // WebSocket attempt fails its handshake with a plain 401. (A sub-router
        // because route_layer must come after a route to apply to it.)
        .merge(
            Router::new()
                .route("/ws", any(ws::handler))
                // Sound, on a socket of its own so it never queues behind a picture.
                // Same guard, same credential kinds; only the payload differs.
                .route("/ws/audio", any(ws::audio_handler))
                .route_layer(require_auth),
        );

    routed
        .fallback_service(spa)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            shell_origin_cors,
        ))
        // Added last and therefore **outermost**: it sees every request before the
        // CORS layer and before routing, because what it acts on is the `Host` a
        // browser arrived under rather than which handler would answer. Inert unless
        // `[server].dev_subdomain` is set *and* that host is loopback — and the two
        // never meet, because `ConfigFile::resolve_embedded` leaves `dev_hostname`
        // `None` on the only gateway that answers the shell origin. That is what
        // keeps a 307 off an `OPTIONS`: a redirected preflight never completes, and
        // the request it was clearing then never happens.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            dev_hostname_redirect,
        ))
        .with_state(state)
}

/// The one origin an embedded gateway answers cross-origin: the shell's own.
///
/// A real origin, not the opaque `null` a `file://` document sends. The app registers
/// `remotex` as a standard, secure, CORS-enabled scheme and serves the SPA out of its
/// own bundle at `remotex://app/index.html`, so the
/// page has one origin that holds still across launches — which is what keeps the
/// client's remembered preferences, and which an ephemeral loopback port never could.
///
/// So this is not a wildcard standing in for "anything". It is the literal origin of
/// the one page this gateway exists to serve.
const SHELL_ORIGIN: &str = "remotex://app";

/// Let the bundled `remotex://` client talk to its own gateway.
///
/// Two headers and a preflight, and the pair of them is the whole mechanism:
/// `Access-Control-Allow-Origin: remotex://app` names the caller, and
/// `Access-Control-Allow-Credentials: true` is what lets the `remotex_session`
/// cookie travel — without the second one the request succeeds and arrives
/// *unauthenticated*, which is the confusing half of getting this wrong.
///
/// Deliberately narrow in four ways:
///
/// - **Only when [`AppConfig::allow_shell_origin`] is set**, which is only an
///   embedded gateway. A served one has no business answering for a scheme no
///   browser on a network can be; see that field.
/// - **Only on a [`GatewayAuth::Token`] gateway**, and this is not the same
///   condition said twice. The credential is what the second header lets travel, so
///   what makes this safe is that it is a token minted for one launch and given to
///   one page — not a login cookie a person typed a password for, which is what
///   would be at stake if the two fields ever came apart. Checked here rather than
///   trusted from [`crate::config::ConfigFile::resolve_embedded`], because a
///   middleware relying on a distant constructor to hold an invariant it depends on
///   is one refactor from not holding it.
/// - **Only for `Origin: remotex://app`.** Every other origin is either same-origin
///   (a browser opened on this gateway's own address, which needs no header at all)
///   or something this gateway has no business answering. Echoing back whatever
///   arrived would turn one allowed caller into all of them — and `null`, which this
///   used to answer, is the origin of every sandboxed frame and `data:` URL on the
///   web rather than of one client.
/// - **`Vary: Origin`**, so nothing caches an answer made for one origin and
///   serves it to another.
async fn shell_origin_cors(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let embedded = state.config.allow_shell_origin
        && matches!(state.config.auth, GatewayAuth::Token(_));
    if !embedded {
        return next.run(req).await;
    }
    let from_shell = req
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|value| value.as_bytes() == SHELL_ORIGIN.as_bytes());
    if !from_shell {
        return next.run(req).await;
    }

    // A preflight is answered here rather than routed: it asks what *would* be
    // allowed, and no handler downstream knows. `OPTIONS` never reaches a route.
    let mut response = if req.method() == axum::http::Method::OPTIONS {
        let mut preflight = StatusCode::NO_CONTENT.into_response();
        preflight.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        preflight.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static("content-type"),
        );
        preflight
    } else {
        next.run(req).await
    };

    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static(SHELL_ORIGIN),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        header::HeaderValue::from_static("true"),
    );
    headers.insert(header::VARY, header::HeaderValue::from_static("Origin"));
    response
}

/// Whether `name` — a `Host` header with its port and brackets already stripped —
/// can only mean this machine.
///
/// Three families, and the third is the one that matters for the redirect below:
///
/// - a loopback literal, which is all of `127.0.0.0/8` and `::1` rather than just
///   `127.0.0.1`;
/// - `localhost`;
/// - **anything under `.localhost`**, which RFC 6761 §6.3 reserves for loopback in
///   its entirety. So `gw-a.remotex.localhost`, the `gateway-1.localhost` of some
///   other tool and any other label somebody types are all this machine, and all of
///   them are names this gateway may find itself serving under.
///
/// Case-insensitive, because DNS names are and a `Host` header may arrive in any
/// case.
fn is_loopback_name(name: &str) -> bool {
    if let Ok(ip) = name.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    let name = name.to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

/// Send a loopback browser to `<label>.remotex.localhost`, so this gateway has a
/// cookie origin of its own (see `[server].dev_subdomain`).
///
/// Three deliberate limits, because a redirect is a thing to be careful with:
///
/// - **Loopback only.** A request whose `Host` is anything else — a real hostname,
///   a LAN address, a reverse proxy's name — is passed through untouched, so no
///   deployment can be redirected however the key is set. Note that widening what
///   counts as loopback cannot widen where this sends anybody: the target is built
///   from this gateway's own validated label and never from the request, so there
///   is no input that makes it point somewhere else.
/// - **The home page only**, and not merely "not the API". Opening the gateway is
///   the one request that decides which origin everything else belongs to: the
///   document that lands on `<label>.remotex.localhost` asks for its assets, its `/api`
///   and its `/ws` from there by itself. Redirecting anything else would be
///   redundant at best and harmful at worst — a `fetch` that followed a
///   cross-origin redirect would drop its credentials, and a WebSocket upgrade
///   does not follow one at all.
/// - **307, not 301.** A permanent redirect is cached by the browser hard enough
///   to outlive the config key that caused it, on a hostname somebody may want
///   back. This one is a development convenience and must be as easy to remove as
///   it was to add.
async fn dev_hostname_redirect(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(dev_hostname) = state.config.dev_hostname.as_deref() else {
        return next.run(req).await;
    };
    if req.uri().path() != "/" {
        return next.run(req).await;
    }
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return next.run(req).await;
    };
    // `host:port`, `[::1]:port`, or a bare name. The port is kept as it arrived
    // rather than read from the config: it is the one the browser can reach, which
    // behind anything at all is not necessarily the one this process bound.
    let (name, port) = match host.rsplit_once(':') {
        // A colon with no digits after it is part of an unbracketed IPv6 literal,
        // not a port — so `::1` does not become the host `:` on port `1`.
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (name, Some(port))
        }
        _ => (host, None),
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    // Any loopback name that is not *this* gateway's own, which is stricter than it
    // first looks and deliberately so. `gw-b.remotex.localhost` on gateway A's port is a
    // browser about to give gateway A a cookie under gateway B's name — the exact
    // collision this whole mechanism exists to prevent, arrived at by editing the
    // port in the URL bar and not the label. So the test is not "did you come in on
    // a bare loopback address" but "are you already where you belong".
    //
    // Also the loop guard: without the second half this would redirect its own
    // target forever.
    if !is_loopback_name(name) || name.eq_ignore_ascii_case(dev_hostname) {
        return next.run(req).await;
    }

    let authority = match port {
        Some(port) => format!("{dev_hostname}:{port}"),
        None => dev_hostname.to_owned(),
    };
    let target = format!(
        "http://{authority}{}",
        req.uri()
            .path_and_query()
            .map_or("/", |path_and_query| path_and_query.as_str())
    );
    match header::HeaderValue::from_str(&target) {
        Ok(location) => {
            (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, location)]).into_response()
        }
        // Unreachable with a validated label and a `Host` that parsed, and a
        // redirect nobody can follow is worse than none.
        Err(e) => {
            warn!("server: cannot redirect to {target:?}: {e}");
            next.run(req).await
        }
    }
}

/// Middleware guarding everything session-related: whatever this gateway's
/// [`GatewayAuth`] asks for, or no service.
///
/// Both modes read the same `remotex_session` cookie and differ only in what
/// makes it valid — a login gateway looks the value up in [`AuthSessions`], an
/// embedded one compares it against the token it minted at startup. One carrier
/// for both, because a cookie is the only credential a *document* can carry: the
/// SPA's `fetch` calls and its `ws://` upgrades are issued by the page, not by
/// the client that opened it, and neither can be given a header.
async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let authenticated = authenticate(&state, req.headers());
    if !authenticated {
        return AppError::Unauthorized.into_response();
    }
    next.run(req).await
}

/// Whether these headers carry this gateway's credential. Shared by
/// [`require_auth`] and [`status_handler`] so the guard and the answer about the
/// guard cannot drift apart.
fn authenticate(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(presented) = auth::token_from_headers(headers) else {
        return false;
    };
    match &state.config.auth {
        // Validation also refreshes the session's sliding expiry.
        GatewayAuth::Login(_) => state.auth.validate(&presented),
        GatewayAuth::Token(expected) => expected.matches(&presented),
    }
}

/// `Set-Cookie` attributes for the session cookie. `Secure` cookies set over
/// plain HTTP are silently dropped by Safari (even on localhost, unlike
/// Chrome), so the flag is only added when the request actually arrived over
/// HTTPS — which, since this server only speaks HTTP, means via a
/// TLS-terminating proxy setting `x-forwarded-proto`.
fn cookie_flags(headers: &HeaderMap) -> &'static str {
    let https = headers
        .get("x-forwarded-proto")
        .is_some_and(|proto| proto.as_bytes() == b"https");
    if https {
        "HttpOnly; SameSite=Strict; Path=/; Secure"
    } else {
        "HttpOnly; SameSite=Strict; Path=/"
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
}

/// Verify the credentials against `[server].site_passwd` and set the session
/// cookie. 401 on a mismatch, with no hint which of the two fields was wrong.
async fn login_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let GatewayAuth::Login(site_passwd) = &state.config.auth else {
        // Unreachable: `router` registers `no_login_handler` at this path on a
        // token gateway. Answered rather than asserted, because the shape that
        // would reach here — a login route on a gateway with no credential — must
        // not be a panic in a running program.
        return Err(AppError::Forbidden);
    };
    let site_passwd = site_passwd.clone();
    // bcrypt verification burns tens of milliseconds by design — keep it off
    // the async workers.
    let ok = tokio::task::spawn_blocking(move || {
        site_passwd.verify(&req.username, &req.password)
    })
    .await
    .map_err(anyhow::Error::from)?;
    if !ok {
        return Err(AppError::Unauthorized);
    }
    let token = state.auth.create();
    let cookie = format!("{}={token}; {}", auth::COOKIE_NAME, cookie_flags(&headers));
    Ok(([(header::SET_COOKIE, cookie)], Json(OkResponse { ok: true })))
}

/// Invalidate the caller's login (if any), end the remote session with it, and
/// clear the cookie. Public: it only ever drops the caller's own token.
///
/// Both halves, because a login and the desktop it opened end together. Ending only
/// the login left the engine to the ordinary detach path — indistinguishable from a
/// browser that crashed, so the gateway held the target for its reattach grace and a
/// login inside that minute resumed the desktop instead of showing the picker (see
/// [`crate::session::SessionManager::log_out`]).
///
/// Server-side rather than a `disconnect` the browser sends first: one request does
/// both, so there is no ordering to get right and no dependence on the WebSocket
/// still being up — which is precisely the state a browser is in when the grace
/// period is already counting down.
async fn logout_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = auth::token_from_headers(&headers) {
        state.auth.invalidate(&token);
    }
    state.sessions.log_out();
    let cookie = format!(
        "{}=; {}; Max-Age=0",
        auth::COOKIE_NAME,
        cookie_flags(&headers)
    );
    ([(header::SET_COOKIE, cookie)], Json(OkResponse { ok: true }))
}

#[derive(Serialize)]
struct ConfigResponse {
    branding: String,
    /// Whether `GET /api/logo` has an icon to serve. A flag rather than a URL:
    /// the client already knows its gateway's origin, and a URL here would be a
    /// second spelling of it.
    logo: bool,
}

/// Public, non-secret client config. Read on load so the login screen and the
/// browser tab title carry the deployment's branding before authentication.
async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        branding: state.config.branding.text.clone(),
        logo: state.config.branding.logo.is_some(),
    })
}

/// The `[branding].logo` file, as the page's icon.
///
/// Read from disk per request rather than held in memory: the file is a favicon,
/// requested once per tab, and reading it here is what lets an operator swap the
/// image without a restart. A configured path that cannot be read answers 404 —
/// the extension was checked at config resolution, but existence is a fact about
/// disk that can change under a running gateway.
async fn logo_handler(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let logo = state.config.branding.logo.as_ref().ok_or(AppError::NotFound)?;
    let bytes = tokio::fs::read(&logo.path).await.map_err(|e| {
        warn!("cannot read [branding].logo {}: {e}", logo.path.display());
        AppError::NotFound
    })?;
    Ok(([(header::CONTENT_TYPE, logo.mime)], bytes))
}

/// The login routes on an embedded gateway: 403, always.
///
/// A refusal rather than a missing route, because the two say different things. A
/// 404 reads as "this build is older than you thought" and sends somebody looking
/// for a routing mistake; a 403 says the request was understood and is not allowed
/// here — which is the truth, and it is the same answer whatever credentials are
/// offered, since an embedded gateway holds none to check them against.
async fn no_login_handler() -> AppError {
    AppError::Forbidden
}

#[derive(Serialize)]
struct StatusResponse {
    authenticated: bool,
}

/// Whether the caller holds a live session — the SPA asks on load to decide
/// between the login screen and the desktop.
///
/// This route exists on an embedded gateway too, unlike the two beside it: the
/// same SPA runs there, asks the same question first, and the answer is yes as
/// soon as `remotex.app` has put the launch token in its window's cookie store.
/// A no there means the client and the gateway disagree about the token, which is
/// the app's problem to report — it is not something a login form could fix.
async fn status_handler(State(state): State<AppState>, headers: HeaderMap) -> Json<StatusResponse> {
    Json(StatusResponse {
        authenticated: authenticate(&state, &headers),
    })
}

#[derive(Serialize)]
struct TargetInfo {
    name: String,
    protocol: &'static str,
    /// The target's `subtype` where it has one, `null` otherwise — the same
    /// field [`crate::protocol::ServerMsg::Connected`] carries, and here for the
    /// same reason one step earlier: three entries in this list say `vnc`, and
    /// which of them is a Mac in Standard mode and which is High Performance is
    /// what somebody is choosing between.
    subtype: Option<&'static str>,
    host: String,
    port: u16,
}

/// The list of target profiles the browser may pick from the post-login picker.
/// Non-secret info only — credentials never leave the server.
async fn targets_handler(State(state): State<AppState>) -> Json<Vec<TargetInfo>> {
    let targets = state
        .config
        .targets
        .iter()
        .map(|t| TargetInfo {
            name: t.name.clone(),
            protocol: t.protocol.name(),
            subtype: t.subtype.map(crate::config::Subtype::name),
            host: t.host.clone(),
            port: t.port,
        })
        .collect();
    Json(targets)
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ClaimRequest {
    /// Take the slot even if another browser's WebSocket holds it (takeover).
    force: bool,
    /// The caller's previous token; matching the current claim lets the same
    /// browser reclaim (reconnect) without the takeover prompt.
    session_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    session_id: String,
}

/// Claim the single session slot. Returns the token the WebSocket
/// must present as `/ws?session=<token>`; 409 while another browser is
/// attached (retry with `force` to take over).
async fn claim_handler(
    State(state): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> ApiResult<Json<ClaimResponse>> {
    let session_id = state.sessions.claim(req.force, req.session_id.as_deref())?;
    Ok(Json(ClaimResponse { session_id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A router whose only interesting property is the dev hostname, over a
    /// static dir that does not exist — every assertion below is about the
    /// redirect, and a request that is *not* redirected only has to be shown not
    /// to be one.
    fn dev_router(dev_hostname: Option<&str>) -> Router {
        router(router_config(dev_hostname))
    }

    /// The config both test routers are built from, so the only thing that ever
    /// differs between them is the field under test.
    fn router_config(dev_hostname: Option<&str>) -> AppConfig {
        AppConfig {
            listen: crate::config::ListenAddr::Tcp("127.0.0.1:52675".to_owned()),
            static_dir: "frontend/dist".into(),
            // Never dialed: no test here starts a session.
            targets: vec![crate::config::TargetConfig {
                name: "unreachable".to_owned(),
                protocol: crate::config::Protocol::Vnc,
                subtype: None,
                host: "127.0.0.1".to_owned(),
                port: 9,
                username: String::new(),
                password: String::new(),
                vnc_password: String::new(),
                domain: None,
                width: Some(1280),
                height: Some(800),
                security: crate::config::Security::Auto,
                egfx: None,
                resize: false,
                clipboard: false,
                audio: false,
                audio_codec: None,
                render_type: crate::config::RenderType::Full,
                render_subtype: crate::config::RenderSubtype::Png,
                render_quality: None,
                render_motion_subtype: None,
                render_motion_quality: None,
                render_motion_debug: false,
                render_adaptive: false,
                render_adaptive_min: None,
                audio_bitrate: None,
                audio_adaptive: false,
                audio_bitrate_min: None,
            }],
            auth: crate::auth::GatewayAuth::Login(
                crate::auth::SitePasswd::parse(
                    &crate::auth::generate("admin", "hunter2", 4).unwrap(),
                )
                .unwrap(),
            ),
            branding: crate::config::Branding {
                text: "remotex".to_owned(),
                logo: None,
            },
            dev_hostname: dev_hostname.map(str::to_owned),
            allow_shell_origin: false,
        }
    }

    /// `dev_router`, but answering the shell's origin the way an embedded gateway
    /// does.
    ///
    /// Both halves of that shape, not just the flag: `resolve_embedded` mints a
    /// token and sets the flag together, so a router carrying one without the other
    /// is a gateway that cannot exist — and the tests below would then be asserting
    /// credentialed CORS on top of a *login* cookie, which is the one combination
    /// this must never produce.
    fn embedded_router() -> Router {
        let mut config = router_config(None);
        config.auth = GatewayAuth::Token(crate::auth::EmbeddedToken::generate());
        config.allow_shell_origin = true;
        router(config)
    }

    /// The response to `method path` carrying `origin` as `Origin`, or no `Origin`
    /// header at all when it is `None`.
    async fn response_for(
        router: Router,
        method: &str,
        path: &str,
        origin: Option<&str>,
    ) -> Response {
        use tower::ServiceExt as _;

        let mut request = axum::http::Request::builder().method(method).uri(path);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        router
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn header_of(response: &Response, name: header::HeaderName) -> Option<String> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    }

    /// The client is loaded from `remotex://app`, so it calls its own gateway
    /// cross-origin. Both headers matter and for different reasons: without the
    /// first the call is refused, and without the second it succeeds *without the
    /// cookie* — which surfaces as a mysterious 401 rather than as a CORS error.
    #[tokio::test]
    async fn an_embedded_gateway_answers_the_shell_origin_with_credentials() {
        let response =
            response_for(embedded_router(), "GET", "/api/health", Some(SHELL_ORIGIN)).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(),
            Some(SHELL_ORIGIN)
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).as_deref(),
            Some("true"),
            "without this the cookie does not travel and the call arrives anonymous"
        );
        assert_eq!(
            header_of(&response, header::VARY).as_deref(),
            Some("Origin"),
            "or a cache could serve this answer to a different origin"
        );
    }

    /// The preflight has to be answered here, because no route knows what would be
    /// allowed and `OPTIONS` reaches none of them.
    #[tokio::test]
    async fn a_preflight_from_the_shell_origin_is_answered() {
        let response = response_for(
            embedded_router(),
            "OPTIONS",
            "/api/session",
            Some(SHELL_ORIGIN),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN).as_deref(),
            Some(SHELL_ORIGIN)
        );
        assert!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_METHODS)
                .is_some_and(|methods| methods.contains("POST")),
            "claiming the session slot is a POST"
        );
    }

    /// The half that keeps this safe. A served gateway is reachable by browsers on
    /// a network and holds a login cookie, and nothing that reaches it can be a
    /// `remotex://` document — so these headers there could only ever be granted to
    /// something lying about where it came from.
    #[tokio::test]
    async fn a_served_gateway_never_answers_the_shell_origin() {
        let response =
            response_for(dev_router(None), "GET", "/api/health", Some(SHELL_ORIGIN)).await;

        assert_eq!(response.status(), StatusCode::OK, "the request still works");
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            None,
            "but a browser may not read it cross-origin"
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            None
        );
    }

    /// The credential is the other half of the condition. A login gateway's cookie
    /// is a password somebody typed and a session that outlives the page; the flag
    /// alone must not be enough to let the shell origin send one, however it came to
    /// be set.
    #[tokio::test]
    async fn the_flag_alone_does_not_open_a_login_gateway() {
        let mut config = router_config(None);
        config.allow_shell_origin = true;
        let response = response_for(router(config), "GET", "/api/health", Some(SHELL_ORIGIN)).await;

        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            None,
            "a login credential is never handed to the shell origin"
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            None
        );
    }

    /// Only the shell origin, and not whatever turned up. Echoing the request's own
    /// `Origin` back is how one allowed caller quietly becomes all of them — and
    /// `null` is in this list because it is what a `file://` document and every
    /// sandboxed frame on the web send, which this gateway used to answer and no
    /// longer does.
    #[tokio::test]
    async fn an_embedded_gateway_answers_no_other_origin() {
        for origin in [
            "http://evil.example",
            "http://127.0.0.1:52675",
            "null",
            "remotex://elsewhere",
        ] {
            let response =
                response_for(embedded_router(), "GET", "/api/health", Some(origin)).await;
            assert_eq!(
                header_of(&response, header::ACCESS_CONTROL_ALLOW_ORIGIN),
                None,
                "{origin} must not be granted cross-origin access"
            );
        }
    }

    /// The `Location` a `GET /` under `host` is sent to, or `None` when it was
    /// not redirected at all.
    async fn redirect_for(router: Router, host: &str, path: &str) -> Option<String> {
        use tower::ServiceExt as _;

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .header(header::HOST, host)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if response.status() != StatusCode::TEMPORARY_REDIRECT {
            return None;
        }
        Some(
            response
                .headers()
                .get(header::LOCATION)
                .expect("a redirect carries a Location")
                .to_str()
                .unwrap()
                .to_owned(),
        )
    }

    /// The hole this closes, and the reason the rule is "not already where you
    /// belong" rather than "arrived on a bare address".
    ///
    /// `gw-b.remotex.localhost:52675` is somebody who edited the port in the URL bar and
    /// not the label. Serving it would put *this* gateway's cookie under the other
    /// one's hostname, which is precisely the collision the whole mechanism exists
    /// to prevent — so it is redirected like any other name that is not ours.
    #[tokio::test]
    async fn another_gateways_hostname_on_this_port_is_redirected_here() {
        for host in [
            "gw-b.remotex.localhost:52675",
            "gateway-1.localhost:52675",
            // A sub-subdomain of our own name is still not our own name.
            "x.gw-a.remotex.localhost:52675",
            // Nor is the suffix every gateway's name hangs off: no gateway is
            // called that, and a cookie left there would be every gateway's.
            "remotex.localhost:52675",
            // The rest of 127/8 is loopback too, not just .0.1.
            "127.0.0.2:52675",
        ] {
            assert_eq!(
                redirect_for(dev_router(Some("gw-a.remotex.localhost")), host, "/").await,
                Some("http://gw-a.remotex.localhost:52675/".to_owned()),
                "{host} should have been sent to this gateway's own hostname"
            );
        }
    }

    /// The loop guard, which is now explicit: our own name is a loopback name, and
    /// redirecting it would point at itself forever. Case-insensitively, because a
    /// `Host` header may arrive in any case and DNS does not care.
    #[tokio::test]
    async fn this_gateways_own_hostname_is_never_redirected_again() {
        for host in [
            "gw-a.remotex.localhost:52675",
            "GW-A.REMOTEX.LOCALHOST:52675",
            "gw-a.remotex.localhost",
        ] {
            assert_eq!(
                redirect_for(dev_router(Some("gw-a.remotex.localhost")), host, "/").await,
                None,
                "{host} is already where it belongs"
            );
        }
    }

    /// The point of the whole thing: each gateway gets a cookie origin of its own,
    /// with the port and the loopback spelling it arrived under both preserved.
    #[tokio::test]
    async fn a_loopback_browser_is_sent_to_the_dev_hostname() {
        for host in ["127.0.0.1:52675", "localhost:52675", "[::1]:52675"] {
            assert_eq!(
                redirect_for(dev_router(Some("gw-a.remotex.localhost")), host, "/").await,
                Some("http://gw-a.remotex.localhost:52675/".to_owned()),
                "{host} should have been redirected"
            );
        }
        // No port in the Host is legal (port 80) and must not invent one.
        assert_eq!(
            redirect_for(dev_router(Some("gw-a.remotex.localhost")), "localhost", "/").await,
            Some("http://gw-a.remotex.localhost/".to_owned())
        );
    }

    /// The safety property. A deployment reaches this gateway under its own name,
    /// and must never be bounced to a loopback hostname however this is configured.
    #[tokio::test]
    async fn a_request_that_did_not_arrive_on_loopback_is_left_alone() {
        for host in [
            "remotex.example.com",
            "remotex.example.com:52675",
            "10.22.34.32:52675",
            "[fdb8:d92a::1]:52675",
            // Not loopback however much it reads like it: the suffix is what
            // RFC 6761 reserves, and this one only *contains* the word.
            "localhost.example.com:52675",
            "notlocalhost:52675",
        ] {
            assert_eq!(
                redirect_for(dev_router(Some("gw-a.remotex.localhost")), host, "/").await,
                None,
                "{host} must not be redirected"
            );
        }
    }

    /// Only the home page. Everything else belongs to whichever origin the
    /// document was loaded from, and a `fetch` that followed a cross-origin
    /// redirect would drop its cookie.
    #[tokio::test]
    async fn nothing_but_the_home_page_is_redirected() {
        for path in ["/api/health", "/api/auth/status", "/ws", "/assets/app.js"] {
            assert_eq!(
                redirect_for(dev_router(Some("gw-a.remotex.localhost")), "127.0.0.1:52675", path).await,
                None,
                "{path} must not be redirected"
            );
        }
    }

    /// And with the key unset it is inert, which is every deployment.
    #[tokio::test]
    async fn without_the_key_nothing_is_redirected() {
        assert_eq!(
            redirect_for(dev_router(None), "127.0.0.1:52675", "/").await,
            None
        );
    }

    /// Serve the real gateway with a **generated tone** in place of a remote's
    /// audio, so the browser half of the audio path can be listened to without a
    /// server that redirects.
    ///
    /// It exists because the RDP side and the browser side fail independently, and
    /// only one of them needs a Windows host. Whenever the representation changes —
    /// and it has twice, from an open-ended WAV to Ogg/Opus to bare Opus packets
    /// decoded by WebCodecs — the question is whether *browsers* play what this now
    /// sends, live and without stalling. The PCM's provenance is irrelevant to that,
    /// so this supplies it locally and the answer is unambiguous: a failure here is
    /// the format, not the remote.
    ///
    /// The tone comes and goes in five-second phases, with the format published and
    /// cleared around the gaps the way a real host's channel opening and closing
    /// does. That is deliberate: the behaviour worth checking in a browser is no
    /// longer only "does it play" but "does it *start on its own*, stop, and start
    /// again" — so enable audio during a quiet phase and then touch nothing.
    ///
    /// `#[ignore]`d and in-crate on purpose: it needs
    /// [`SessionManager::with_test_spawner`], and it must add nothing a real
    /// deployment could reach — no config key, no debug flag, no tone in the
    /// shipping product.
    ///
    /// ```sh
    /// cargo test --lib serve_a_test_tone -- --ignored --nocapture
    /// ```
    ///
    /// Then open the printed URL, log in, pick the target, and press ☰ → Enable
    /// audio. A 440 Hz tone means the whole browser-side path works, on that
    /// browser — and **this is where a codec's browser support is settled**,
    /// because there is no fallback: a browser whose `AudioDecoder` will not take
    /// what the gateway sends says so under the button and plays nothing. Worth
    /// running on each browser that matters rather than trusting a support table;
    /// the last representation needed Safari 18.4 and plenty of published tables
    /// still said it was unsupported.
    ///
    /// Which of the two it serves comes from the environment, so both can be heard
    /// against a deterministic source without a Windows host having to make a sound:
    ///
    /// ```sh
    /// REMOTEX_TEST_TONE_CODEC=pcm cargo test --lib serve_a_test_tone -- --ignored --nocapture
    /// ```
    ///
    /// The passthrough setting also answers a question the paragraph above cannot:
    /// it reaches no `AudioDecoder` at all, so a browser that plays the tone under
    /// `pcm` and not under `opus` has a WebCodecs problem rather than an audio one.
    #[tokio::test]
    #[ignore = "manual: serves a tone for a browser to play, and waits"]
    async fn serve_a_test_tone() {
        use std::io::Write as _;

        use tokio::net::TcpListener;

        use crate::audio::{AudioBridge, PCM_CD_QUALITY};
        use crate::config::{Protocol, Security, TargetConfig};
        use crate::protocol::{ServerMsg, UNSCALED};
        use crate::session::SessionManager;

        /// One 20 ms buffer of 440 Hz stereo sine, from `phase` in samples.
        fn tone(phase: &mut u32) -> Vec<u8> {
            const HZ: f32 = 440.0;
            let frames = PCM_CD_QUALITY.sample_rate / 50;
            let mut buf = Vec::with_capacity(frames as usize * 4);
            for _ in 0..frames {
                let t = *phase as f32 / PCM_CD_QUALITY.sample_rate as f32;
                let sample = ((t * HZ * std::f32::consts::TAU).sin() * 8000.0) as i16;
                // Both channels, little-endian: the layout the queue carries and
                // the encoder deinterleaves.
                buf.extend_from_slice(&sample.to_le_bytes());
                buf.extend_from_slice(&sample.to_le_bytes());
                *phase = phase.wrapping_add(1);
            }
            buf
        }

        // Read once and reused below, so what the target actually serves and what
        // the printed line claims it serves cannot drift apart.
        let tone_codec = match std::env::var("REMOTEX_TEST_TONE_CODEC").as_deref() {
            Ok("pcm") => Some(crate::config::AudioCodec::Pcm),
            Ok("opus") | Err(_) => None,
            Ok(other) => panic!("REMOTEX_TEST_TONE_CODEC is opus or pcm, not {other:?}"),
        };

        let target = TargetConfig {
            name: "test-tone".to_owned(),
            protocol: Protocol::Rdp,
            subtype: None,
            host: "127.0.0.1".to_owned(),
            port: 9, // discard: this engine is scripted, nothing is dialed
            username: String::new(),
            password: String::new(),
            vnc_password: String::new(),
            domain: None,
            width: Some(640),
            height: Some(480),
            security: Security::Auto,
            egfx: None,
            resize: false,
            clipboard: false,
            audio: true,
            audio_codec: tone_codec,
            render_type: crate::config::RenderType::Full,
            render_subtype: crate::config::RenderSubtype::Png,
            render_quality: None,
            render_motion_subtype: None,
            render_motion_quality: None,
            render_motion_debug: false,
            render_adaptive: false,
            render_adaptive_min: None,
            audio_bitrate: None,
            audio_adaptive: false,
            audio_bitrate_min: None,
        };

        // The scripted engine: announce a desktop size so the SPA leaves its
        // "waiting for the remote desktop" overlay and shows the floating menu,
        // then feed the bridge in real time. A plain thread rather than a task
        // because everything it touches is synchronous, and it holds both channel
        // ends so the session layer sees a live engine.
        let sessions = Arc::new(SessionManager::with_test_spawner(
            vec![target.clone()],
            |_target, input_rx, frame_tx, audio| {
                let audio: Arc<AudioBridge> = audio.expect("the target opted into audio");
                std::thread::spawn(move || {
                    let mut input_rx = input_rx;
                    let size = ServerMsg::Resize { w: 640, h: 480, scale: UNSCALED };
                    if frame_tx.blocking_send(size.clone()).is_err() {
                        return;
                    }
                    // Paced against a deadline rather than by sleeping a fixed
                    // 20 ms: the per-iteration overhead makes a fixed sleep
                    // deliver ~2.5 s of audio every 3 s, and a browser would
                    // stutter on the underrun — which is exactly the symptom this
                    // harness exists to measure honestly.
                    let buffer = std::time::Duration::from_millis(20);
                    let mut phase = 0u32;
                    let mut due = std::time::Instant::now();
                    // 250 buffers of 20 ms: five seconds of tone, then five of the
                    // remote being quiet, which on a real host means the audio
                    // channel closing and negotiating again.
                    let mut left_in_phase = 0u32;
                    let mut playing = false;
                    while !frame_tx.is_closed() {
                        if left_in_phase == 0 {
                            playing = !playing;
                            left_in_phase = 250;
                            if playing {
                                audio.publish_format(PCM_CD_QUALITY);
                            } else {
                                audio.clear_format();
                            }
                        }
                        left_in_phase -= 1;
                        if playing {
                            audio.wave(tone(&mut phase));
                        }
                        // Answer `Refresh` by re-announcing the size, which is what
                        // every real engine does and what a reattaching browser
                        // depends on: the session layer injects it on every attach,
                        // and a client that never hears a size sits on "waiting for
                        // the remote desktop" forever. Polled on the audio cadence
                        // rather than blocked on, since this thread owes the queue a
                        // buffer every 20 ms.
                        while let Ok(msg) = input_rx.try_recv() {
                            if matches!(msg, crate::protocol::ClientMsg::Refresh)
                                && frame_tx.blocking_send(size.clone()).is_err()
                            {
                                return;
                            }
                        }
                        due += buffer;
                        if let Some(nap) = due.checked_duration_since(std::time::Instant::now()) {
                            std::thread::sleep(nap);
                        }
                    }
                });
            },
        ));

        let config = AppConfig {
            listen: crate::config::ListenAddr::Tcp("127.0.0.1:0".to_owned()),
            static_dir: "frontend/dist".into(),
            targets: vec![target],
            auth: crate::auth::GatewayAuth::Login(
                crate::auth::SitePasswd::parse(
                    &crate::auth::generate("admin", "hunter2", 4).unwrap(),
                )
                .unwrap(),
            ),
            branding: crate::config::Branding {
                text: "audio tone harness".to_owned(),
                logo: None,
            },
            dev_hostname: None,
            allow_shell_origin: false,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router_with_sessions(config, sessions);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // println! rather than log: this is the test's whole user interface.
        println!("\n  Open  http://{addr}/   (admin / hunter2)");
        println!("  Pick \"test-tone\", then ☰ → Enable audio. 440 Hz for 5s, quiet for 5s.");
        println!("  Press it during a quiet phase and then close the drawer: the tone");
        println!("  must arrive on its own, go away, and come back, untouched.");
        // Two different things to watch for, so they are said separately rather
        // than as one sentence that half applies.
        match tone_codec {
            Some(crate::config::AudioCodec::Pcm) => {
                println!("  Serving passthrough PCM, which reaches no decoder at all: silence");
                println!("  here is the schedule or the socket, never WebCodecs.");
            }
            _ => {
                println!("  Serving Opus through WebCodecs. A line under the button instead");
                println!("  means this browser has no decoder for it.");
            }
        }
        println!("  Set REMOTEX_TEST_TONE_CODEC=pcm|opus to try the other.");
        // A real `audio = true` RDP target separately covers server negotiation.
        println!("  Ctrl-C when done; this waits 15 minutes.\n");
        std::io::stdout().flush().unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(900)).await;
    }

    /// The exact `/api/targets` entry. Pinned because the picker reads every key
    /// of it, and because `subtype` is `null` far more often than it is set — a
    /// field that were *absent* on those targets is one the client has to test for
    /// two ways.
    #[test]
    fn a_target_entry_names_its_subtype_or_says_it_has_none() {
        let mac = serde_json::to_string(&TargetInfo {
            name: "mac".to_owned(),
            protocol: "vnc",
            subtype: Some("ard-high-performance"),
            host: "192.0.2.10".to_owned(),
            port: 5900,
        })
        .unwrap();
        assert_eq!(
            mac,
            r#"{"name":"mac","protocol":"vnc","subtype":"ard-high-performance","host":"192.0.2.10","port":5900}"#
        );

        let win = serde_json::to_string(&TargetInfo {
            name: "win".to_owned(),
            protocol: "rdp",
            subtype: None,
            host: "192.0.2.11".to_owned(),
            port: 3389,
        })
        .unwrap();
        assert!(win.contains(r#""subtype":null"#), "{win}");
    }

    /// The exact `/api/config` body. Pinned because the login screen reads the
    /// branding before authentication.
    #[test]
    fn config_response_contains_only_public_branding() {
        let json = serde_json::to_string(&ConfigResponse {
            branding: "remotex".to_owned(),
            logo: false,
        })
        .unwrap();
        assert_eq!(json, r#"{"branding":"remotex","logo":false}"#);
    }
}
