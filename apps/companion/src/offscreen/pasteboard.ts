// Reading and writing the system clipboard from a document nobody is looking at.
//
// `navigator.clipboard.readText()` and `writeText()` both throw *"Document is not
// focused"* here, and an offscreen document is never focused — that is what it is for.
// So this is the old `execCommand` route through a textarea, which has no such
// requirement and is the reason `clipboardRead` and `clipboardWrite` are in the
// manifest at all.
//
// Text only. An image on the clipboard reads back as an empty string, which
// `ClipboardSynchronizer` already treats as "nothing to send" rather than "the
// clipboard was cleared" — the difference matters, because the second would push an
// empty clipboard at the remote every time somebody copied a picture.

import type { Pasteboard } from "../../../viewer/src/main/clipboard.ts";

export const pasteboard: Pasteboard = {
  read(): string {
    const field = document.createElement("textarea");
    document.body.append(field);
    field.focus();
    try {
      // The paste lands in the field, and the field is what is read back.
      document.execCommand("paste");
      return field.value;
    } finally {
      field.remove();
    }
  },

  write(text: string): void {
    const field = document.createElement("textarea");
    field.value = text;
    document.body.append(field);
    field.select();
    try {
      document.execCommand("copy");
    } finally {
      field.remove();
    }
  },
};
