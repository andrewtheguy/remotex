import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { MAX_CLIPBOARD_BYTES, type RemoteClipboard } from "./protocol.ts";
import { useDockedHeight } from "./SoftKeyboardPanel.tsx";

// Deliberately follows tmp/remotex-old's clipboard state machine: the remote
// value starts as a metadata card, revealing it turns that same slot into the
// editable textarea, and closing the panel throws the manual state away. The
// difference is storage — the plaintext snapshot came from remotex on Fetch;
// it is not encrypted into localStorage by this browser.

const NOTICE_MS = 1800;
const CRC32_POLYNOMIAL = 0xedb88320;
const encoder = new TextEncoder();

const CRC32_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let c = i;
    for (let j = 0; j < 8; j += 1) {
      c = (c & 1) === 1 ? CRC32_POLYNOMIAL ^ (c >>> 1) : c >>> 1;
    }
    table[i] = c >>> 0;
  }
  return table;
})();

function computeCrc32Hex(bytes: Uint8Array): string {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    const tableIndex = (crc ^ byte) & 0xff;
    crc = (crc >>> 8) ^ CRC32_TABLE[tableIndex];
  }
  const normalized = (crc ^ 0xffffffff) >>> 0;
  return normalized.toString(16).padStart(8, "0");
}

interface ClipboardPanelProps {
  onSend: (text: string) => void;
  remoteClipboard: RemoteClipboard | null;
  onClose: () => void;
  onDockedHeightChange?: (px: number) => void;
}

