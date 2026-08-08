// The gate every other file in this client depends on not having to check.
//
// Worth its own test precisely because nothing else tests it: once this returns true
// the rest of the client assumes a secure context everywhere, with no branch left to
// exercise. If it ever returned true on an insecure origin the failure would surface
// as WebCodecs, the clipboard and the keyboard all going missing separately, which is
// the state this exists to make impossible.
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
};
globals.window = {
  isSecureContext: true,
  location: { origin: "http://10.0.0.4:52380" },
};
globals.document = { createElement: element };

const { startupPermitted } = await import("./secureContext.ts");

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

test("a secure context starts the client and puts nothing on the page", () => {
  globals.window.isSecureContext = true;
  const root = element("div");
  root.children = [element("existing")];

  assert.equal(startupPermitted(asRoot(root)), true);
  // Untouched: whatever the document already had is the app's to replace.
  assert.equal(root.children.length, 1);
});

test("an insecure context refuses, and says where and how", () => {
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
