import { readFileSync } from "node:fs";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The version shown on the login screen. Cargo.toml is the single source of
// truth (frontend/package.json stays an unused placeholder).
const cargoToml = readFileSync(
  new URL("../Cargo.toml", import.meta.url),
  "utf-8",
);
const version = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? "dev";

// Where `cargo run -- serve` is listening. 52380 is the built-in default, but
// a local config is free to pick another port ([server].port), and editing this
// file to match is a change that then wants un-editing before it is committed —
// so it is an environment variable:
//
//   REMOTEX_DEV_BACKEND=52675 bun run dev
//
// A full origin works too, for a backend on another host:
//
//   REMOTEX_DEV_BACKEND=http://192.168.1.10:52380 bun run dev
const backend = process.env.REMOTEX_DEV_BACKEND ?? "52380";
const backendUrl = /^\d+$/.test(backend)
  ? `http://localhost:${backend}`
  : backend;

// Dev server proxies the API and the WebSocket to the Rust backend, so
// `bun run dev` on :5173 talks to a locally running gateway.
//
// A standalone `bun run build` writes frontend/dist. Cargo instead sets this to
// its private OUT_DIR, because generated files are outputs rather than inputs to
// build.rs. Either way the one bundle is compiled into the gateway binary
// (src/assets.rs), which serves it over HTTP from an origin root.
const outDir = process.env.REMOTEX_FRONTEND_OUT_DIR ?? "dist";

export default defineConfig({
  // Relative asset URLs, and safe here rather than by luck: there is no
  // client-side router, so the document is only ever at `/` or at a one-segment
  // path the SPA fallback answered — and `./assets/…` resolves to `/assets/…`
  // from both.
  base: "./",
  define: {
    __APP_VERSION__: JSON.stringify(version),
  },
  build: {
    outDir,
    // Vite does not empty an output outside the project root by default. Cargo's
    // directory must not retain obsolete content-hashed assets between builds.
    emptyOutDir: true,
  },
  plugins: [react()],
  server: {
    proxy: {
      "/api": backendUrl,
      "/ws": {
        target: backendUrl,
        ws: true,
      },
    },
  },
});
