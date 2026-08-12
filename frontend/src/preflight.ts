// The one environment check this client makes, and it is a refusal rather than a
// branch.
//
// Two things below the login are not optional and neither can be supplied by the
// page itself. A **secure context**, because that is what `navigator.clipboard`,
// `navigator.keyboard` and WebCodecs are all gated on. And the **WebCodecs
// decoders**, because the desktop arrives as an encoded video stream and its sound
// as encoded packets, and nothing here decodes either one itself — the gateway
// names a configuration and the browser's decoder does the work (videoDecoder.ts,
// audioPlayer.ts).
//
// Missing either, every dependent feature used to go missing separately and explain
// itself separately: near-identical "reach this gateway over HTTPS" messages under
// the clipboard and the keyboard, a banner over the canvas, another beside the audio
// toggle, and a fallback path behind each. A session that half worked, in a way
// nobody could describe.
//
// So the checks happen once, before React mounts, and the answer is no. Nothing
// downstream tests `isSecureContext` or `typeof VideoDecoder` again, and nothing
// carries a second path for either case: there is no such session left to have a
// path for.
//
// The gateway speaks plain HTTP and always has — it has no TLS listener and is not
// getting one. A secure context therefore comes from where the page is reached, not
// from what the gateway is:
//
//   http://localhost:52380, http://127.0.0.1:52380, http://[::1]:52380   loopback
//   http://<label>.localhost:52380                                       RFC 6761
//   https://gateway.example                                    TLS-terminating proxy
//
// A LAN address on plain http is the case this refuses, and the message names the
// three ways out.

/** One reason the client will not start, in the words it goes on screen in. */
interface Refusal {
  heading: string;
  detail: readonly string[];
  /**
   * Whether to name the origin underneath. True where the origin *is* the problem,
   * false where it is not — under "this browser cannot decode" it would read as an
   * accusation against the address, which is the one thing that is fine.
   */
  origin: boolean;
}

const INSECURE: Refusal = {
  heading: "This gateway needs a secure connection",
  detail: [
    "The remote desktop needs a secure context for its video, audio, clipboard and keyboard.",
    "Reach it over HTTPS, over an SSH tunnel to localhost, or at http://localhost.",
  ],
  origin: true,
};

/**
 * The decoders this client cannot work without, named as a reader would say them.
 *
 * Both, not either: audio is a target's own choice and video is a render dial's, so
 * a browser with one and not the other is a browser that plays some targets and not
 * others — which is exactly the half-working session this file exists to refuse.
 * Checked as globals rather than through `isConfigSupported`, because that is
 * asynchronous and per codec, and this is a question about the browser.
 */
function missingDecoders(): string[] {
  const missing: string[] = [];
  if (typeof VideoDecoder === "undefined") {
    missing.push("video");
  }
  if (typeof AudioDecoder === "undefined") {
    missing.push("audio");
  }
  return missing;
}

function noDecoders(missing: readonly string[]): Refusal {
  return {
    heading: "This browser cannot decode a remote desktop",
    detail: [
      `The desktop and its sound arrive encoded, and this browser has no WebCodecs ${missing.join(" or ")} decoder to play them with. Both are needed.`,
      "Which browsers have them depends on the version and the platform. A recent desktop Chrome or Edge is the safe answer.",
    ],
    origin: false,
  };
}

/**
 * Why the client may not start, or null.
 *
 * The secure context is asked about first because it is the answerable one: WebCodecs
 * is itself secure-context gated, so an insecure origin fails both checks, and
 * "install a different browser" would be the wrong thing to tell someone whose
 * browser is fine.
 */
function refusal(): Refusal | null {
  if (!window.isSecureContext) {
    return INSECURE;
  }
  const missing = missingDecoders();
  return missing.length > 0 ? noDecoders(missing) : null;
}

/**
 * Put the refusal on screen, in place of the client.
 *
 * Plain DOM, no React and no stylesheet of its own: the point of failing here is that
 * nothing else has started, and a message that needed the app to render would be a
 * message that could fail the same way.
 */
function renderRefusal(root: HTMLElement, reason: Refusal): void {
  root.replaceChildren();
  const panel = document.createElement("main");
  panel.className = "boot-refusal";
  const heading = document.createElement("h1");
  heading.textContent = reason.heading;
  panel.append(heading);
  for (const line of reason.detail) {
    const paragraph = document.createElement("p");
    paragraph.textContent = line;
    panel.append(paragraph);
  }
  if (reason.origin) {
    const where = document.createElement("p");
    where.className = "boot-refusal-origin";
    where.textContent = window.location.origin;
    panel.append(where);
  }
  root.append(panel);
}

/**
 * Whether the client may start, having said why if it may not.
 *
 * Returns rather than throws so the caller reads as the sequence it is: check, then
 * mount. A throw here would land in the console, which is the one place a user who
 * has just been handed a blank page is not looking.
 */
export function startupPermitted(root: HTMLElement): boolean {
  const reason = refusal();
  if (!reason) {
    return true;
  }
  renderRefusal(root, reason);
  return false;
}
