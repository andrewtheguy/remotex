// How each engine words running out of memory, wherever the page shows what went
// wrong: JavaScriptCore's and SpiderMonkey's RangeError, V8's for a buffer or a
// WebAssembly memory it could not allocate, and Emscripten's abort for a module
// whose memory could not grow. A Rust module's own allocation failure traps as
// `unreachable`, which a panic does too, so it is not one of them.
const OUT_OF_MEMORY =
  /out of memory|\bOOM\b|could not allocate|cannot allocate|allocation failed|cannot enlarge memory/i;

/**
 * Whether `reason`, a failure the page shows, is the browser saying it ran out of
 * memory: the page then says so, and how to get the memory back
 * (`keepsEndedWorkers`).
 */
export function saysOutOfMemory(reason: string): boolean {
  return OUT_OF_MEMORY.test(reason);
}

/** What `keepsEndedWorkers` reads of the browser. */
type Device = Pick<Navigator, "userAgent" | "platform" | "maxTouchPoints">;

/**
 * Whether this device keeps what a worker held, its instance and the memory it
 * shares, after the worker is ended while its thread waits in WebAssembly, until
 * the tab is closed, a reload included: an iPhone or an iPad, whatever the
 * browser. The compositor's pool threads wait there between jobs: an iPhone's tab
 * ran out of memory at its seventh session that ended them so, and on every
 * reload after, and a new tab did not. Safari on a Mac, running the same
 * sessions, did not.
 *
 * So there the paint worker stops its pool before it is ended
 * (desktopPainter.ts), and a page that ran out of memory, whose reload would
 * end its workers so, says to close the tab rather than reload it. An iPad that
 * asks for desktop pages names itself a Mac, and is told from one by its touch
 * points.
 */
export function keepsEndedWorkers(device: Device = navigator): boolean {
  return (
    /iPhone|iPad|iPod/.test(device.userAgent) ||
    (device.platform === "MacIntel" && device.maxTouchPoints > 1)
  );
}
