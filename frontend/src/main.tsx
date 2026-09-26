import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { chooseAppleHevc } from "./appleHevc.ts";
import { startupPermitted } from "./preflight.ts";
import { chooseVideoChroma } from "./videoChroma.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts. Then, with a decoder known to exist, the
// two questions asked of it — how much colour it takes, and whether it takes a High
// Performance Mac's HEVC — whose answers every session socket this page opens carries
// (videoChroma.ts, appleHevc.ts). Awaited here so that nothing
// downstream has to wait on it or carry a path for its absence.
if (startupPermitted(root)) {
  await Promise.all([chooseVideoChroma(), chooseAppleHevc()]);
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
