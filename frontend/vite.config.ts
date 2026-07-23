import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Dev server proxies the API and the WebSocket to the Rust backend (port 52380),
// so `bun run dev` on :5173 talks to `cargo run -- serve`.
export default defineConfig({
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
