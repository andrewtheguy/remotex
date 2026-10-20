import { useEffect, useState } from "react";
import { AppVersion } from "./AppVersion.tsx";
import { decodesAppleMedia } from "./appleMedia.ts";
import { connectionShortLabel } from "./connectionLabel.ts";
import { decodesFlac } from "./flacSound.ts";
import { gatewayFetch } from "./gateway.ts";
import { versionMismatch } from "./gatewayVersion.ts";
import { decodesAudio } from "./preflight.ts";
import { composesRdpGraphics } from "./rdpGraphics.ts";
import { decodesRdpH264 } from "./rdpH264.ts";
import { runnableDecoders } from "./softwareSupport.ts";
import ThroughputPanel, { useThroughputAvailable } from "./ThroughputPanel.tsx";
import {
  type Choices,
  type DeviceRow,
  type PictureOption,
  type Pipeline,
  type PipelineRow,
  readRememberedChoices,
  rememberChoice,
  type Sound,
  type SoundRow,
  type TargetInfo,
  targetOptions,
  type ViewOption,
} from "./targetChoices.ts";
import { sizeFollows } from "./useRemoteDesktop.ts";
import { decodesAppleH264, decodesVp9 } from "./videoCodecs.ts";

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
    rdpGraphics: composesRdpGraphics(),
    rdpH264: decodesRdpH264(),
    appleH264: decodesAppleH264(),
    vp9: decodesVp9(),
    audio: decodesAudio(),
    lossless: decodesFlac(),
    runsHevc: runnableDecoders().hevc,
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
                    {/* The remote's sound, where the target offers it. */}
                    {options.soundRow && (
                      <SoundChoice
                        target={t.name}
                        row={options.soundRow}
                        chosen={options.choices.audio}
                        locked={pendingTarget !== null}
                        choose={(audio) =>
                          setRemembered((was) =>
                            rememberChoice(was, t.name, "audio", audio),
                          )
                        }
                      />
                    )}
                    {/* This browser's camera and microphone, offered to the
                        remote where the target has somewhere to put them. */}
                    <DeviceChoices
                      camera={options.cameraRow}
                      microphone={options.microphoneRow}
                      locked={pendingTarget !== null}
                      choose={(device, offered) =>
                        setRemembered((was) =>
                          rememberChoice(was, t.name, device, offered),
                        )
                      }
                    />
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
                    {/* A stream passed in every session is said, not
                        offered. */}
                    {options.passed && (
                      <p className="picker-choice">
                        <span className="picker-choice-heading">
                          Passthrough
                        </span>
                        <span className="picker-option-text">
                          <span>{options.passed.label}</span>
                          <span className="picker-option-note">
                            {options.passed.note}
                          </span>
                        </span>
                      </p>
                    )}
                    {/* How the host draws the pipeline, on an RDP target
                        with one. */}
                    {options.pipelineRow && (
                      <PipelineChoice
                        target={t.name}
                        row={options.pipelineRow}
                        chosen={options.choices.pipeline}
                        locked={pendingTarget !== null}
                        choose={(pipeline) =>
                          setRemembered((was) =>
                            rememberChoice(was, t.name, "pipeline", pipeline),
                          )
                        }
                      />
                    )}
                    {/* How a Standard Mac's screens are shown: Screen
                        Sharing Standard's alone. */}
                    <ViewChoice
                      target={t.name}
                      views={options.views}
                      chosen={options.choices.standardView}
                      locked={pendingTarget !== null}
                      choose={(view) =>
                        setRemembered((was) =>
                          rememberChoice(was, t.name, "standardView", view),
                        )
                      }
                    />
                    {/* How the target's picture reaches this browser, where
                        it has one to choose: a Mac's or a wlshare desktop's. */}
                    <PictureChoice
                      target={t.name}
                      pictures={options.pictures}
                      chosen={options.choices.passthrough}
                      locked={pendingTarget !== null}
                      choose={(passthrough) =>
                        setRemembered((was) =>
                          rememberChoice(
                            was,
                            t.name,
                            "passthrough",
                            passthrough,
                          ),
                        )
                      }
                    />
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

// How a Standard Mac's screens are shown: one at a time or Combined Display.
// Screen Sharing Standard's alone; nothing on any other target.
function ViewChoice({
  target,
  views,
  chosen,
  locked,
  choose,
}: {
  target: string;
  views: ViewOption[] | null;
  chosen: Choices["standardView"];
  locked: boolean;
  choose: (view: ViewOption["value"]) => void;
}) {
  if (!views) {
    return null;
  }
  return (
    <fieldset className="picker-choice">
      <legend>Displays</legend>
      {views.map((view) => (
        <label key={view.value} className="picker-option">
          <input
            type="radio"
            name={`picker-view-${target}`}
            checked={chosen === view.value}
            disabled={locked}
            onChange={() => choose(view.value)}
          />
          <span className="picker-option-text">
            <span>{view.label}</span>
            <span className="picker-option-note">{view.note}</span>
          </span>
        </label>
      ))}
    </fieldset>
  );
}

