// The one environment check this client makes, and it is a refusal rather than a
// branch.
//
// Almost everything below the login needs a secure context: WebCodecs for audio and
// video, `navigator.clipboard` for the clipboard bridge, `navigator.keyboard` for
// immersive mode. On a page that is not one, each of those goes missing separately
// and each used to explain itself separately — three near-identical "reach this
// gateway over HTTPS (or localhost)" messages, three fallbacks behind them, and a
// session that half worked in a way nobody could describe.
//
// So the check happens once, before React mounts, and the answer is no. Nothing
// downstream tests `isSecureContext` again and nothing carries a second path for the
// case: there is no such session to have a path for.
//
// The gateway speaks plain HTTP and always has — it has no TLS listener and is not
// getting one. A secure context therefore comes from where the page is reached, not
// from what the gateway is:
//
//   http://localhost:52380, http://127.0.0.1:52380, http://[::1]:52380   loopback
//   http://<label>.localhost:52380                                       RFC 6761
//   https://gateway.example                                    TLS-terminating proxy
//   remotex://app                                     the shell's own scheme, `secure`
//
// A LAN address on plain http is the case this refuses, and the message names the
// three ways out.

const HEADING = "This gateway needs a secure connection";

const DETAIL = [
  "The remote desktop needs a secure context for its video, audio, clipboard and keyboard.",
  "Reach it over HTTPS, over an SSH tunnel to localhost, or at http://localhost.",
];

/**
 * Put the refusal on screen, in place of the client.
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
  heading.textContent = HEADING;
  panel.append(heading);
  for (const line of DETAIL) {
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
