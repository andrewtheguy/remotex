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
