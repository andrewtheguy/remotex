//! End-to-end tests of the web login: `/api/auth/*`, the cookie, and
//! the guard on the rest of `/api/*` and `/ws`.
//!
//! These only exercise the HTTP surface — no engine ever connects — so the
//! target points at a port nothing listens on.

mod common;

use std::net::SocketAddr;

use rdpweb::config::{AppConfig, Protocol, Security, TargetConfig};
use rdpweb::server;
use tokio::net::TcpListener;

/// Start the server with the shared test credential. The target is never
/// dialed by these tests.
async fn spawn_app() -> SocketAddr {
    let config = AppConfig {
        host: "127.0.0.1".to_owned(),
        port: 0,
        static_dir: "frontend/dist".into(),
        site_passwd: common::test_site_passwd(),
        target: TargetConfig {
            name: "unreachable".to_owned(),
            protocol: Protocol::Vnc,
            host: "127.0.0.1".to_owned(),
            port: 9, // discard; never connected
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 1280,
            height: 800,
            security: Security::Auto,
            resize: false,
        },
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::router(config);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn post_login(addr: SocketAddr, body: &str) -> (u16, String, String) {
    let req = format!(
        "POST /api/auth/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    common::http_request(addr, &req).await
}

async fn get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> (u16, String) {
    let cookie_line = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{cookie_line}Connection: close\r\n\r\n"
    );
    let (status, _head, body) = common::http_request(addr, &req).await;
    (status, body)
}

#[tokio::test]
async fn guarded_routes_refuse_unauthenticated_requests() {
    let addr = spawn_app().await;

    let (status, _) = get(addr, "/api/config", None).await;
    assert_eq!(status, 401, "/api/config must require a login");

    let (status, _) = common::post_session(addr, "", "{}").await;
    assert_eq!(status, 401, "/api/session must require a login");

    // A made-up cookie is as good as none.
    let (status, _) = get(addr, "/api/config", Some("rdpweb_session=forged")).await;
    assert_eq!(status, 401, "a forged cookie must be refused");

    // The public surface still answers.
    let (status, body) = get(addr, "/api/health", None).await;
    assert_eq!((status, body.as_str()), (200, "ok"));
    let (status, body) = get(addr, "/api/auth/status", None).await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"authenticated":false}"#);
}

#[tokio::test]
async fn login_rejects_wrong_credentials_without_a_cookie() {
    let addr = spawn_app().await;

    for body in [
        r#"{"username":"admin","password":"wrong"}"#,
        r#"{"username":"mallory","password":"hunter2"}"#,
        r#"{"username":"","password":""}"#,
    ] {
        let (status, head, _) = post_login(addr, body).await;
        assert_eq!(status, 401, "must refuse {body}");
        assert!(
            !head.to_lowercase().contains("set-cookie"),
            "a refused login must not set a cookie: {head}"
        );
    }
}

#[tokio::test]
async fn login_sets_the_session_cookie_and_grants_access() {
    let addr = spawn_app().await;

    let (status, head, body) =
        post_login(addr, r#"{"username":"admin","password":"hunter2"}"#).await;
    assert_eq!(status, 200, "login failed: {body}");
    assert_eq!(body, r#"{"ok":true}"#);

    let set_cookie = head
        .lines()
        .find(|l| l.to_lowercase().starts_with("set-cookie:"))
        .expect("login must set the session cookie");
    assert!(set_cookie.contains("rdpweb_session="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/"), "{set_cookie}");
    // Plain HTTP (no TLS proxy): Secure would make Safari drop the cookie.
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");

    let cookie = set_cookie["set-cookie:".len()..]
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let (status, body) = get(addr, "/api/config", Some(&cookie)).await;
    assert_eq!(status, 200, "the cookie must unlock the API: {body}");
    let (status, body) = get(addr, "/api/auth/status", Some(&cookie)).await;
    assert_eq!((status, body.as_str()), (200, r#"{"authenticated":true}"#));
}

#[tokio::test]
async fn websocket_upgrade_without_a_login_fails_with_401() {
    let addr = spawn_app().await;

    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?session=whatever"))
        .await
        .expect_err("the unauthenticated upgrade must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 401)
        }
        other => panic!("expected an HTTP 401 handshake failure, got: {other:?}"),
    }
}

#[tokio::test]
async fn logout_invalidates_the_session_and_clears_the_cookie() {
    let addr = spawn_app().await;
    let cookie = common::login(addr).await;

    let (status, _) = get(addr, "/api/config", Some(&cookie)).await;
    assert_eq!(status, 200);

    let req = format!(
        "POST /api/auth/logout HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Cookie: {cookie}\r\nContent-Length: 0\r\n\r\n"
    );
    let (status, head, _) = common::http_request(addr, &req).await;
    assert_eq!(status, 200);
    let set_cookie = head
        .lines()
        .find(|l| l.to_lowercase().starts_with("set-cookie:"))
        .expect("logout must clear the cookie");
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");

    // The old token is dead server-side, not just cleared in the browser.
    let (status, _) = get(addr, "/api/config", Some(&cookie)).await;
    assert_eq!(status, 401, "the invalidated session must be refused");
    let (_, body) = get(addr, "/api/auth/status", Some(&cookie)).await;
    assert_eq!(body, r#"{"authenticated":false}"#);
}
