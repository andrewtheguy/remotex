// The configuration editor: a text box, and the gateway's own opinion of what is in
// it.
//
// Save asks `check-config --embedded` first, and a refusal leaves both the file and
// this window exactly as they were — the text stays here to be fixed, and the config
// on disk is still one the gateway starts on.

import { shell } from "./bridge.ts";

const text = document.getElementById("text") as HTMLTextAreaElement;
const problem = document.getElementById("problem") as HTMLParagraphElement;
const save = document.getElementById("save") as HTMLButtonElement;
const cancel = document.getElementById("cancel") as HTMLButtonElement;

text.value = await shell.readConfig();
text.focus();

save.addEventListener("click", async () => {
  save.disabled = true;
  problem.classList.add("hidden");
  const result = await shell.saveConfig(text.value);
  if (result.ok) {
    window.close();
    return;
  }
  problem.textContent = result.error;
  problem.classList.remove("hidden");
  save.disabled = false;
});

cancel.addEventListener("click", () => window.close());
