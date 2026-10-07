import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { chooseAppleMedia } from "./appleMedia.ts";
import { gatewayConfig } from "./gatewayConfig.ts";
import { startupPermitted } from "./preflight.ts";
import { chooseRdpH264 } from "./rdpH264.ts";
import { chooseVideoChroma } from "./videoChroma.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts. Then, with a decoder known to exist, the
// questions asked of it — how much colour it takes, whether it takes a High
// Performance Mac's HEVC, and whether it takes the H.264 of an RDP host's pipeline
// — whose answers every session socket this page opens carries (videoChroma.ts,
// appleMedia.ts, rdpH264.ts). Awaited here so that nothing downstream has to wait
// on it or carry a path for its absence. The chroma's question is answered with
// the gateway's public config, which says whether the page may decode 4:4:4
// itself: it is waited for there, and is the request `App` shares.
if (startupPermitted(root)) {
  await Promise.all([
    chooseVideoChroma(gatewayConfig().then((config) => config.vp9Wasm)),
    chooseAppleMedia(),
    chooseRdpH264(),
  ]);
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
