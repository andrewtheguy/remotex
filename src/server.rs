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
/// - `/ws` — binary WebSocket carrying the remote-desktop session, including audio.
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
/// ([`tests::serve_a_test_tone`]) needs the real router — SPA, login, and `/ws` — in
/// front of a scripted engine rather than a real RDP connect.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    /// browser — and **this is where Opus-only is settled**, because there is no
    /// fallback: a browser whose `AudioDecoder` will not take Opus says so under the
    /// button and plays nothing. Worth running on each browser that matters rather
    /// than trusting a support table; the last representation needed Safari 18.4 and
    /// plenty of published tables still said it was unsupported.
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
        println!("  Pick \"test-tone\", then ☰ → Enable audio. 440 Hz for 5s, quiet for 5s.");
        println!("  Press it during a quiet phase and then close the drawer: the tone");
        println!("  must arrive on its own, go away, and come back, untouched.");
        println!("  A line under the button instead means this browser has no Opus decoder.");
        println!("  The macOS viewer takes the same test through Remote > Enable Audio:");
        println!("  open -n dist/remotex-viewer.app --args --settings qa \\");
        println!("    --gateway http://{addr}");
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
