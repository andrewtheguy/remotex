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

/**
 * Whether a reload gives the page's memory back, which the session then
 * reattaches with as it was. Not in WebKit — Safari, and every browser on iOS
 * and iPadOS — where a shared WebAssembly memory outlives a reload of its tab:
 * an iPhone's tab ran out of memory again on every reload, and a new tab did
 * not (WebKit bug 256023). There the page says to close the tab instead. Told
 * apart by the vendor WebKit names, which Chromium's and Gecko's do not.
 */
export function reloadFreesMemory(vendor = navigator.vendor): boolean {
  return vendor !== "Apple Computer, Inc.";
}
