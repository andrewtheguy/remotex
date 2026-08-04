import { useCallback, useEffect, useState } from "react";
import { gatewayFetch } from "./gateway.ts";
import { gatewayConfig } from "./gatewayConfig.ts";
import Login from "./Login.tsx";
import { NATIVE_HOST, postToHost } from "./nativeHost.ts";
import RemoteDesktop from "./RemoteDesktop.tsx";
import { SESSION_KEY } from "./useRemoteDesktop.ts";

// Gate the desktop behind the web login. The desktop is only mounted
// once authenticated — mounting it claims the session slot, which must not
// happen before the login succeeds.
type AuthState = "checking" | "unauthenticated" | "authenticated";

export default function App() {
  const [authState, setAuthState] = useState<AuthState>("checking");
  // Deployment branding (login screen, interstitials, tab title). Defaults to
  // "remotex" and stays there until GET /api/config answers — a public route,
  // so it resolves before login.
  const [branding, setBranding] = useState("remotex");

  useEffect(() => {
    let cancelled = false;
    gatewayConfig().then(({ branding }) => {
      if (!cancelled && branding) {
        setBranding(branding);
        document.title = branding;
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    gatewayFetch("/api/auth/status")
      .then((res) => res.json() as Promise<{ authenticated: boolean }>)
      .then(({ authenticated }) => {
        if (!cancelled) {
          setAuthState(authenticated ? "authenticated" : "unauthenticated");
        }
      })
      .catch(() => {
        if (!cancelled) {
          setAuthState("unauthenticated");
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Log out: end this browser's login. The slot token goes too, so
  // the next login claims fresh instead of silently reattaching.
  const logout = useCallback(() => {
    sessionStorage.removeItem(SESSION_KEY);
    void gatewayFetch("/api/auth/logout", { method: "POST" }).finally(() =>
      setAuthState("unauthenticated"),
    );
  }, []);

  // The server answered 401 mid-session (expired session or a restart wiped
  // the in-memory store): back to the login screen.
  const unauthorized = useCallback(() => setAuthState("unauthenticated"), []);

  // Inside `remotex.app` there is no login screen to fall back to: the gateway
  // takes the launch token the app put in the cookie store, and if that is not
  // what it minted, nobody can type their way out of it. Tell the app, which has a
  // screen for exactly this, and show it nothing meanwhile.
  useEffect(() => {
    if (NATIVE_HOST && authState === "unauthenticated") {
      postToHost({ type: "unauthenticated" });
    }
  }, [authState]);

  if (authState !== "authenticated") {
    if (NATIVE_HOST) {
      return null;
    }
    return (
      <Login
        checking={authState === "checking"}
        branding={branding}
        onLogin={() => setAuthState("authenticated")}
      />
    );
  }
  return (
    <RemoteDesktop
      branding={branding}
      onLogout={logout}
      onUnauthorized={unauthorized}
    />
  );
}
