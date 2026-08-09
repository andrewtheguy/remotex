// The one rule that decides where this extension does anything.
//
// It is written twice — as a match pattern in the manifest and as a predicate in
// `shared/origin.ts` — and the two have to mean the same thing. Chrome enforces its
// half, so what is tested here is that the predicate agrees with it: the same hosts in,
// the same answers out, including the ones nobody types on purpose.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  COMPANION_MATCH,
  hostLabelFor,
  isCompanionUrl,
} from "../src/shared/origin.ts";

test("a gateway under the served suffix is served, on any port", () => {
  // The port is what tells two gateways apart, and a match pattern cannot express one.
  // Every port on these names is this extension's, which is the design: nothing else
  // answers on `.remotex.localhost` at all.
  assert.equal(isCompanionUrl("http://gw-a.remotex.localhost/"), true);
  assert.equal(isCompanionUrl("http://gw-a.remotex.localhost:52380/"), true);
  assert.equal(
    isCompanionUrl("http://gw-b.remotex.localhost:52676/target/2?a=b#c"),
    true,
  );
});

test("the bare suffix counts, because Chrome's pattern matches it too", () => {
  // `*.remotex.localhost` covers the domain as well as its subdomains. A predicate that
  // said otherwise would call a window Chrome had injected into "not ours".
  assert.equal(isCompanionUrl("http://remotex.localhost:52380/"), true);
});

test("a deeper name under the suffix is still under it", () => {
  assert.equal(isCompanionUrl("http://x.gw-a.remotex.localhost/"), true);
});

test("https is not served, because the gateway has no TLS listener", () => {
  // The dev redirect always sends a browser to http://, and a `.localhost` name is a
  // secure context regardless — so a second scheme would be a second pattern to keep in
  // step for a URL nothing produces.
  assert.equal(isCompanionUrl("https://gw-a.remotex.localhost/"), false);
});

test("every other host is not served, however much it reads like one", () => {
  for (const url of [
    undefined,
    "",
    "not a url",
    "http://localhost:52380/",
    "http://127.0.0.1:52380/",
    "http://[::1]:52380/",
    // The suffix is what is reserved. These only contain the words.
    "http://remotex.localhost.example.com/",
    "http://notremotex.localhost/",
    "http://gw-a.remotex.localhost.evil.com/",
    "https://gateway.example.com/",
    "about:blank",
    "chrome://extensions",
    "chrome-extension://abc/popup.html",
    "file:///Users/andrew/notes.txt",
    "data:text/html,<p>hi",
    "javascript:void 0",
  ]) {
    assert.equal(isCompanionUrl(url), false, `${url} should not be served`);
  }
});

test("the match pattern is the predicate's own suffix, spelled Chrome's way", () => {
  // Not a tautology: it is the one line that would have to change in both places, and
  // this is what fails if only one of them does.
  assert.equal(COMPANION_MATCH, "http://*.remotex.localhost/*");
  assert.equal(
    isCompanionUrl(COMPANION_MATCH.replace("*.", "gw-a.").replace("/*", "/")),
    true,
  );
});

test("the label the popup shows keeps the port", () => {
  // The hostname says which cookie origin; the port says which gateway. The popup shows
  // the pair, because that is the one a person recognises.
  assert.equal(
    hostLabelFor("http://gw-a.remotex.localhost:52380/x"),
    "gw-a.remotex.localhost:52380",
  );
  assert.equal(
    hostLabelFor("http://gw-a.remotex.localhost/"),
    "gw-a.remotex.localhost",
  );
  assert.equal(hostLabelFor("about:blank"), null);
  assert.equal(hostLabelFor(undefined), null);
});
