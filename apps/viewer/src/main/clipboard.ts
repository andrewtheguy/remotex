// The Mac's pasteboard and the remote's clipboard, kept in step.
//
// One of the two things the app still does for itself, and it is here for a reason a
// web page cannot argue with: reading the system pasteboard on a timer and writing
// it without a user gesture are both things a document is not allowed to do. This
// also keeps working while the window is unfocused or minimised, which is the case
// the whole arrangement is for — copy something in another app, come back, paste.
//
// The page owns the *panel* — the reveal, the copy consent, the editor — and this
// owns the automatic sync underneath it.
//
// Both directions are guarded against echoing. The client has guards of its own for
// the same loop seen from the other side; these stop a value that arrived from the
// remote being handed straight back as though the user had copied it here.

/** The pasteboard, as little of it as this needs. Injected, so tests need none. */
export interface Pasteboard {
  read(): string;
  write(text: string): void;
}

export interface ClipboardHooks {
  pasteboard: Pasteboard;
  /** Where a local change goes: the bridge's `clipboardLocal` command. */
  send(text: string): void;
  /** Started and stopped with the sync. Absent in tests, which poll by hand. */
  schedule?(tick: () => void): () => void;
}

/**
 * How often the pasteboard is read.
 *
 * There is no change notification for it on macOS, so this is a poll or it is
 * nothing.
 */
export const POLL_INTERVAL_MS = 400;

/**
 * The gateway refuses anything larger in either direction, so an oversized value is
 * skipped rather than truncated — the remote keeps what it had, which is better than
 * being given half of something.
 */
export const MAXIMUM_BYTES = 65_536;

export class ClipboardSynchronizer {
  private enabled = false;
  private lastSeen = "";
  private lastFromRemote: string | null = null;
  private lastToRemote: string | null = null;
  private cancelPolling: (() => void) | null = null;

  constructor(private readonly hooks: ClipboardHooks) {}

  get isEnabled(): boolean {
    return this.enabled;
  }

  /**
   * Follow or stop following the pasteboard.
   *
   * Enabling pushes once: whatever is on the pasteboard when a desktop comes up is
   * something the user may well be about to paste, and it predates anything this
   * could have observed.
   */
  update(enabled: boolean): void {
    if (this.enabled === enabled) {
      return;
    }
    this.enabled = enabled;
    if (enabled) {
      this.lastSeen = this.hooks.pasteboard.read();
      this.pushLocal(this.lastSeen);
      this.cancelPolling = this.hooks.schedule?.(() => this.poll()) ?? null;
    } else {
      this.cancelPolling?.();
      this.cancelPolling = null;
      this.lastFromRemote = null;
      this.lastToRemote = null;
    }
  }

  /**
   * The remote's clipboard changed on its own.
   *
   * Only unsolicited pushes reach here: a value the *user* asked to see is the
   * page's panel to show, and its Copy button is the consent to put it on this Mac.
   */
  receiveFromRemote(text: string): void {
    if (!this.enabled || text === "") {
      return;
    }
    const alreadyMirrored = text === this.lastFromRemote;
    const echoed = text === this.lastToRemote;
    this.lastFromRemote = text;
    if (alreadyMirrored || echoed) {
      return;
    }
    // A newer local value wins: somebody who copied here a moment ago wants their
    // own text when they paste.
    const current = this.hooks.pasteboard.read();
    if (current !== this.lastSeen) {
      this.poll();
      return;
    }
    this.hooks.pasteboard.write(text);
    this.lastSeen = text;
  }

  /** Read the pasteboard, and send it on if it has changed. */
  poll(): void {
    if (!this.enabled) {
      return;
    }
    const text = this.hooks.pasteboard.read();
    if (text === this.lastSeen) {
      return;
    }
    this.lastSeen = text;
    this.pushLocal(text);
  }

  private pushLocal(text: string): void {
    if (
      text === "" ||
      utf8Bytes(text) > MAXIMUM_BYTES ||
      text === this.lastFromRemote ||
      text === this.lastToRemote
    ) {
      return;
    }
    this.lastToRemote = text;
    this.hooks.send(text);
  }
}

// `TextEncoder`, not `Buffer.byteLength`: this class is shared with
// `apps/companion`, whose offscreen document is a browser context with no `Buffer` in
// it. Same answer, one fewer environment assumption.
export function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}
