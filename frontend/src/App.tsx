import { useCallback, useEffect, useState } from "react";
import Login from "./Login.tsx";
import { postNativeHostEvent, useNativeHost } from "./nativeHost.ts";
import RemoteDesktop from "./RemoteDesktop.tsx";
import { SESSION_KEY } from "./useRemoteDesktop.ts";

// Gate the desktop behind the web login. The desktop is only mounted
// once authenticated — mounting it claims the session slot, which must not
// happen before the login succeeds.
type AuthState = "checking" | "unauthenticated" | "authenticated";

export default function App() {
  const [authState, setAuthState] = useState<AuthState>("checking");
  const nativeHost = useNativeHost();
  // Deployment branding (login screen, interstitials, tab title). Defaults to
  // "remotex" and stays there until GET /api/config answers — a public route,
  // so it resolves before login.
  const [branding, setBranding] = useState("remotex");

  useEffect(() => {
    let cancelled = false;
    fetch("/api/config")
      .then((res) => res.json() as Promise<{ branding: string }>)
      .then(({ branding }) => {
        if (!cancelled && branding) {
          setBranding(branding);
          document.title = branding;
        }
      })
      .catch(() => {
        // Keep the "remotex" default; the tab title stays index.html's.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!nativeHost || authState === "authenticated") {
      return;
    }
    postNativeHostEvent({
      type: "state",
      state: {
        screen: authState === "checking" ? "checking" : "login",
        connectionStatus: null,
        connectedTarget: null,
        remoteIsMac: false,
        canResize: false,
        canClipboard: false,
        canCaptureKeyboard: false,
      },
    });
  }, [authState, nativeHost]);

  useEffect(() => {
    let cancelled = false;
    fetch("/api/auth/status")
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
    void fetch("/api/auth/logout", { method: "POST" }).finally(() =>
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
      nativeHost={nativeHost}
      onLogout={logout}
      onUnauthorized={unauthorized}
    />
  );
}
