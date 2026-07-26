import { useEffect, useRef } from "react";
import FloatingMenu from "./FloatingMenu.tsx";
import {
  type NativeCommand,
  postNativeHostEvent,
  setNativeCommandHandler,
} from "./nativeHost.ts";
import TargetPicker from "./TargetPicker.tsx";
import {
  CAN_PINCH_ZOOM,
  type ConnectionStatus,
  useRemoteDesktop,
} from "./useRemoteDesktop.ts";

// Why a native command cannot run right now, or null when it can. Kept apart
// from the dispatch below so each stays readable: this is the only place a
// native command is refused, and the switch there is then a plain mapping.
//
// The viewer disables the matching menu items from the same state, so a refusal
// means its picture of the session went stale — a target switch mid-menu, say.
function unavailable(
  command: NativeCommand,
  caps: { canClipboard: boolean; canResize: boolean; hasModes: boolean },
): string | null {
  switch (command.type) {
    case "clipboard":
    case "clipboardRequest":
      return caps.canClipboard ? null : "clipboard is disabled for this target";
    case "resize":
      return caps.canResize ? null : "resize is unavailable";
    case "setResolution":
      return caps.hasModes ? null : "this target offers no resolution menu";
    default:
      return null;
  }
}

const STATUS_LABEL: Record<ConnectionStatus, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  busy: "Session in use",
  takenOver: "Session taken over",
};

