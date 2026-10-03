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

// A standalone `bun run build` writes frontend/dist. Cargo instead sets this to
// its private OUT_DIR, because generated files are outputs rather than inputs to
// build.rs. Either way the one bundle is compiled into the gateway binary
// (src/assets.rs), which serves it over HTTP from an origin root. The gateway is
// the only thing that serves the page: there is no dev server in front of it.
const outDir = process.env.REMOTEX_FRONTEND_OUT_DIR ?? "dist";

export default defineConfig({
  // Absolute asset URLs: the gateway serves the page from an origin root, and
  // the document is not always one segment deep — a display shown in a tab of its
  // own is the page at `/display/2`, where `./assets/…` would resolve to
  // `/display/assets/…` and come back as the SPA fallback's HTML.
  base: "/",
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
});
