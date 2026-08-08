import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  type CompanionCommandHandlers,
  useCompanionCommands,
  useCompanionState,
} from "./companion.ts";
import FloatingMenu, { type PanelControls } from "./FloatingMenu.tsx";
import {
  NATIVE_HOST,
  type NativeCommandHandlers,
  type NativeState,
  useNativeCommands,
  useNativeState,
} from "./nativeHost.ts";
import TargetPicker from "./TargetPicker.tsx";
import {
  CAN_PINCH_ZOOM,
  type ConnectionStatus,
  useRemoteDesktop,
} from "./useRemoteDesktop.ts";

const STATUS_LABEL: Record<ConnectionStatus, string> = {
  connecting: "Connecting…",
  connected: "Connected",
  reconnecting: "Reconnecting…",
  busy: "Session in use",
  takenOver: "Session taken over",
  failed: "Cannot open the session",
};

export default function RemoteDesktop({
  branding,
  onLogout,
  onUnauthorized,
}: {
  /** Deployment display name shown on the interstitials. */
  branding: string;
  onLogout: () => void;
  onUnauthorized: () => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const pointerRef = useRef<HTMLImageElement>(null);
  const {
    status,
    mode,
    connectError,
    pendingTarget,
    size,
    hostScale,
    renderPlan,
    connection,
    canClipboard,
    canAudio,
    audioEnabled,
    audioError,
    videoError,
    audioByDefault,
    setAudioByDefault,
    displays,
    activeDisplayId,
    remoteClipboard,
    macKeyOverridesEnabled,
    macKeyOverridesActive,
    isMacHost,
    remoteIsMac,
    setMacKeyOverridesEnabled,
    onLocalShortcut,
    takeOver,
    retry,
    connect,
    switchTarget,
    selectDisplay,
    refresh,
    setAudio,
    sendKeyCombo,
    requestClipboard,
    sendClipboard,
    pushLocalClipboard,
    setBottomInset,
  } = useRemoteDesktop(canvasRef, overlayRef, pointerRef, onUnauthorized);

  // The panel actions, published by FloatingMenu and called by the native menu
  // bar. A ref because the menus reach in at any moment, not as part of a render.
  const panelControlsRef = useRef<PanelControls | null>(null);
  const onPanelControls = useCallback((controls: PanelControls | null) => {
    panelControlsRef.current = controls;
  }, []);

  // Whether the caret is in something typeable, which is the shell's cue to take
  // its menu shortcuts back so ⌘V reaches the field instead of the guest.
  //
  // One listener on the document rather than a prop on every input: the panels are
  // the client's and may grow another text box at any time, and a field that
  // forgot to report itself would be one the user cannot paste into.
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    if (!NATIVE_HOST) {
      return;
    }
    const update = () => {
      const el = document.activeElement;
      setEditing(
        el instanceof HTMLElement &&
          (el.isContentEditable ||
            el instanceof HTMLInputElement ||
            el instanceof HTMLTextAreaElement),
      );
    };
    update();
    document.addEventListener("focusin", update);
    document.addEventListener("focusout", update);
    return () => {
      document.removeEventListener("focusin", update);
      document.removeEventListener("focusout", update);
    };
  }, []);

  // One description of this session, for both hosts. `remotex.app` renders its menu
  // bar from it; the companion extension's popup renders its status lines and its
  // Resize to display arithmetic from it. Deliberately not two objects: it is a dozen
  // values that change together, and a second, smaller one would be a second thing to
  // keep in step for the sake of fields one host happens not to read yet.
  //
  // With neither host present it is built and dropped; both hooks post nothing.
  const hostState: NativeState = {
    branding,
    mode,
    status,
    ready: size !== null,
    editing,
    size,
    canClipboard,
    canAudio,
    audioEnabled,
    audioError,
    displays,
    activeDisplayId,
    macKeyOverridesEnabled,
    macKeyOverridesActive,
    remoteIsMac,
  };
  useNativeState(hostState);
  useCompanionState(hostState);

  // The other direction. Every one of these is a control the shell hides: the
  // menu item is the button, and this is the wire between them.
  const nativeCommands: NativeCommandHandlers = useMemo(
    () => ({
      clipboardLocal: ({ text }) => pushLocalClipboard(text),
      openClipboard: () => panelControlsRef.current?.openClipboard(),
      openSessionInfo: () => panelControlsRef.current?.openHelp(),
      openDisplays: () => panelControlsRef.current?.toggleDisplays(),
      closePanel: () => panelControlsRef.current?.closePanel(),
      selectDisplay: ({ id }) => selectDisplay(id),
      setAudio: ({ enabled }) => setAudio(enabled),
      setMacKeyOverrides: ({ enabled }) => setMacKeyOverridesEnabled(enabled),
      refresh: () => refresh(),
      switchTarget: () => switchTarget(),
      takeOver: () => takeOver(),
      sendKeyCombo: ({ codes }) => sendKeyCombo(codes),
    }),
    [
      pushLocalClipboard,
      selectDisplay,
      setAudio,
      setMacKeyOverridesEnabled,
      refresh,
      switchTarget,
      takeOver,
      sendKeyCombo,
    ],
  );
  useNativeCommands(nativeCommands);

  // The companion's whole command surface, and the contrast with the table above is
  // the design of both seams. The shell hides the floating menu, so every control it
  // hides has to come back as a menu item and a command. The extension hides nothing —
  // the menu is on screen under it — so the only thing it sends is the one a page
  // cannot get for itself, and it goes to the same callback the menu bar's version
  // calls.
  const companionCommands: CompanionCommandHandlers = useMemo(
    () => ({
      // The handshake is recorded by the store in companion.ts; nothing here reacts
      // to it, and the entry exists so the handler table stays exhaustive.
      hello: () => {},
      clipboardLocal: ({ text }) => pushLocalClipboard(text),
    }),
    [pushLocalClipboard],
  );
  useCompanionCommands(companionCommands);

  // A speaker on the tab title while sound is playing — the one place the desktop
  // has room to say so, since the toggle lives in the drawer. At the *front*, not
  // the end: a tab title is truncated from the right, so a suffix is the first
  // thing to vanish. Desktop only, so the picker's tab stays the plain branding.
  useEffect(() => {
    document.title =
      mode === "desktop" && audioEnabled ? `🔊 ${branding}` : branding;
  }, [mode, audioEnabled, branding]);

  // The status overlay covers the connection lifecycle (connecting/reconnecting)
  // and the claim conflicts (busy/takenOver); in the desktop it also covers the
  // gap before the first frame. The picker owns the screen once connected.
  const showStatus = status !== "connected" || (mode === "desktop" && !size);

  return (
    /* screen-touch swaps native scrolling for the gesture transform
       (pinch zoom + pan) and stretches the input overlay over the whole
       viewport so gestures land everywhere — see index.css. */
    <div className={`screen${CAN_PINCH_ZOOM ? " screen-touch" : ""}`}>
      <div className="surface">
        {/* Starts 0×0 so no ghost block shows before the first resize; the
            resize handler keeps the full pixel bitmap separate from its
            remote-point CSS size. Kept
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
      {mode === "desktop" && (
        <FloatingMenu
          onLogout={onLogout}
          onSwitchTarget={switchTarget}
          sendKeyCombo={sendKeyCombo}
          onKeyboardInset={setBottomInset}
          canClipboard={canClipboard}
          remoteClipboard={remoteClipboard}
          onFetchClipboard={requestClipboard}
          onSendClipboard={sendClipboard}
          displays={displays}
          activeDisplayId={activeDisplayId}
          onSelectDisplay={selectDisplay}
          size={size}
          hostScale={hostScale}
          connection={connection}
          renderPlan={renderPlan}
          canAudio={canAudio}
          audioEnabled={audioEnabled}
          audioError={audioError}
          onAudioChange={setAudio}
          macKeyOverridesEnabled={macKeyOverridesEnabled}
          macKeyOverridesActive={macKeyOverridesActive}
          isMacHost={isMacHost}
          remoteIsMac={remoteIsMac}
          onMacKeyOverridesChange={setMacKeyOverridesEnabled}
          onLocalShortcut={onLocalShortcut}
          chromeless={NATIVE_HOST}
          onPanelControls={NATIVE_HOST ? onPanelControls : undefined}
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
          audioByDefault={audioByDefault}
          onAudioByDefaultChange={setAudioByDefault}
          onLogout={onLogout}
          onUnauthorized={onUnauthorized}
        />
      )}

      {/* A video target this browser cannot decode. Its own banner rather than a
          line in the status overlay, because the overlay hides itself the moment the
          session is up — and this is a session that *is* up, showing nothing. It is
          also not `connectError`: nothing is wrong with the session or the gateway,
          it is this browser that cannot decode what is arriving. */}
      {videoError && mode === "desktop" && (
        <div className="video-banner" role="alert">
          {videoError}
        </div>
      )}

      {showStatus && (
        <div className="status-overlay">
          <span className="status-brand">{branding}</span>
          <span className={`status status-${status}`}>
            {STATUS_LABEL[status]}
          </span>
          {/* Why the session is not up, when the reason is known. "Reconnecting…"
              is true and unhelpful next to "the server answered 502", and the
              picker is not on screen to carry it while the overlay is. */}
          {connectError && <span className="status-hint">{connectError}</span>}
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
          {status === "failed" && (
            <button type="button" className="status-action" onClick={retry}>
              Retry
            </button>
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
