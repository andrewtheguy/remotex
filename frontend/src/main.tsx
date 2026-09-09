import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { startupPermitted } from "./preflight.ts";
import { chooseVideoChroma } from "./videoChroma.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts. Then, with a decoder known to exist, the
// one question asked of it — how much colour it takes — whose answer every session
// socket this page opens carries (videoChroma.ts). Awaited here so that nothing
// downstream has to wait on it or carry a path for its absence.
if (startupPermitted(root)) {
  await chooseVideoChroma();
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
