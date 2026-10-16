// Whether this page loads as little WebAssembly as it can (docs/architecture.md,
// The client and its bundle): one whose browser gives a page less memory than a
// desktop's does. No browser says how much a page may have, so this goes by what
// they do say. An iPhone or an iPad holds it tight whatever the browser, since
// iOS caps the memory each app's processes may use whatever engine runs in them,
// and its WebKit reports no device memory; elsewhere a Chromium browser reports
// the device's memory (`navigator.deviceMemory`, in GiB, rounded), and Safari
// and Firefox on a desktop report none.

/** The most device memory, in GiB, that is still held tight. */
export const TIGHT_DEVICE_MEMORY = 4;

/** What the browser says of itself that bears on its memory. */
export interface MemorySigns {
  platform: string;
  maxTouchPoints: number;
  /** `navigator.deviceMemory`, where the browser reports it. */
  deviceMemory: number | undefined;
}

export function memoryIsTight(signs: MemorySigns): boolean {
  if (/^(iPhone|iPad|iPod)/.test(signs.platform)) {
    return true;
  }
  // An iPad asks for desktop pages and says it is a Mac, and no Mac has a
  // touchscreen.
  if (signs.platform === "MacIntel" && signs.maxTouchPoints > 1) {
    return true;
  }
  return (
    signs.deviceMemory !== undefined &&
    signs.deviceMemory <= TIGHT_DEVICE_MEMORY
  );
}

/** {@link memoryIsTight} for this browser. */
export function pageMemoryIsTight(): boolean {
  const { deviceMemory } = navigator as Navigator & { deviceMemory?: number };
  return memoryIsTight({
    platform: navigator.platform || "",
    maxTouchPoints: navigator.maxTouchPoints || 0,
    deviceMemory,
  });
}

// How each engine words running out of memory, wherever the page shows what went
// wrong: JavaScriptCore's and SpiderMonkey's RangeError, V8's for a buffer or a
// WebAssembly memory it could not allocate, and Emscripten's abort for a module
// whose memory could not grow. A Rust module's own allocation failure traps as
// `unreachable`, which a panic does too, so it is not one of them.
const OUT_OF_MEMORY =
  /out of memory|\bOOM\b|could not allocate|cannot allocate|allocation failed|cannot enlarge memory/i;

/**
 * Whether `reason`, a failure the page shows, is the browser saying it ran out of
 * memory: the page then says so and offers the reload that gives it all back, the
 * session reattached as it was.
 */
export function saysOutOfMemory(reason: string): boolean {
  return OUT_OF_MEMORY.test(reason);
}
