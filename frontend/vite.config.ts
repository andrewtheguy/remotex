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

// Dev server proxies the API and the WebSocket to the Rust backend (port 52380),
// so `bun run dev` on :5173 talks to `cargo run -- serve`.
export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(version),
  },
  plugins: [react()],
  server: {
    proxy: {
      "/api": "http://localhost:52380",
      "/ws": {
        target: "http://localhost:52380",
        ws: true,
      },
    },
  },
});