// How a Mac's or a wlshare desktop's picture reaches this browser: which of its
// own streams is passed through. One that cannot be had here is greyed, with the
// reason. Nothing on a target with no such choice.
function PictureChoice({
  target,
  pictures,
  chosen,
  locked,
  choose,
}: {
  target: string;
  pictures: PictureOption[] | null;
  chosen: Choices["passthrough"];
  locked: boolean;
  choose: (passthrough: PictureOption["value"]) => void;
}) {
  if (!pictures) {
    return null;
  }
  return (
    <fieldset className="picker-choice">
      <legend>Picture</legend>
      {pictures.map((picture) => (
        <label
          key={picture.value ?? "tiles"}
          className={`picker-option${picture.disabled ? " picker-option-unavailable" : ""}`}
        >
          <input
            type="radio"
            name={`picker-picture-${target}`}
            checked={chosen === picture.value}
            disabled={picture.disabled || locked}
            onChange={() => choose(picture.value)}
          />
          <span className="picker-option-text">
            <span>{picture.label}</span>
            <span className="picker-option-note">{picture.note}</span>
          </span>
        </label>
      ))}
    </fieldset>
  );
}

// A target's sound under it: ticked or not, and under a ticked one the format it
// is sent as. A format this browser cannot play is greyed, with the reason.
function SoundChoice({
  target,
  row,
  chosen,
  locked,
  choose,
}: {
  target: string;
  row: SoundRow;
  chosen: Sound;
  locked: boolean;
  choose: (audio: Sound) => void;
}) {
  return (
    <>
      <label className="picker-option">
        <input
          type="checkbox"
          checked={row.checked}
          disabled={row.disabled || locked}
          onChange={(e) => choose(e.target.checked ? "opus" : "off")}
        />
        <span className="picker-option-text">
          <span>{row.label}</span>
          <span className="picker-option-note">{row.note}</span>
        </span>
      </label>
      {row.checked && (
        <div
          className="picker-suboptions"
          role="radiogroup"
          aria-label="Sound format"
        >
          {row.formats.map((format) => (
            <label
              key={format.value}
              className={`picker-option${format.disabled ? " picker-option-unavailable" : ""}`}
            >
              <input
                type="radio"
                name={`picker-sound-${target}`}
                checked={chosen === format.value}
                disabled={format.disabled || locked}
                onChange={() => choose(format.value)}
              />
              <span className="picker-option-text">
                <span>{format.label}</span>
                <span className="picker-option-note">{format.note}</span>
              </span>
            </label>
          ))}
        </div>
      )}
    </>
  );
}

// This browser's devices, offered to the remote or not: the camera and the
// microphone, each a tick where the target has somewhere to put it, and nothing
// where it has not. The tick is the offer; the session's menu turns the device on.
function DeviceChoices({
  camera,
  microphone,
  locked,
  choose,
}: {
  camera: DeviceRow | null;
  microphone: DeviceRow | null;
  locked: boolean;
  choose: (device: "camera" | "microphone", offered: boolean) => void;
}) {
  const rows = [
    ["camera", camera],
    ["microphone", microphone],
  ] as const;
  return rows.map(([device, row]) =>
    row ? (
      <label key={device} className="picker-option">
        <input
          type="checkbox"
          checked={row.checked}
          disabled={locked}
          onChange={(e) => choose(device, e.target.checked)}
        />
        <span className="picker-option-text">
          <span>{row.label}</span>
          <span className="picker-option-note">{row.note}</span>
        </span>
      </label>
    ) : null,
  );
}

/**
 * How the host draws the pipeline: high quality or adaptive, and under a chosen
 * adaptive one its H.264 over the full screen or by region. Choosing adaptive
 * takes the full screen.
 */
function PipelineChoice({
  target,
  row,
  chosen,
  locked,
  choose,
}: {
  target: string;
  row: PipelineRow;
  chosen: Pipeline;
  locked: boolean;
  choose: (pipeline: Pipeline) => void;
}) {
  const adaptive = chosen !== "high-quality";
  return (
    <fieldset className="picker-choice">
      <legend>Graphics</legend>
      {row.modes.map((mode) => (
        <label
          key={mode.value}
          className={`picker-option${mode.disabled ? " picker-option-unavailable" : ""}`}
        >
          <input
            type="radio"
            name={`picker-pipeline-${target}`}
            checked={(mode.value === "adaptive") === adaptive}
            disabled={mode.disabled || locked}
            onChange={() =>
              choose(
                mode.value === "adaptive" ? "adaptive-desktop" : "high-quality",
              )
            }
          />
          <span className="picker-option-text">
            <span>{mode.label}</span>
            <span className="picker-option-note">{mode.note}</span>
          </span>
        </label>
      ))}
      {adaptive && (
        <div className="picker-suboptions" role="radiogroup" aria-label="H.264">
          {row.h264.map((option) => (
            <label key={option.value} className="picker-option">
              <input
                type="radio"
                name={`picker-h264-${target}`}
                checked={chosen === option.value}
                disabled={option.disabled || locked}
                onChange={() => choose(option.value)}
              />
              <span className="picker-option-text">
                <span>{option.label}</span>
                <span className="picker-option-note">{option.note}</span>
              </span>
            </label>
          ))}
        </div>
      )}
    </fieldset>
  );
}
