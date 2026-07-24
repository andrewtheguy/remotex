import { type FormEvent, useState } from "react";

// The web-login gate: one user, POST /api/auth/login sets the
// session cookie. Shown while the mount-time auth check runs and whenever the
// server answers 401. The version is pinned at the bottom.
export default function Login({
  checking,
  onLogin,
}: {
  /** True while the mount-time /api/auth/status probe is still in flight. */
  checking: boolean;
  onLogin: () => void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const res = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      });
      if (res.ok) {
        onLogin();
        return;
      }
      setError(
        res.status === 401
          ? "Invalid credentials"
          : `Login failed (${res.status})`,
      );
    } catch {
      setError("Network error");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="login-screen">
      {checking ? (
        <span className="login-hint">Checking authentication…</span>
      ) : (
        <form className="login-form" onSubmit={(e) => void submit(e)}>
          <h1>rdpweb</h1>
          {error && <p className="login-error">{error}</p>}
          <label htmlFor="login-username">Username</label>
          <input
            id="login-username"
            type="text"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoComplete="username"
            autoCapitalize="off"
            disabled={submitting}
          />
          <label htmlFor="login-password">Password</label>
          <input
            id="login-password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="current-password"
            disabled={submitting}
          />
          <button type="submit" disabled={submitting}>
            {submitting ? "Logging in…" : "Log in"}
          </button>
        </form>
      )}
      <div className="login-version">v{__APP_VERSION__}</div>
    </div>
  );
}
