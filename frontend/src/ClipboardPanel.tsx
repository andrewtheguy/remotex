import { useCallback, useEffect, useRef, useState } from "react";
import { MAX_CLIPBOARD_BYTES } from "./protocol.ts";
import { useIsDesktop } from "./SoftKeyboardPanel.tsx";

// The clipboard bridge's whole interface: a text box with Fetch and Send.
//
// Deliberately manual in both directions. The server owns the clipboard data —
// the VNC engine buffers what the remote last copied, the Mac agent reads its
// pasteboard when asked — so this panel holds nothing that isn't on screen, and
// there is no background sync to reason about. It also never touches the
// *local* OS clipboard: navigator.clipboard needs a permission and a secure
// context (this server is usually plain HTTP on a LAN), and silently reading
// what you copied in another tab is a worse deal than pressing Ctrl+V into a
// text box yourself.
//
// Shown only for targets that opted in (`clipboard = true`); FloatingMenu keeps
// the button disabled otherwise.

// How long the "Fetched"/"Sent" line stays up.
const NOTICE_MS = 2000;

const encoder = new TextEncoder();

interface ClipboardPanelProps {
  // Ask for the remote's clipboard. The reply arrives out of band, as a new
  // `remoteClipboard` — hence the seq counter rather than a promise.
  onFetch: () => void;
  onSend: (text: string) => void;
  // The last reply from the server. `seq` ticks on every reply so re-fetching
  // identical text still counts as an answer.
  remoteClipboard: { text: string; seq: number } | null;
  onClose: () => void;
  // Reports the panel's height (CSS px) while it's docked to the bottom edge
  // (mobile), 0 while it floats (desktop) or when it unmounts — same contract
  // as SoftKeyboardPanel, so the touch canvas can inset above it.
  onDockedHeightChange?: (px: number) => void;
}

export function ClipboardPanel({
  onFetch,
  onSend,
  remoteClipboard,
  onClose,
  onDockedHeightChange,
}: ClipboardPanelProps) {
  // Seeded from the last reply so reopening the panel shows what was last
  // fetched rather than an empty box; lastSeqRef starts there too, so that
  // restored text doesn't announce itself as a fresh fetch.
  const [text, setText] = useState(() => remoteClipboard?.text ?? "");
  const [notice, setNotice] = useState<string | null>(null);
  // Set when Fetch is pressed, cleared by the reply. Distinguishes "waiting"
  // from "the remote's clipboard is genuinely empty", which look identical
  // otherwise — the server answers an empty remote clipboard with "".
  const [awaitingFetch, setAwaitingFetch] = useState(false);
  const panelRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const isDesktop = useIsDesktop();

  // Adopt each reply. Keyed on seq, so fetching the same text twice still
  // replaces the box (and re-shows the notice).
  const seq = remoteClipboard?.seq;
  const lastSeqRef = useRef<number | undefined>(remoteClipboard?.seq);
  useEffect(() => {
    if (seq === undefined || seq === lastSeqRef.current) {
      return;
    }
    lastSeqRef.current = seq;
    setText(remoteClipboard?.text ?? "");
    setAwaitingFetch(false);
    setNotice(
      remoteClipboard?.text
        ? "Fetched from remote"
        : "Remote clipboard is empty",
    );
  }, [seq, remoteClipboard?.text]);

  // Clear the notice a moment after it appears (and on unmount).
  useEffect(() => {
    if (notice === null) {
      return;
    }
    const timer = setTimeout(() => setNotice(null), NOTICE_MS);
    return () => clearTimeout(timer);
  }, [notice]);

  // Same docked-height contract as the soft keyboard: only the bottom-docked
  // mobile panel covers the canvas, so the desktop one reports 0.
  useEffect(() => {
    const report = onDockedHeightChange;
    if (!report) {
      return;
    }
    if (isDesktop) {
      report(0);
      return;
    }
    const panel = panelRef.current;
    if (!panel) {
      return;
    }
    const measure = () => report(panel.getBoundingClientRect().height);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(panel);
    return () => {
      observer.disconnect();
      report(0);
    };
  }, [isDesktop, onDockedHeightChange]);

  // Focus the box on open: the common move is paste-then-send, and the canvas
  // only takes keyboard focus back on a pointer press over it.
  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const handleFetch = useCallback(() => {
    setAwaitingFetch(true);
    setNotice(null);
    onFetch();
  }, [onFetch]);

  const handleSend = useCallback(() => {
    onSend(text);
    setNotice("Sent to remote");
  }, [onSend, text]);

  // Bytes, not characters: the cap is a byte cap on both sides, and a box of
  // emoji hits it four times sooner than the character count suggests.
  const bytes = encoder.encode(text).length;
  const overCap = bytes > MAX_CLIPBOARD_BYTES;

  return (
    <div className="cb-panel" ref={panelRef}>
      <div className="cb-header">
        <span className="cb-title">Clipboard</span>
        <button
          type="button"
          className="cb-close"
          aria-label="Close clipboard"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      <textarea
        ref={textareaRef}
        className="cb-text"
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="Fetch the remote's clipboard, or paste here and send."
        aria-label="Clipboard text"
        spellCheck={false}
      />

      <div className="cb-actions">
        <button type="button" className="cb-btn" onClick={handleFetch}>
          {awaitingFetch ? "Fetching…" : "Fetch from remote"}
        </button>
        <button
          type="button"
          className="cb-btn cb-btn-primary"
          onClick={handleSend}
          disabled={overCap}
          title={
            overCap
              ? `Too long: ${bytes} bytes, the limit is ${MAX_CLIPBOARD_BYTES}`
              : undefined
          }
        >
          Send to remote
        </button>
      </div>

      <div className="cb-status">
        <span className={overCap ? "cb-count cb-count-over" : "cb-count"}>
          {bytes} / {MAX_CLIPBOARD_BYTES} bytes
        </span>
        <output className="cb-notice" aria-live="polite">
          {notice ?? ""}
        </output>
      </div>
    </div>
  );
}
