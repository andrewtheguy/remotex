// The one environment check this client makes, and it is a refusal rather than a
// branch.
//
// One thing below the login is not optional and cannot be supplied by the page
// itself: a **secure context**, because that is what `navigator.clipboard`,
// `navigator.keyboard` and WebCodecs are all gated on.
//
// Missing it, every dependent feature used to go missing separately and explain
// itself separately: near-identical "reach this gateway over HTTPS" messages under
// the clipboard and the keyboard, a banner over the canvas, and a fallback path
// behind each. A session that half worked, in a way nobody could describe.
//
// So the check happens once, before React mounts, and the answer is no. Nothing
// downstream tests `isSecureContext` again, and nothing carries a second path for
// it: there is no such session left to have a path for.
//
// The **decoders** are not refusals. A browser without a `VideoDecoder`, or whose
// decoder refuses a codec, starts, and what needs the decoder is greyed at the
// picker with the reason (targetChoices.ts): wlshare's VP9 by its own question and
// a High Performance Mac's H.264 where there is no `VideoDecoder` (videoCodecs.ts),
// the Mac's HEVC where the
// page cannot decode it either (appleMedia.ts), and an RDP host's H.264
// (rdpH264.ts). The gateway's tiles, a Standard Mac's Adaptive picture and an RDP
// host's pipeline without H.264 are decoded by the page and need none of it. The
// **audio decoder** likewise: Safari from 16.4 to 18 has `VideoDecoder` and no
// `AudioDecoder`, and the desktop is the whole of a session where sound is not.
// Such a browser plays the one sound that needs no audio decoder, FLAC, which the
// page decodes itself (flacDecoder.ts). What does need one is greyed with the
// reason: Opus at the picker (targetChoices.ts), and a High Performance Mac's
// AAC-ELD in the menu's Audio button (FloatingMenu.tsx). Asked here, by
// `decodesAudio`, so that the question has one answer and one place.
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

/** Why the client will not start, in the words it goes on screen in. */
const INSECURE = {
  heading: "This gateway needs a secure connection",
  detail: [
    "The remote desktop needs a secure context for its video, audio, clipboard and keyboard.",
    "Reach it over HTTPS, over an SSH tunnel to localhost, or at http://localhost.",
  ],
};

/**
 * Whether this browser has a WebCodecs audio decoder, and so can play a remote's
 * sound at all. Checked as a global rather than through `isConfigSupported`,
 * because that is asynchronous and per codec, and this is a question about the
 * browser.
 */
export function decodesAudio(): boolean {
  return typeof AudioDecoder !== "undefined";
}

/** Why a sound that needs an audio decoder is greyed where `decodesAudio` says no. */
export const NO_AUDIO_DECODER =
  "This browser has no WebCodecs AudioDecoder, so it cannot play the remote's sound.";

/**
 * Put the refusal on screen, in place of the client, with the origin underneath:
 * "this gateway" is ambiguous the moment someone has two.
 *
 * Plain DOM, no React and no stylesheet of its own: the point of failing here is that
 * nothing else has started, and a message that needed the app to render would be a
 * message that could fail the same way.
 */
function renderRefusal(root: HTMLElement): void {
  root.replaceChildren();
  const panel = document.createElement("main");
  panel.className = "boot-refusal";
  const heading = document.createElement("h1");
  heading.textContent = INSECURE.heading;
  panel.append(heading);
  for (const line of INSECURE.detail) {
    const paragraph = document.createElement("p");
    paragraph.textContent = line;
    panel.append(paragraph);
  }
  const where = document.createElement("p");
  where.className = "boot-refusal-origin";
  where.textContent = window.location.origin;
  panel.append(where);
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
  if (window.isSecureContext) {
    return true;
  }
  renderRefusal(root);
  return false;
}
