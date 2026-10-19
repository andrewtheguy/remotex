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
 * (`reloadFreesMemory`).
 */
export function saysOutOfMemory(reason: string): boolean {
  return OUT_OF_MEMORY.test(reason);
}

/** What `reloadFreesMemory` reads of the browser. */
type Device = Pick<Navigator, "userAgent" | "platform" | "maxTouchPoints">;

/**
 * Whether a reload gives the page's memory back, which the session then
 * reattaches with as it was. Not on an iPhone or an iPad, whatever the browser:
 * there a session's compositor keeps its memory after the session ends, until
 * the tab is closed, a reload included — an iPhone's tab failed at its seventh
 * session and on every reload after, and a new tab did not. Safari on a Mac,
 * running the same sessions, did not. There the page says to close the tab
 * instead. An iPad that asks for desktop pages names itself a Mac, and is told
 * from one by its touch points.
 */
export function reloadFreesMemory(device: Device = navigator): boolean {
  const ios =
    /iPhone|iPad|iPod/.test(device.userAgent) ||
    (device.platform === "MacIntel" && device.maxTouchPoints > 1);
  return !ios;
}
