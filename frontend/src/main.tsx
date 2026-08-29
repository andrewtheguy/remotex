import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App.tsx";
import "./index.css";
import { startupPermitted } from "./preflight.ts";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element not found");
}

// The other half of index.css's `touch-action: pan-x pan-y`, for WebKit alone:
// Safari zooms the page from its own non-standard `gesture*` events, which
// `touch-action` does not always reach. These are separate from the touch
// events the desktop's pinch is built on, so refusing them costs that gesture
// nothing. Every other browser never fires them.
for (const type of ["gesturestart", "gesturechange", "gestureend"]) {
  document.addEventListener(type, (event) => event.preventDefault(), {
    passive: false,
  });
}

// Before `App`, which asks the gateway who this is on its first render: a session
// claimed from a page that cannot decode its own video is a session taken away from
// wherever it was working. See preflight.ts.
if (startupPermitted(root)) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
