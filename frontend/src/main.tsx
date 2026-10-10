import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { chooseAppleMedia } from "./appleMedia.ts";
import { askNativeVp9 } from "./nativeVp9.ts";
import { startupPermitted } from "./preflight.ts";
import { chooseRdpH264 } from "./rdpH264.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts. Then, with a decoder known to exist, the
// questions asked of it — whether it takes the gateway's 4:4:4 VP9, which decides
// whether this page decodes that itself (nativeVp9.ts), and whether it takes a High
// Performance Mac's HEVC and the H.264 of an RDP host's pipeline, whose answers
// every session socket this page opens carries (appleMedia.ts, rdpH264.ts).
// Awaited here so that nothing downstream has to wait on it or carry a path for its
// absence.
if (startupPermitted(root)) {
  await Promise.all([askNativeVp9(), chooseAppleMedia(), chooseRdpH264()]);
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
