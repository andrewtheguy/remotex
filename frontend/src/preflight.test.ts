// The gate every other file in this client depends on not having to check.
//
// Worth its own test precisely because nothing else tests it: once this returns true
// the rest of the client assumes a secure context and a pair of WebCodecs decoders
// everywhere, with no branch left to exercise. If it ever returned true without them
// the failure would surface as the clipboard, the keyboard, the picture and the sound
// going missing separately, which is the state this exists to make impossible.
import assert from "node:assert/strict";
import { test } from "node:test";

interface FakeElement {
  className: string;
  textContent: string;
  children: FakeElement[];
  append: (...nodes: FakeElement[]) => void;
  replaceChildren: (...nodes: FakeElement[]) => void;
}

function element(tag: string): FakeElement {
  const node: FakeElement = {
    className: tag,
    textContent: "",
    children: [],
    append: (...nodes) => {
      node.children.push(...nodes);
    },
    replaceChildren: (...nodes) => {
      node.children = [...nodes];
    },
  };
  return node;
}

const globals = globalThis as unknown as {
  window: { isSecureContext: boolean; location: { origin: string } };
  document: { createElement: (tag: string) => FakeElement };
  // Present or absent is the whole of what the module asks about them, so a
  // constructor nobody calls is enough of a decoder here.
  VideoDecoder: unknown;
  AudioDecoder: unknown;
};
globals.window = {
  isSecureContext: true,
  location: { origin: "http://10.0.0.4:52380" },
};
globals.document = { createElement: element };

const { startupPermitted } = await import("./preflight.ts");

/** A browser with everything, which each test then takes something away from. */
function capable(): void {
  globals.window.isSecureContext = true;
  globals.VideoDecoder = class {};
  globals.AudioDecoder = class {};
}

// The module wants an `HTMLElement` and uses four of its members. Narrowing the
// parameter to what it actually touches would be a type invented for this test's
// benefit; casting the fake is the smaller lie.
const asRoot = (node: FakeElement) => node as unknown as HTMLElement;

function text(root: FakeElement): string {
  const parts: string[] = [];
  const walk = (node: FakeElement) => {
    parts.push(node.textContent);
    for (const child of node.children) {
      walk(child);
    }
  };
  walk(root);
  return parts.join(" ");
}

test("a capable browser starts the client and puts nothing on the page", () => {
  capable();
  const root = element("div");
  root.children = [element("existing")];

  assert.equal(startupPermitted(asRoot(root)), true);
  // Untouched: whatever the document already had is the app's to replace.
  assert.equal(root.children.length, 1);
});

test("an insecure context refuses, and says where and how", () => {
  capable();
  globals.window.isSecureContext = false;
  const root = element("div");
  root.children = [element("a-spinner-from-index-html")];

  assert.equal(startupPermitted(asRoot(root)), false);

  const rendered = text(root);
  // The origin, because "this gateway" is ambiguous the moment someone has two.
  assert.ok(rendered.includes("http://10.0.0.4:52380"));
  // And the ways out. A refusal that does not name one is a broken deployment as
  // far as the person reading it is concerned.
  assert.ok(rendered.includes("HTTPS"));
  assert.ok(rendered.includes("localhost"));
  // Replaced, not appended: whatever `index.html` was showing is not the answer.
  assert.equal(root.children.length, 1);
  assert.equal(root.children[0]?.className, "boot-refusal");
});

test("a missing decoder refuses, and names which one", () => {
  for (const missing of ["VideoDecoder", "AudioDecoder"] as const) {
    capable();
    globals[missing] = undefined;
    const root = element("div");

    assert.equal(startupPermitted(asRoot(root)), false, missing);
    const rendered = text(root);
    assert.match(rendered, /WebCodecs/);
    assert.match(
      rendered,
      missing === "VideoDecoder" ? /\bvideo\b/ : /\baudio\b/,
      `${missing} went missing and the message did not say so`,
    );
  }
});

test("a browser with neither decoder is told about both, once", () => {
  capable();
  globals.VideoDecoder = undefined;
  globals.AudioDecoder = undefined;
  const root = element("div");

  assert.equal(startupPermitted(asRoot(root)), false);
  const rendered = text(root);
  assert.match(rendered, /video or audio/);
  // Not the origin: the address is the one thing that is fine here, and naming it
  // sends the reader to fix the deployment instead of the browser.
  assert.ok(!rendered.includes("http://10.0.0.4:52380"));
});

test("an insecure context is the reason given, even with no decoders either", () => {
  // WebCodecs is itself secure-context gated, so this is not a contrived pairing —
  // it is what every insecure origin looks like. Telling that reader to install a
  // different browser would be telling them to fix the wrong thing.
  capable();
  globals.window.isSecureContext = false;
  globals.VideoDecoder = undefined;
  globals.AudioDecoder = undefined;
  const root = element("div");

  assert.equal(startupPermitted(asRoot(root)), false);
  const rendered = text(root);
  assert.ok(rendered.includes("HTTPS"));
  assert.ok(!rendered.includes("WebCodecs"));
});
