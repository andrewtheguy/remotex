// bun bench/decode.ts FILE.frames [PASSES]: the module in FLAC_WASM_DIR (pkg) decoding a sample's frames through its
// binding and copying each channel out, as the page does to play one, PASSES times over, timed by its own clock. With
// CHECK=1 the first pass's samples are compared with FILE.pcm, the signal the frames were made from, outside the
// timing.
import { readFileSync } from "node:fs";
import { basename, resolve } from "node:path";

const [file, passes = "1"] = process.argv.slice(2);
if (!file) {
  throw new Error("usage: bun bench/decode.ts FILE.frames [PASSES]");
}
const dir = resolve(
  process.env.FLAC_WASM_DIR ?? `${import.meta.dirname}/../pkg`,
);
const { default: init, Flac } = await import(`${dir}/remotex_flac.js`);
const { memory } = await init({
  module_or_path: readFileSync(`${dir}/remotex_flac_bg.wasm`),
});

const rate = Number(basename(file, ".frames").split("-").pop());
const block = rate === 48_000 ? 960 : 882;
const bytes = new Uint8Array(readFileSync(file));
const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
const frames: Uint8Array[] = [];
for (let at = 0; at < bytes.length; ) {
  const len = view.getUint32(at, true);
  frames.push(bytes.subarray(at + 4, at + 4 + len));
  at += 4 + len;
}

const flac = new Flac(rate, 2, block);
// A frame's samples, both channels: where the module left them, or from a build older than that, the copy it made.
const decode = (frame: Uint8Array): Float32Array => {
  const planar: number | Float32Array = flac.decode(frame);
  return typeof planar === "number"
    ? new Float32Array(memory.buffer, planar, 2 * block)
    : planar;
};
const played = [new Float32Array(block), new Float32Array(block)];
if (process.env.CHECK === "1") {
  const pcm = new Int16Array(
    readFileSync(file.replace(/\.frames$/, ".pcm")).buffer,
  );
  for (const [f, frame] of frames.entries()) {
    const planar = decode(frame);
    for (let n = 0; n < block; n++) {
      for (let channel = 0; channel < 2; channel++) {
        if (
          planar[channel * block + n] * 32768 !==
          pcm[(f * block + n) * 2 + channel]
        ) {
          process.stdout.write(
            `frame ${f}, sample ${n}, channel ${channel} is not the signal's\n`,
          );
          process.exit(1);
        }
      }
    }
  }
}
let sum = 0;
const started = performance.now();
for (let pass = 0; pass < Number(passes); pass++) {
  for (const frame of frames) {
    const planar = decode(frame);
    for (const [channel, plane] of played.entries()) {
      plane.set(planar.subarray(channel * block, (channel + 1) * block));
    }
    sum += played[0][block >> 1];
  }
}
const elapsed = performance.now() - started;
const decoded = Number(passes) * frames.length;
process.stdout.write(
  `${decoded} frames: mean ${((elapsed * 1000) / decoded).toFixed(2)} us (sum ${sum})\n`,
);
flac.free();
