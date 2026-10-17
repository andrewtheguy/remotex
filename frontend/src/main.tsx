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
// questions asked of it — whether it takes VP9 at 4:4:4, which decides whether a
// wlshare target is sent it, a High Performance Mac's HEVC and the H.264 of an RDP
// host's pipeline, whose answers every session socket this page opens carries
// (nativeVp9.ts, appleMedia.ts, rdpH264.ts).
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
