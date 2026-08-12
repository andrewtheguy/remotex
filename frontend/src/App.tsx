import { useCallback, useEffect, useState } from "react";
import { gatewayFetch, gatewayUrl } from "./gateway.ts";
import { gatewayConfig } from "./gatewayConfig.ts";
import Login from "./Login.tsx";
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
    gatewayConfig().then(({ branding, logo }) => {
      if (cancelled) {
        return;
      }
      if (branding) {
        setBranding(branding);
        document.title = branding;
      }
      // The tab's icon. There is no <link rel="icon"> in index.html to fight
      // with — a gateway without a logo keeps no icon at all — so this only ever
      // adds one, and never needs removing: the config is fetched once per page.
      if (logo) {
        const link = document.createElement("link");
        link.rel = "icon";
        link.href = gatewayUrl("/api/logo");
        document.head.appendChild(link);
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

  if (authState !== "authenticated") {
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
