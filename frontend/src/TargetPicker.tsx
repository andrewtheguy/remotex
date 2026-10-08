import { useEffect, useState } from "react";
import { AppVersion } from "./AppVersion.tsx";
import { decodesAppleMedia } from "./appleMedia.ts";
import { connectionShortLabel } from "./connectionLabel.ts";
import { gatewayFetch } from "./gateway.ts";
import { versionMismatch } from "./gatewayVersion.ts";
import { composesRdpGraphics } from "./rdpGraphics.ts";
import { runnableDecoders } from "./softwareSupport.ts";
import ThroughputPanel, { useThroughputAvailable } from "./ThroughputPanel.tsx";
import {
  type Choices,
  readRememberedChoices,
  rememberChoice,
  type TargetInfo,
  targetOptions,
} from "./targetChoices.ts";
import { sizeFollows } from "./useRemoteDesktop.ts";
import { videoChroma } from "./videoChroma.ts";

// The post-login target picker: the state where the user is authenticated and
// holds the session slot, but no connection has started yet (see
// useRemoteDesktop's "picker" mode). It lists the `[[targets]]` profiles from
// GET /api/targets. Picking one opens it rather than connecting: what the
// session is started with — its size, sound, a passthrough — is chosen under it,
// and Start is what connects (targetChoices.ts has the rules for which options
// a target shows and which are greyed). The size the desktop will have is shown
// there before Start, as a choice where there are two. Every target starts
// closed, a gateway's only one included, so starting a session is the same two
// steps everywhere.
//
// `connect` sends the Start over the live socket with the choices; `sound` tells
// it the session will carry the remote's sound, so the click is spent on an audio
// context. `pendingTarget` is the profile a Start is waiting on (buttons lock
// until the server answers). `connectError` carries a failed connect's message so
// it shows here rather than on a dead-end screen. `onLogout` ends the web login;
// `onUnauthorized` fires if the target list itself comes back 401 (the login
// expired).