export function ClipboardPanel({
  onSend,
  remoteClipboard,
  onClose,
  onDockedHeightChange,
}: ClipboardPanelProps) {
  const [clipboardInput, setClipboardInput] = useState("");
  const [isManualInputActive, setIsManualInputActive] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const clipboardInputRef = useRef<HTMLTextAreaElement>(null);

  useDockedHeight(panelRef, onDockedHeightChange);

  const isRemoteMetadataMode = remoteClipboard !== null && !isManualInputActive;
  // The remote's clipboard was refused for its size, so there is nothing to
  // conceal, reveal or copy — only the size to report. Kept distinct from an
  // empty clipboard, which is what a truncating transfer would have looked like.
  const oversizedBytes =
    isRemoteMetadataMode && remoteClipboard
      ? remoteClipboard.oversizedBytes
      : null;
  const remoteBytes = useMemo(
    () => encoder.encode(remoteClipboard?.text ?? ""),
    [remoteClipboard?.text],
  );
  const metadataLines = useMemo(() => {
    if (!remoteClipboard) {
      return [];
    }
    const changedAt =
      remoteClipboard.changedAtMs === null
        ? "UNKNOWN"
        : new Date(remoteClipboard.changedAtMs).toLocaleString(undefined, {
            dateStyle: "short",
            timeStyle: "medium",
          });
    if (remoteClipboard.oversizedBytes !== null) {
      return [
        `LEN ${remoteClipboard.oversizedBytes}B`,
        `LIMIT ${MAX_CLIPBOARD_BYTES}B`,
        `AT ${changedAt}`,
      ];
    }
    return [
      `CRC32 ${computeCrc32Hex(remoteBytes)}`,
      `LEN ${remoteBytes.byteLength}B`,
      `AT ${changedAt}`,
    ];
  }, [remoteClipboard, remoteBytes]);

  useEffect(() => {
    if (notice === null) {
      return;
    }
    const timer = setTimeout(() => setNotice(null), NOTICE_MS);
    return () => clearTimeout(timer);
  }, [notice]);

  // As in remotex-old, only an already-manual/empty input gets focus. The
  // metadata card remains the first deliberate action for a fetched snapshot.
  useEffect(() => {
    if (!isRemoteMetadataMode) {
      clipboardInputRef.current?.focus();
    }
  }, [isRemoteMetadataMode]);

  const revealRemoteClipboard = useCallback(() => {
    if (!remoteClipboard) {
      return;
    }
    setClipboardInput(remoteClipboard.text);
    setIsManualInputActive(true);
  }, [remoteClipboard]);

  // The editor with nothing in it, for a remote clipboard there is no text for.
  const startManualInput = useCallback(() => {
    setClipboardInput("");
    setIsManualInputActive(true);
  }, []);

  // Why Copy has nothing to put on this browser's clipboard. The two cases look
  // identical from here — both are empty text — and only one of them means the
  // remote copied nothing.
  const emptyCopyNotice =
    oversizedBytes === null
      ? "Nothing to copy"
      : "Remote clipboard too large to transfer";

  const selectClipboardText = useCallback((target: HTMLTextAreaElement) => {
    target.focus();
    target.select();
    target.setSelectionRange(0, target.value.length);
  }, []);

  const handleSend = useCallback(() => {
    const bytes = encoder.encode(clipboardInput).byteLength;
    if (bytes > MAX_CLIPBOARD_BYTES || isRemoteMetadataMode) {
      return;
    }
    // The remote takes ownership of whatever arrives, so an empty send would
    // wipe its clipboard rather than leave it alone.
    if (clipboardInput.length === 0) {
      setNotice("Reveal or enter clipboard text first");
      return;
    }
    onSend(clipboardInput);
    setNotice("Clipboard sent to remote");
  }, [clipboardInput, isRemoteMetadataMode, onSend]);

  const handleCopy = useCallback(async () => {
    const text =
      isRemoteMetadataMode && remoteClipboard
        ? remoteClipboard.text
        : clipboardInput;
    // Same reason in the other direction: writing an empty string is not a
    // no-op, it clears this browser's clipboard.
    if (text.length === 0) {
      setNotice(emptyCopyNotice);
      return;
    }
    if (navigator.clipboard?.writeText) {
      try {
        await navigator.clipboard.writeText(text);
        setNotice("Clipboard copied");
        return;
      } catch {
        // Fall through to the selection-based path used by remotex-old.
      }
    }

    const existingInput = clipboardInputRef.current;
    if (existingInput) {
      const previousStart = existingInput.selectionStart;
      const previousEnd = existingInput.selectionEnd;
      selectClipboardText(existingInput);
      document.execCommand("copy");
      existingInput.setSelectionRange(previousStart, previousEnd);
      setNotice("Clipboard copied");
      return;
    }

    // Metadata mode has no mounted textarea. Use a short-lived off-screen one
    // so Copy remains a fallback without revealing the remote value on screen.
    const input = document.createElement("textarea");
    input.value = text;
    input.className = "cb-copy-fallback";
    document.body.append(input);
    try {
      selectClipboardText(input);
      document.execCommand("copy");
      setNotice("Clipboard copied");
    } finally {
      input.remove();
    }
  }, [
    clipboardInput,
    emptyCopyNotice,
    isRemoteMetadataMode,
    remoteClipboard,
    selectClipboardText,
  ]);

  const inputBytes = encoder.encode(clipboardInput).byteLength;
  const overCap = inputBytes > MAX_CLIPBOARD_BYTES;

  return (
    <div className="panel" ref={panelRef}>
      <div className="panel-header">
        <span className="panel-title">Clipboard</span>
        <button
          type="button"
          className="panel-close"
          aria-label="Close clipboard"
          onClick={onClose}
        >
          ✕
        </button>
      </div>

      <div className="cb-input-shell">
        {oversizedBytes !== null ? (
          /* Nothing was transferred, so there is no text behind this to
             reveal. It still switches to the editor, which is the one thing
             left to do from here — otherwise this state has no way out of
             metadata mode and Send stays disabled for the session. */
          <button
            type="button"
            className="cb-metadata-card"
            onClick={startManualInput}
            aria-label="Remote clipboard too large; switch to typing"
          >
            {metadataLines.map((line) => (
              <span key={line} className="cb-metadata-primary">
                {line}
              </span>
            ))}
            <span className="cb-metadata-secondary">
              Too large to transfer. Copy less on the remote, or click to type
            </span>
          </button>
        ) : isRemoteMetadataMode ? (
          <button
            type="button"
            className="cb-metadata-card"
            onClick={revealRemoteClipboard}
            aria-label="Reveal remote clipboard content"
          >
            {metadataLines.map((line) => (
              <span key={line} className="cb-metadata-primary">
                {line}
              </span>
            ))}
            <span className="cb-metadata-secondary">
              Click anywhere to reveal
            </span>
          </button>
        ) : (
          <textarea
            ref={clipboardInputRef}
            className="cb-text"
            value={clipboardInput}
            onChange={(event) => {
              setClipboardInput(event.currentTarget.value);
              setIsManualInputActive(true);
            }}
            onFocus={(event) => selectClipboardText(event.currentTarget)}
            onClick={(event) => selectClipboardText(event.currentTarget)}
            rows={6}
            aria-label="Clipboard text"
            spellCheck={false}
          />
        )}
      </div>

      <div className="cb-actions">
        <button
          type="button"
          className="cb-btn"
          onClick={handleSend}
          disabled={
            isRemoteMetadataMode || overCap || clipboardInput.length === 0
          }
          title={
            overCap
              ? `Too long: ${inputBytes} bytes, the limit is ${MAX_CLIPBOARD_BYTES}`
              : undefined
          }
        >
          Send
        </button>
        <button type="button" className="cb-btn" onClick={handleCopy}>
          Copy
        </button>
      </div>

      <div className="cb-status">
        {!isRemoteMetadataMode && (
          <span className={overCap ? "cb-count cb-count-over" : "cb-count"}>
            {inputBytes} / {MAX_CLIPBOARD_BYTES} bytes
          </span>
        )}
        <output className="cb-notice" aria-live="polite">
          {notice ?? ""}
        </output>
      </div>
    </div>
  );
}
