// The screen before there is a client, and the one that says why there isn't.
//
// The gateway's stderr is shown verbatim and is never parsed or summarised: a config
// it refused names the target, the key or the line that is wrong, and no sentence
// written here could be as useful as the one already on screen.

import { shell } from "./bridge.ts";

const heading = document.getElementById("heading") as HTMLHeadingElement;
const message = document.getElementById("message") as HTMLParagraphElement;
const log = document.getElementById("log") as HTMLPreElement;
const retry = document.getElementById("retry") as HTMLButtonElement;
const configure = document.getElementById("configure") as HTMLButtonElement;

shell.onStatus((status) => {
  if (status.phase === "starting") {
    heading.textContent = `Starting ${status.branding}…`;
    message.textContent = "Starting the local gateway.";
    message.classList.remove("problem");
    log.classList.add("hidden");
    retry.disabled = true;
    return;
  }
  heading.textContent = `${status.branding} could not start`;
  message.textContent = status.message;
  message.classList.add("problem");
  log.textContent = status.log;
  log.classList.toggle("hidden", status.log.trim() === "");
  retry.disabled = false;
});

retry.addEventListener("click", () => shell.act("retry"));
configure.addEventListener("click", () => shell.act("configure"));
