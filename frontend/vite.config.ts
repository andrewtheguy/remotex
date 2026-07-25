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
export default defineConfig({
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
