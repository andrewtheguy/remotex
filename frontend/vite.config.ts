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
// Strip `type="module"` and `crossorigin` from the emitted script tag.
//
// Both are fatal for the one client that is not served over HTTP. A module script
// is *always* fetched with CORS, and a `file://` document's origin is opaque, so
// WebKit blocks it — the page loads, the bundle never runs, and the window is
// blank with two errors carrying no message. `crossorigin` would put a classic
// script in the same mode for the same result.
//
// `defer` replaces it rather than simply going away, and that part is not
// cosmetic: a module script is deferred by definition, a classic one runs the
// moment the parser reaches it — which here is inside `<head>`, before the
// `<div id="root">` it mounts into exists. Without this the bundle loads, runs and
// throws "Root element not found".
//
// The bundle below is an IIFE precisely so this rewrite is honest: without the
// format change, dropping `type="module"` from a file full of `import` statements
// would trade a blocked fetch for a syntax error.
function classicScriptTag() {
  return {
    name: "remotex:classic-script-tag",
    transformIndexHtml(html: string) {
      return html
        .replace(/\stype="module"/g, " defer")
        .replace(/\scrossorigin/g, "");
    },
  };
}

export default defineConfig({
  build: {
    rollupOptions: {
      output: {
        // One classic script, no `import` at runtime. See `classicScriptTag`.
        format: "iife" as const,
        inlineDynamicImports: true,
      },
    },
    // Emits `<link rel="modulepreload">`, which is meaningless without modules.
    modulePreload: false,
  },
  // Relative asset URLs, because one of the two clients is not served from an
  // origin root: `remotex.app` loads this page from `file://` inside its bundle,
  // where the absolute `/assets/…` vite emits by default resolves to the
  // filesystem root and finds nothing.
  //
  // Safe for the served client too, and not by luck: there is no client-side
  // router here, so the document is only ever at `/` or at a one-segment path the
  // SPA fallback answered — and `./assets/…` resolves to `/assets/…` from both.
  base: "./",
  define: {
    __APP_VERSION__: JSON.stringify(version),
  },
  plugins: [react(), classicScriptTag()],
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
