import { useEffect, useState } from "react";

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
  host: string;
  port: number;
}

export default function TargetPicker({
  connect,
  pendingTarget,
  connectError,
  onLogout,
  onUnauthorized,
}: {
  connect: (name: string) => void;
  pendingTarget: string | null;
  connectError: string | null;
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
                      : `${t.protocol.toUpperCase()} · ${t.host}:${t.port}`}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
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
