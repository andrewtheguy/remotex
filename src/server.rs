use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderName, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use tower::service_fn;
use tower_http::services::ServeDir;

use crate::{
    auth::{self, AuthSessions},
    config::AppConfig,
    error::{ApiResult, AppError},
    protocol,
    session::SessionManager,
    ws,
};

/// Shared application state handed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    /// The single session slot: claim here, attach over `/ws`.
    pub sessions: Arc<SessionManager>,
    /// Live auth sessions behind the login cookie.
    pub auth: Arc<AuthSessions>,
}

/// Build the axum router.
///
/// - `/api/auth/*` + `/api/health` — public: the login flow itself and the
///   liveness probe.
/// - the rest of `/api/*` and `/ws` — refuse requests without a valid login
///   cookie; unknown `/api/*` paths return 404 rather than the SPA,
///   so API clients get an honest error.
/// - `/ws`    — binary WebSocket carrying the remote-desktop session
/// - fallback — the built SPA, served from `config.static_dir` on disk. Real
///   files are served by [`ServeDir`]; any unknown path returns `index.html`
///   with a 200 so client-side routes resolve (matching an SPA's expectations).
///   The static shell stays public — it renders the login screen and holds no
///   secrets; everything it talks to is behind the cookie.
pub fn router(config: AppConfig) -> Router {
    let sessions = Arc::new(SessionManager::new(config.targets.clone()));
    router_with_sessions(config, sessions)
}

/// [`router`] over a caller-supplied session slot.
///
/// The seam exists for one thing: the manual audio harness
/// ([`tests::serve_a_test_tone`]) needs the real router — SPA, login, and the
/// audio endpoint — in front of a scripted engine rather than a real RDP connect.
pub(crate) fn router_with_sessions(
    config: AppConfig,
    sessions: Arc<SessionManager>,
) -> Router {
    // Use `.fallback` (returns the fallback response as-is) rather than
    // `.not_found_service` (which forces a 404 status), so SPA routes get 200.
    let index_path = config.static_dir.join("index.html");
    let spa_index = service_fn(move |_req| {
        let index_path = index_path.clone();
        async move {
            let response = match tokio::fs::read(&index_path).await {
                Ok(bytes) => {
                    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            };
            Ok::<_, Infallible>(response)
        }
    });
    let spa = ServeDir::new(&config.static_dir).fallback(spa_index);

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
        .route("/auth/login", post(login_handler))
        .route("/auth/logout", post(logout_handler))
        .route("/auth/status", get(status_handler))
        .merge(
            Router::new()
                .route("/targets", get(targets_handler))
                .route("/session", post(claim_handler))
                .route("/session/audio", get(audio_handler))
                .route_layer(require_auth.clone()),
        )
        .fallback(|| async { AppError::NotFound });

    Router::new()
        .nest("/api", api)
        // The cookie check runs before the upgrade, so an unauthenticated
        // WebSocket attempt fails its handshake with a plain 401. (A sub-router
        // because route_layer must come after a route to apply to it.)
        .merge(
            Router::new()
                .route("/ws", any(ws::handler))
                .route_layer(require_auth),
        )
        .fallback_service(spa)
        .with_state(state)
}

/// Middleware guarding everything session-related: no valid login cookie, no
/// service. Validation also refreshes the session's sliding expiry.
async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let authenticated = auth::token_from_headers(req.headers())
        .is_some_and(|token| state.auth.validate(&token));
    if !authenticated {
        return AppError::Unauthorized.into_response();
    }
    next.run(req).await
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
    let site_passwd = state.config.site_passwd.clone();
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
#[serde(rename_all = "camelCase")]
struct ConfigResponse {
    branding: String,
    /// [`protocol::PROTOCOL_VERSION`] — what the macOS viewer checks before it
    /// opens a session, since it ships separately from this binary.
    protocol_version: u32,
}

/// Public, non-secret client config. Read on load so the login screen and the
/// browser tab title carry the deployment's branding before authentication, and
/// so a separately-shipped client can refuse a wire protocol it cannot speak.
async fn config_handler(State(state): State<AppState>) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        branding: state.config.branding.clone(),
        protocol_version: protocol::PROTOCOL_VERSION,
    })
}

#[derive(Serialize)]
struct StatusResponse {
    authenticated: bool,
}

/// Whether the caller holds a live session — the SPA asks on load to decide
/// between the login screen and the desktop.
async fn status_handler(State(state): State<AppState>, headers: HeaderMap) -> Json<StatusResponse> {
    let authenticated = auth::token_from_headers(&headers)
        .is_some_and(|token| state.auth.validate(&token));
    Json(StatusResponse { authenticated })
}