export default function RemoteDesktop({
  branding,
  nativeHost,
  onLogout,
  onUnauthorized,
}: {
  /** Deployment display name shown on the interstitials. */
  branding: string;
  /** True only after the macOS viewer and this frontend agree on the bridge. */
  nativeHost: boolean;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const pointerRef = useRef<HTMLImageElement>(null);
  const {
    status,
    mode,
    connectedTarget,
    remoteIsMac,
    connectError,
    pendingTarget,
    size,
    canResize,
    displayModes,
    canClipboard,
    remoteClipboard,
    remoteClipboardPush,
    takeOver,
    connect,
    switchTarget,
    resizeToWindow,
    setResolution,
    sendKeyCombo,
    sendNativeKey,
    releaseNativeKeys,
    requestClipboard,
    sendClipboard,
    setBottomInset,
  } = useRemoteDesktop(
    canvasRef,
    overlayRef,
    pointerRef,
    onUnauthorized,
    nativeHost,
  );

  // The status overlay covers the connection lifecycle (connecting/reconnecting)
  // and the claim conflicts (busy/takenOver); in the desktop it also covers the
  // gap before the first frame. The picker owns the screen once connected.
  const showStatus = status !== "connected" || (mode === "desktop" && !size);

  useEffect(() => {
    if (!nativeHost) {
      return;
    }
    postNativeHostEvent({
      type: "state",
      state: {
        screen: mode,
        connectionStatus: status,
        connectedTarget,
        remoteIsMac,
        displayModes,
        remoteSize: size,
        canResize,
        canClipboard,
        canCaptureKeyboard: status === "connected" && mode === "desktop",
      },
    });
  }, [
    canClipboard,
    canResize,
    connectedTarget,
    displayModes,
    mode,
    nativeHost,
    remoteIsMac,
    size,
    status,
  ]);

  useEffect(() => {
    if (!nativeHost || !remoteClipboardPush) {
      return;
    }
    postNativeHostEvent({
      type: "remoteClipboard",
      text: remoteClipboardPush.text,
      changedAtMs: remoteClipboardPush.changedAtMs,
    });
  }, [nativeHost, remoteClipboardPush]);

  useEffect(() => {
    if (!nativeHost) {
      return;
    }
    return setNativeCommandHandler((command: NativeCommand) => {
      const refusal = unavailable(command, {
        canClipboard,
        canResize,
        hasModes: displayModes.length > 0,
      });
      if (refusal) {
        return { ok: false, error: refusal };
      }
      switch (command.type) {
        case "key":
          sendNativeKey(command.code, command.pressed, command.caps);
          return { ok: true };
        case "releaseKeys":
          releaseNativeKeys();
          return { ok: true };
        case "clipboard":
          sendClipboard(command.text);
          return { ok: true };
        case "clipboardRequest":
          void requestClipboard().then((snapshot) => {
            if (!snapshot) {
              return;
            }
            postNativeHostEvent({
              type: "clipboardFetchResult",
              requestId: command.requestId,
              text: snapshot.text,
              changedAtMs: snapshot.changedAtMs,
            });
          });
          return { ok: true };
        case "resize":
          resizeToWindow();
          return { ok: true };
        case "setResolution":
          setResolution(command.w, command.h);
          return { ok: true };
        case "switchTarget":
          switchTarget();
          return { ok: true };
        case "logout":
          onLogout();
          return { ok: true };
        case "takeOver":
          takeOver();
          return { ok: true };
      }
    });
  }, [
    canClipboard,
    canResize,
    displayModes,
    nativeHost,
    onLogout,
    releaseNativeKeys,
    requestClipboard,
    resizeToWindow,
    sendClipboard,
    sendNativeKey,
    setResolution,
    switchTarget,
    takeOver,
  ]);

  return (
    /* screen-touch swaps native scrolling for the gesture transform
       (pinch zoom + pan) and stretches the input overlay over the whole
       viewport so gestures land everywhere — see index.css. */
    <div className={`screen${CAN_PINCH_ZOOM ? " screen-touch" : ""}`}>
      <div className="surface">
        {/* Starts 0×0 so no ghost block shows before the first resize; the
            resize handler sets the pixel size and the 1:1 CSS size. Kept
            mounted in both modes so the hook's canvas ref stays stable. */}
        <canvas ref={canvasRef} className="framebuffer" width={0} height={0} />
        {/* Transparent overlay captures mouse + keyboard input. tabIndex
            makes the div focusable — without it, focus() in the mousedown
            handler is a no-op and the keydown/keyup listeners (scoped to
            the focused overlay, not window) never fire. */}
        <div
          ref={overlayRef}
          className="input-overlay"
          role="application"
          // biome-ignore lint/a11y/noNoninteractiveTabindex: the remote-desktop surface (role=application) must take focus to receive keyboard input
          tabIndex={0}
        />
        {/* The pointer for the touch gesture layer's virtual cursor, drawn
            only when the engine sends cursor shapes instead of compositing
            them (VNC). Sized and positioned imperatively by the hook; hidden
            by default, and decorative, so it carries no alt text. */}
        <img ref={pointerRef} className="remote-pointer" alt="" />
      </div>

      {/* The floating menu is desktop-only; its Switch target button returns to
          the picker (see FloatingMenu.tsx), and Log out ends the login. */}
      {mode === "desktop" && !nativeHost && (
        <FloatingMenu
          onLogout={onLogout}
          onSwitchTarget={switchTarget}
          onResizeToWindow={canResize ? resizeToWindow : undefined}
          displayModes={displayModes}
          remoteSize={size}
          onSetResolution={setResolution}
          sendKeyCombo={sendKeyCombo}
          onKeyboardInset={setBottomInset}
          canClipboard={canClipboard}
          remoteClipboard={remoteClipboard}
          onFetchClipboard={requestClipboard}
          onSendClipboard={sendClipboard}
        />
      )}

      {/* The post-login target picker: shown once the slot is held and no
          target is connected. */}
      {status === "connected" && mode === "picker" && (
        <TargetPicker
          branding={branding}
          connect={connect}
          pendingTarget={pendingTarget}
          connectError={connectError}
          onLogout={onLogout}
          onUnauthorized={onUnauthorized}
        />
      )}

      {showStatus && (
        <div className="status-overlay">
          <span className="status-brand">{branding}</span>
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
          {status === "connected" && mode === "desktop" && !size && (
            <span className="status-hint">Waiting for the remote desktop…</span>
          )}
          {status === "busy" && (
            <>
              <span className="status-hint">
                This desktop is open in another browser.
              </span>
              <button
                type="button"
                className="status-action"
                onClick={takeOver}
              >
                Take over
              </button>
            </>
          )}
          {status === "takenOver" && (
            <>
              <span className="status-hint">
                Another browser took over this session.
              </span>
              <button
                type="button"
                className="status-action"
                onClick={takeOver}
              >
                Take it back
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
