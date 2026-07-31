import { useEffect, useState } from "react";

// The post-login target picker: the state where the user is authenticated and
// holds the session slot, but no connection has started yet (see
// useRemoteDesktop's "picker" mode). It lists the `[[targets]]` profiles from
// GET /api/targets and starts a session against the one the user picks.
//
// `connect` sends the pick over the live socket; `pendingTarget` is the profile
// a pick is waiting on (buttons lock until the server answers). `connectError`
// carries a failed connect's message so it shows here rather than on a
// dead-end screen. `remoteBusy` is the one refusal with something to press: the
// remote's own session is held by a different client, and connecting again with
// `force` takes it. `onLogout` ends the web login; `onUnauthorized` fires if the
// target list itself comes back 401 (the login expired).

interface TargetInfo {
  name: string;
  protocol: string;
  host: string;
  port: number;
}

// "12m" rather than "754s", at the precision a glance wants — the same three
// steps the agent's own menu bar uses for the same number.
function heldFor(seconds: number): string {
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m`;
  }
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

export default function TargetPicker({
  branding,
  connect,
  pendingTarget,
  connectError,
  remoteBusy,
  autoResizeByDefault,
  audioByDefault,
  onAutoResizeByDefaultChange,
  onAudioByDefaultChange,
  onLogout,
  onUnauthorized,
}: {
  branding: string;
  connect: (name: string, force?: boolean) => void;
  pendingTarget: string | null;
  connectError: string | null;
  remoteBusy: {
    target: string;
    holder: string;
    heldSecs: number;
    takenOver: boolean;
  } | null;
  // The two remembered defaults, shown here as the one place they can be set
  // before a target is picked — and the same values the desktop menu's live
  // controls edit. "… if compatible" is deliberately not a per-target check: the
  // picker never learns a target's capabilities (GET /api/targets carries none),
  // so this is an intent, applied on connect only where `connected` reports the
  // target can honour it.
  autoResizeByDefault: boolean;
  audioByDefault: boolean;
  onAutoResizeByDefaultChange: (enabled: boolean) => void;
  onAudioByDefaultChange: (enabled: boolean) => void;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const [targets, setTargets] = useState<TargetInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/api/targets")
      .then((res) => {
        if (res.status === 401) {
          onUnauthorized();
          return null;
        }
        if (!res.ok) {
          throw new Error(`HTTP ${res.status}`);
        }
        return res.json() as Promise<TargetInfo[]>;
      })
      .then((list) => {
        if (!cancelled && list) {
          setTargets(list);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setLoadError("Could not load targets");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [onUnauthorized]);

  return (
    <div className="picker-screen">
      <div className="picker-panel">
        <span className="picker-brand">{branding}</span>
        <h1>Pick a target</h1>
        {connectError && <p className="picker-error">{connectError}</p>}
        {loadError && <p className="picker-error">{loadError}</p>}
        {remoteBusy && (
          <div className="picker-busy">
            {remoteBusy.takenOver ? (
              // This session was running and somebody else claimed it. Say what
              // happened rather than describing the target as busy: nothing was
              // asked for here, and only the remote's session went — this login
              // and this gateway are still ours, which is why the list is right
              // here to pick from again.
              <p>
                Your session on{" "}
                <strong>{remoteBusy.target || "that target"}</strong> was taken
                over from {remoteBusy.holder}.
              </p>
            ) : (
              <p>
                <strong>{remoteBusy.target || "That target"}</strong> is in use
                from {remoteBusy.holder}, for {heldFor(remoteBusy.heldSecs)}.
              </p>
            )}
            <button
              type="button"
              className="picker-takeover"
              onClick={() => connect(remoteBusy.target, true)}
              disabled={pendingTarget !== null || !remoteBusy.target}
            >
              {remoteBusy.takenOver ? "Take it back" : "Take over"}
            </button>
          </div>
        )}
        {targets === null && !loadError && (
          <p className="picker-hint">Loading targets…</p>
        )}
        {targets?.length === 0 && (
          <p className="picker-hint">No targets are configured.</p>
        )}
        <ul className="picker-list">
          {targets?.map((t) => {
            const connecting = pendingTarget === t.name;
            return (
              <li key={t.name}>
                <button
                  type="button"
                  className="picker-target"
                  onClick={() => connect(t.name)}
                  disabled={pendingTarget !== null}
                >
                  <span className="picker-target-name">{t.name}</span>
                  <span className="picker-target-meta">
                    {connecting
                      ? "Connecting…"
                      : `${t.protocol.toUpperCase()} · ${t.host}:${t.port}`}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
        {/* The two remembered defaults, applied to whatever is picked above only
            where the target supports it — hence "if compatible", a fixed caption
            rather than a per-target check the picker has no way to make. */}
        <div className="picker-defaults">
          <label className="picker-default">
            <input
              type="checkbox"
              checked={autoResizeByDefault}
              onChange={(e) => onAutoResizeByDefaultChange(e.target.checked)}
            />
            <span>Auto-resize the remote to the window, if compatible</span>
          </label>
          <label className="picker-default">
            <input
              type="checkbox"
              checked={audioByDefault}
              onChange={(e) => onAudioByDefaultChange(e.target.checked)}
            />
            <span>Play the remote's sound, if compatible</span>
          </label>
        </div>
        <button
          type="button"
          className="picker-logout"
          onClick={onLogout}
          disabled={pendingTarget !== null}
        >
          Log out
        </button>
      </div>
    </div>
  );
}