#[derive(Serialize)]
struct TargetInfo {
    name: String,
    protocol: &'static str,
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

#[derive(Deserialize)]
struct AudioParams {
    session: Option<String>,
}

/// How long to wait for the remote's audio channel to finish negotiating before
/// answering 503.
///
/// There is something to wait for because a browser can ask the moment the
/// desktop appears, while MS-RDPEA is still exchanging formats a few PDUs behind
/// it. Bounded because the other reason nothing has been negotiated is that
/// nothing ever will be — a server that offers no format this gateway asked for
/// (see [`crate::rdp_audio`]) — and a request that hung for that would look like
/// audio that is merely slow.
const AUDIO_FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// The claimed session's live audio, as an open-ended `audio/wav` response the
/// browser plays with a plain `<audio>` element (see docs/remote-audio.md).
///
/// Deliberately not part of the desktop WebSocket: the browser already has a
/// streaming audio client, so this hands it one and leaves buffering, decoding
/// and playback there.
///
/// Authorised twice over, and the second half is the point: the login cookie gets
/// the request past `require_auth`, and the claim token proves the caller holds
/// the *session* — the same token `/ws` attaches with. So the stream belongs to
/// whoever has the single session slot, not to anyone with a login.
///
/// There is no `Content-Length`, no recording, and no seekable history: a
/// listener starts at live audio and receives what arrives after it attached. A
/// `Range` request is answered with this same stream rather than a `206`, since
/// there is no range of anything to serve.
async fn audio_handler(
    State(state): State<AppState>,
    Query(params): Query<AudioParams>,
) -> ApiResult<Response> {
    // Every arrival is logged, and so is every refusal. A media element reports a
    // failed load as nothing more than an `error` event on itself, so without a
    // line here the difference between "the browser never asked", "the token was
    // stale" and "the remote has no audio" is invisible from both ends at once —
    // which is exactly the hole this fills.
    info!("audio: stream requested");
    let Some(token) = params.session else {
        warn!("audio: refused, the request carried no session token");
        return Err(AppError::Forbidden);
    };
    let mut listener = state.sessions.audio_listener(&token).inspect_err(|e| {
        warn!("audio: refused, {e}");
    })?;
    let Some(format) = listener.await_format(AUDIO_FORMAT_TIMEOUT).await else {
        warn!(
            "audio: refused, the remote negotiated no audio format within {}s",
            AUDIO_FORMAT_TIMEOUT.as_secs()
        );
        return Err(AppError::Unavailable("remote audio"));
    };
    info!("audio: streaming {} Hz PCM", format.sample_rate);
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            // Nothing about a live stream may be stored, and nothing may
            // recompress or re-chunk it on the way — `no-transform` is the half
            // that speaks to intermediaries rather than to the browser.
            (header::CACHE_CONTROL, "no-store, no-transform"),
            // There is no length and no history, so there is nothing to range
            // over; saying so stops a client probing for one first.
            (header::ACCEPT_RANGES, "none"),
            // nginx buffers a proxied response by default, which would hold the
            // whole point of this endpoint back. Exact proxy configuration is
            // deployment-specific; this is the one header worth sending
            // unconditionally because it is inert everywhere else.
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(listener.into_stream(format)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve the real gateway with a **generated tone** in place of a remote's
    /// audio, so the browser half of the audio path can be listened to without a
    /// server that redirects.
    ///
    /// This exists because of what the first live test found: the RDP side and the
    /// HTTP side fail independently, and until a Windows host actually hands this
    /// gateway a buffer, the question the whole design rests on — **does a browser
    /// play an open-ended `audio/wav` response progressively, or wait for it to
    /// end?** — cannot be asked at all. The PCM's provenance is irrelevant to that
    /// question, so this supplies it locally.
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
    /// Then open the printed URL, log in, pick the target, and open ☰ → Audio. A
    /// 440 Hz tone means the whole browser-side path works. Silence with a stalled
    /// player is the finding the representation would have to change for.
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
                // Both channels, little-endian: what the WAV header promises.
                buf.extend_from_slice(&sample.to_le_bytes());
                buf.extend_from_slice(&sample.to_le_bytes());
                *phase = phase.wrapping_add(1);
            }
            buf
        }

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
            width: 640,
            height: 480,
            security: Security::Auto,
            resize: false,
            clipboard: false,
            audio: true,
            agent_public_key: String::new(),
            gateway_private_key: String::new(),
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
                    // What a real negotiation publishes, and what the endpoint
                    // waits for: without it the response is the honest 503 rather
                    // than a header for a format nothing agreed on.
                    audio.publish_format(PCM_CD_QUALITY);
                    // Paced against a deadline rather than by sleeping a fixed
                    // 20 ms: the per-iteration overhead makes a fixed sleep
                    // deliver ~2.5 s of audio every 3 s, and a browser would
                    // stutter on the underrun — which is exactly the symptom this
                    // harness exists to measure honestly.
                    let buffer = std::time::Duration::from_millis(20);
                    let mut phase = 0u32;
                    let mut due = std::time::Instant::now();
                    while !frame_tx.is_closed() {
                        audio.wave(tone(&mut phase));
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
            host: "127.0.0.1".to_owned(),
            port: 0,
            static_dir: "frontend/dist".into(),
            targets: vec![target],
            site_passwd: crate::auth::SitePasswd::parse(
                &crate::auth::generate("admin", "hunter2", 4).unwrap(),
            )
            .unwrap(),
            branding: "audio tone harness".to_owned(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router_with_sessions(config, sessions);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // println! rather than log: this is the test's whole user interface.
        println!("\n  Open  http://{addr}/   (admin / hunter2)");
        println!("  Pick \"test-tone\", then ☰ → Audio. You should hear 440 Hz.");
        println!("  Ctrl-C when done; this waits 15 minutes.\n");
        std::io::stdout().flush().unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(900)).await;
    }

    /// The exact `/api/config` body. Pinned because the macOS viewer decodes it
    /// with a hand-written `Codable` and refuses a `protocolVersion` it doesn't
    /// recognise: a rename here would read to it as an unreachable gateway.
    #[test]
    fn config_response_serializes_camel_case() {
        let json = serde_json::to_string(&ConfigResponse {
            branding: "remotex".to_owned(),
            protocol_version: 1,
        })
        .unwrap();
        assert_eq!(json, r#"{"branding":"remotex","protocolVersion":1}"#);
    }
}
