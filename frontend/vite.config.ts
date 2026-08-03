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
// One build, and one consumer shape: `bun run build` runs once in CI and the
// single `frontend/dist` it makes is what everything ships — the tarball's
// `share/remotex/web` and the container image. Both serve it over HTTP from an
// origin root, which is the only thing the output below has to suit.
export default defineConfig({
  // Relative asset URLs, and safe here rather than by luck: there is no
  // client-side router, so the document is only ever at `/` or at a one-segment
  // path the SPA fallback answered — and `./assets/…` resolves to `/assets/…`
  // from both.
  base: "./",
  define: {
    __APP_VERSION__: JSON.stringify(version),
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
