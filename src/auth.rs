//! Web login: the `site_passwd` credential and the in-memory auth
//! sessions behind the `remotex_session` cookie.
//!
//! TOML strings carry a bcrypt hash verbatim — its `$./` alphabet needs no
//! escaping: `[server].site_passwd` is plain `username:bcrypt_hash` (generated
//! by `remotex gen-passwd <username>`), `/api/auth/login` verifies a
//! username/password pair against it and mints a session token, and everything
//! guarded (the rest of `/api/*`, `/ws`) requires that token in the cookie.
//! Sessions live in memory only — a server restart logs every browser out,
//! which is fine for a single-user program.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// Name of the auth session cookie.
pub const COOKIE_NAME: &str = "remotex_session";

/// Default bcrypt cost for `gen-passwd` (the crate default).
pub const DEFAULT_COST: u32 = bcrypt::DEFAULT_COST;

/// Sliding session lifetime: every validated request pushes the expiry out.
const SESSION_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// The parsed `[server].site_passwd` credential: the one username allowed to
/// log in and the bcrypt hash of its password.
#[derive(Clone)]
pub struct SitePasswd {
    username: String,
    hash: String,
}

// Manual Debug so an AppConfig dump never prints the hash.
impl std::fmt::Debug for SitePasswd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SitePasswd")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl SitePasswd {
    /// Parse the credential: `username:bcrypt_hash`, verbatim.
    pub fn parse(credential: &str) -> anyhow::Result<Self> {
        let (username, hash) = credential
            .trim()
            .split_once(':')
            .context("site_passwd must be username:bcrypt_hash")?;
        anyhow::ensure!(!username.is_empty(), "site_passwd username is empty");
        anyhow::ensure!(
            ["$2a$", "$2b$", "$2y$"].iter().any(|p| hash.starts_with(p)),
            "site_passwd hash for user {username:?} is not bcrypt (want $2a$/$2b$/$2y$)"
        );
        Ok(Self {
            username: username.to_owned(),
            hash: hash.to_owned(),
        })
    }

    /// The username allowed to log in (for startup logging).
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Check a login attempt. A wrong username still burns a bcrypt verify so
    /// the two rejections don't differ by timing.
    pub fn verify(&self, username: &str, password: &str) -> bool {
        let password_ok = bcrypt::verify(password, &self.hash).unwrap_or(false);
        password_ok && username == self.username
    }
}

/// Generate a `site_passwd` value (the `gen-passwd` subcommand; tests pass a
/// low cost to keep logins fast).
pub fn generate(username: &str, password: &str, cost: u32) -> anyhow::Result<String> {
    anyhow::ensure!(!username.is_empty(), "username must not be empty");
    anyhow::ensure!(!username.contains(':'), "username must not contain ':'");
    anyhow::ensure!(!password.is_empty(), "password must not be empty");
    let hash = bcrypt::hash(password, cost).context("bcrypt hash failed")?;
    Ok(format!("{username}:{hash}"))
}

/// In-memory auth sessions: token → expiry, with a sliding TTL. The map only
/// ever holds this single user's handful of logins, so expired entries are
/// swept opportunistically on create/validate instead of by a timer.
pub struct AuthSessions {
    sessions: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
}

impl Default for AuthSessions {
    fn default() -> Self {
        Self::with_ttl(SESSION_TTL)
    }
}

impl AuthSessions {
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Mint a session token after a successful login.
    pub fn create(&self) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, expiry| *expiry > now);
        sessions.insert(token.clone(), now + self.ttl);
        token
    }

    /// True when the token names a live session; refreshes the sliding expiry.
    pub fn validate(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        match sessions.get_mut(token) {
            Some(expiry) if *expiry > now => {
                *expiry = now + self.ttl;
                true
            }
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }

    /// Drop a session (logout).
    pub fn invalidate(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

/// Extract this app's session token from a request's `Cookie` header.
pub fn token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE_NAME).then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::*;

    // bcrypt's minimum cost, so tests don't burn CPU on real key stretching.
    const TEST_COST: u32 = 4;

    #[test]
    fn generate_parse_verify_roundtrip() {
        let encoded = generate("admin", "hunter2", TEST_COST).unwrap();
        let site = SitePasswd::parse(&encoded).unwrap();
        assert_eq!(site.username(), "admin");
        assert!(site.verify("admin", "hunter2"));
        assert!(!site.verify("admin", "wrong"));
        assert!(!site.verify("mallory", "hunter2"), "username must match too");
    }

    #[test]
    fn generate_rejects_bad_input() {
        assert!(generate("", "pw", TEST_COST).is_err());
        assert!(generate("a:b", "pw", TEST_COST).is_err(), "':' would corrupt the format");
        assert!(generate("admin", "", TEST_COST).is_err());
    }

    #[test]
    fn parse_rejects_malformed_credentials() {
        let msg = |s: &str| format!("{:#}", SitePasswd::parse(s).unwrap_err());
        assert!(msg("just-a-username").contains("username:bcrypt_hash"));
        assert!(msg(":$2b$04$hash").contains("username is empty"));
        assert!(msg("admin:$1$legacy$hash").contains("bcrypt"), "only bcrypt hashes");
    }

    #[test]
    fn sessions_validate_refresh_and_invalidate() {
        let sessions = AuthSessions::default();
        let token = sessions.create();
        assert!(sessions.validate(&token));
        assert!(!sessions.validate("no-such-token"));
        sessions.invalidate(&token);
        assert!(!sessions.validate(&token));
    }

    #[test]
    fn expired_sessions_are_rejected() {
        let sessions = AuthSessions::with_ttl(Duration::ZERO);
        let token = sessions.create();
        assert!(!sessions.validate(&token), "a zero-TTL session is born expired");
    }

    #[test]
    fn token_is_found_among_other_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=1; {COOKIE_NAME}=tok-123; x=y")).unwrap(),
        );
        assert_eq!(token_from_headers(&headers).as_deref(), Some("tok-123"));

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("other=1"));
        assert_eq!(token_from_headers(&headers), None);
        assert_eq!(token_from_headers(&HeaderMap::new()), None);
    }
}