export default function TargetPicker({
  branding,
  connect,
  pendingTarget,
  connectError,
  onLogout,
  onUnauthorized,
}: {
  branding: string;
  connect: (name: string, choices: Choices, sound: boolean) => void;
  pendingTarget: string | null;
  connectError: string | null;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const [targets, setTargets] = useState<TargetInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  // Set when the list came from a gateway of another version than this page's
  // (gatewayVersion.ts): what to say, beside a Reload.
  const [stale, setStale] = useState<string | null>(null);
  // The one target whose options are showing, by name.
  const [openTarget, setOpenTarget] = useState<string | null>(null);
  // What was chosen under each target the last time, in this browser. It
  // replaces the one sound checkbox the picker used to have: the choice is each
  // target's, and it is made where the target is.
  const [remembered, setRemembered] = useState(readRememberedChoices);
  // Offered only on a gateway with `[meter].enabled`; the view replaces the list while open.
  const throughputAvailable = useThroughputAvailable();
  const [showThroughput, setShowThroughput] = useState(false);

  useEffect(() => {
    let cancelled = false;
    gatewayFetch("/api/targets")
      .then((res) => {
        // Another gateway than this page's: its targets are not listed, because
        // nothing this page would start on them is that gateway's to answer.
        // Asked of its 401 too, and first: the reload comes before the login.
        const mismatch = versionMismatch(res);
        if (mismatch) {
          if (!cancelled) {
            setStale(mismatch);
          }
          return null;
        }
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

  if (showThroughput) {
    return (
      <div className="picker-screen">
        <div className="picker-panel throughput-panel">
          <span className="picker-brand">{branding}</span>
          <ThroughputPanel
            closeLabel="Back to targets"
            onClose={() => setShowThroughput(false)}
            onUnauthorized={onUnauthorized}
          />
        </div>
      </div>
    );
  }

  // What this browser can take, asked once at load (main.tsx) and stated on its
  // session socket too: the gateway refuses what this greys. And what of it a
  // desktop can follow, which decides the sizes a target offers here.
  const abilities = {
    appleMedia: decodesAppleMedia(),
    profile1: videoChroma() === "444",
    rdpGraphics: composesRdpGraphics(),
    runs: runnableDecoders(),
    follows: sizeFollows(),
  };

  return (
    <div className="picker-screen">
      <div className="picker-panel">
        <span className="picker-brand">{branding}</span>
        <h1>Pick a target</h1>
        {connectError && <p className="picker-error">{connectError}</p>}
        {loadError && <p className="picker-error">{loadError}</p>}
        {stale && (
          <>
            <p className="picker-error">{stale}</p>
            <button
              type="button"
              className="picker-start"
              onClick={() => location.reload()}
            >
              Reload
            </button>
          </>
        )}
        {targets === null && !loadError && !stale && (
          <p className="picker-hint">Loading targets…</p>
        )}
        {targets?.length === 0 && (
          <p className="picker-hint">No targets are configured.</p>
        )}
        <ul className="picker-list">
          {targets?.map((t) => {
            const connecting = pendingTarget === t.name;
            const open = openTarget === t.name;
            const options = targetOptions(t, remembered[t.name], abilities);
            const optionsId = `picker-options-${t.name}`;
            return (
              <li key={t.name}>
                <button
                  type="button"
                  className="picker-target"
                  aria-expanded={open}
                  aria-controls={optionsId}
                  onClick={() => setOpenTarget(open ? null : t.name)}
                  disabled={pendingTarget !== null}
                >
                  <span className="picker-chevron" aria-hidden="true">
                    {open ? "∨" : "›"}
                  </span>
                  <span className="picker-target-text">
                    <span className="picker-target-name">{t.name}</span>
                    <span className="picker-target-meta">
                      {connecting
                        ? "Connecting…"
                        : [
                            connectionShortLabel(t.protocol, t.subtype),
                            `${t.host}:${t.port}`,
                          ].join(" · ")}
                    </span>
                  </span>
                </button>
                {open && (
                  <div className="picker-options" id={optionsId}>
                    {/* The size the desktop will have: a choice where the
                        target offers two here, and stated where it has one. */}
                    {options.sizes.length > 1 ? (
                      <fieldset className="picker-choice">
                        <legend>Size</legend>
                        {options.sizes.map((size) => (
                          <label key={size.value} className="picker-option">
                            <input
                              type="radio"
                              name={`picker-size-${t.name}`}
                              checked={options.choices.size === size.value}
                              disabled={pendingTarget !== null}
                              onChange={() =>
                                setRemembered((was) =>
                                  rememberChoice(
                                    was,
                                    t.name,
                                    "size",
                                    size.value,
                                  ),
                                )
                              }
                            />
                            <span className="picker-option-text">
                              <span>{size.label}</span>
                              <span className="picker-option-note">
                                {size.note}
                              </span>
                            </span>
                          </label>
                        ))}
                      </fieldset>
                    ) : (
                      <p className="picker-choice">
                        <span className="picker-choice-heading">Size</span>
                        <span className="picker-option-text">
                          <span>{options.sizes[0].label}</span>
                          <span className="picker-option-note">
                            {options.sizes[0].note}
                          </span>
                        </span>
                      </p>
                    )}
                    {/* The remote's sound, where the target offers it: ticked or
                        not, and under a ticked one the format it is sent as. */}
                    {options.soundRow && (
                      <>
                        <label className="picker-option">
                          <input
                            type="checkbox"
                            checked={options.soundRow.checked}
                            disabled={pendingTarget !== null}
                            onChange={(e) =>
                              setRemembered((was) =>
                                rememberChoice(
                                  was,
                                  t.name,
                                  "audio",
                                  e.target.checked ? "opus" : "off",
                                ),
                              )
                            }
                          />
                          <span className="picker-option-text">
                            <span>{options.soundRow.label}</span>
                            <span className="picker-option-note">
                              {options.soundRow.note}
                            </span>
                          </span>
                        </label>
                        {options.soundRow.checked && (
                          <div
                            className="picker-sound-formats"
                            role="radiogroup"
                            aria-label="Sound format"
                          >
                            {options.soundRow.formats.map((format) => (
                              <label
                                key={format.value}
                                className="picker-option"
                              >
                                <input
                                  type="radio"
                                  name={`picker-sound-${t.name}`}
                                  checked={
                                    options.choices.audio === format.value
                                  }
                                  disabled={pendingTarget !== null}
                                  onChange={() =>
                                    setRemembered((was) =>
                                      rememberChoice(
                                        was,
                                        t.name,
                                        "audio",
                                        format.value,
                                      ),
                                    )
                                  }
                                />
                                <span className="picker-option-text">
                                  <span>{format.label}</span>
                                  <span className="picker-option-note">
                                    {format.note}
                                  </span>
                                </span>
                              </label>
                            ))}
                          </div>
                        )}
                      </>
                    )}
                    {/* Where the second of two virtual displays sits, on a
                        target whose host is told. */}
                    {options.placements && (
                      <fieldset className="picker-choice">
                        <legend>Second display</legend>
                        <div className="picker-placements">
                          {options.placements.map((placement) => (
                            <label
                              key={placement.value}
                              className="picker-option"
                            >
                              <input
                                type="radio"
                                name={`picker-placement-${t.name}`}
                                checked={
                                  options.choices.placement === placement.value
                                }
                                disabled={pendingTarget !== null}
                                onChange={() =>
                                  setRemembered((was) =>
                                    rememberChoice(
                                      was,
                                      t.name,
                                      "placement",
                                      placement.value,
                                    ),
                                  )
                                }
                              />
                              <span>{placement.label}</span>
                            </label>
                          ))}
                        </div>
                      </fieldset>
                    )}
                    {/* Only what the target's type offers has a row; one that
                        cannot be had here is greyed and says why. */}
                    {options.rows.map((row) => (
                      <label
                        key={row.key}
                        className={`picker-option${row.disabled ? " picker-option-unavailable" : ""}`}
                      >
                        <input
                          type="checkbox"
                          checked={row.checked}
                          disabled={row.disabled || pendingTarget !== null}
                          onChange={(e) =>
                            setRemembered((was) =>
                              rememberChoice(
                                was,
                                t.name,
                                row.key,
                                e.target.checked,
                              ),
                            )
                          }
                        />
                        <span className="picker-option-text">
                          <span>{row.label}</span>
                          <span className="picker-option-note">{row.note}</span>
                        </span>
                      </label>
                    ))}
                    {options.blocked && (
                      <p className="picker-error">{options.blocked}</p>
                    )}
                    <button
                      type="button"
                      className="picker-start"
                      onClick={() =>
                        connect(t.name, options.choices, options.sound)
                      }
                      disabled={
                        pendingTarget !== null || options.blocked !== null
                      }
                    >
                      {connecting ? "Connecting…" : "Start"}
                    </button>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
        {throughputAvailable && (
          <button
            type="button"
            className="picker-logout"
            onClick={() => setShowThroughput(true)}
            disabled={pendingTarget !== null}
          >
            Throughput
          </button>
        )}
        <button
          type="button"
          className="picker-logout"
          onClick={onLogout}
          disabled={pendingTarget !== null}
        >
          Log out
        </button>
        <AppVersion className="app-version" />
      </div>
    </div>
  );
}
