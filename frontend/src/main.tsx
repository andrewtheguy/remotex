import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { startupPermitted } from "./preflight.ts";
import { chooseVideoCodec } from "./videoCodec.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts. Then, with a decoder known to exist,
// the one question asked of it — VP9 or the H.264 fallback — whose answer every
// session socket this page opens carries (videoCodec.ts). Awaited here so that
// nothing downstream has to wait on it or carry a path for its absence.
if (startupPermitted(root)) {
  await chooseVideoCodec();
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
