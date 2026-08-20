import { useEffect, useState } from "react";
import { connectionShortLabel } from "./connectionLabel.ts";
import { gatewayFetch } from "./gateway.ts";

// The post-login target picker: the state where the user is authenticated and
// holds the session slot, but no connection has started yet (see
// useRemoteDesktop's "picker" mode). It lists the `[[targets]]` profiles from
// GET /api/targets and starts a session against the one the user picks.
//
// `connect` sends the pick over the live socket; `pendingTarget` is the profile
// a pick is waiting on (buttons lock until the server answers). `connectError`
// carries a failed connect's message so it shows here rather than on a
// dead-end screen. `onLogout` ends the web login; `onUnauthorized` fires if the
// target list itself comes back 401 (the login expired).

interface TargetInfo {
  name: string;
  protocol: string;
  // The target's `subtype` where it has one, null otherwise. Shown because three
  // entries in this list can say `vnc` and mean a plain server, a Mac sharing its
  // physical displays, and a Mac on one virtual display it will disable them for —
  // which is a difference somebody is choosing between here, not discovering after
  // connecting. See connectionLabel.ts.
  subtype: string | null;
  host: string;
  port: number;
}

export default function TargetPicker({
  branding,
  connect,
  pendingTarget,
  connectError,
  audioByDefault,
  onAudioByDefaultChange,
  onLogout,
  onUnauthorized,
}: {
  branding: string;
  connect: (name: string) => void;
  pendingTarget: string | null;
  connectError: string | null;
  // The remembered default, shown here as the one place it can be set before a
  // target is picked — and the same value the desktop menu's live control
  // edits. "… if compatible" is deliberately not a per-target check: the
  // picker never learns a target's capabilities (GET /api/targets carries none),
  // so this is an intent, applied on connect only where `connected` reports the
  // target can honour it.
  audioByDefault: boolean;
  onAudioByDefaultChange: (enabled: boolean) => void;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const [targets, setTargets] = useState<TargetInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    gatewayFetch("/api/targets")
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
                      : `${connectionShortLabel(t.protocol, t.subtype)} · ${t.host}:${t.port}`}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
        {/* The remembered default, applied to whatever is picked above only
            where the target supports it — hence "if compatible", a fixed caption
            rather than a per-target check the picker has no way to make. */}
        <div className="picker-defaults">
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
        <div className="app-version">v{__APP_VERSION__}</div>
      </div>
    </div>
  );
}
