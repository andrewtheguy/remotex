import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

// The macOS viewer's canvas page, built separately from the SPA.
//
// A second config rather than a second entry in `vite.config.ts`, because the
// two builds ship to different places and must not be able to reach each
// other's output: `dist/` becomes the gateway's web root and `dist-viewer/`
// goes inside remotex.app, where an SPA `index.html` appearing would mean the
// embedded gateway had started serving a web UI (release.yml checks for it).
//
// No React plugin: this page has no components. Nothing here reads
// `__APP_VERSION__` either — the app shows its own version, from its bundle.
export default defineConfig({
  // Relative asset URLs, because this page is not served from a root. The app
  // serves it under a per-launch random path (`/<token>/`), so Vite's default
  // absolute `/assets/…` asks for something a token away from where it is and
  // gets the 404 that guards everything else — which looks like a blank desktop,
  // since the document still loads and only its script and stylesheet vanish.
  base: "./",
  build: {
    outDir: "dist-viewer",
    emptyOutDir: true,
    rollupOptions: {
      input: fileURLToPath(new URL("viewer.html", import.meta.url)),
    },
  },
});
