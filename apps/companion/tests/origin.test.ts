// The one place a mistake grants more than was meant.
//
// There is no host matcher in this extension — Chrome holds the grants and Chrome
// decides what they match — so all this has to get right is turning "the window I am
// looking at" into the pattern to ask for. Table-driven, because the interesting cases
// are the URLs nobody types on purpose.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  hostLabelFor,
  isOriginPattern,
  originPatternFor,
} from "../src/shared/origin.ts";

test("an ordinary gateway becomes its own origin", () => {
  assert.equal(
    originPatternFor("https://gateway.example.com/"),
    "https://gateway.example.com/*",
  );
  assert.equal(
    originPatternFor("http://localhost/picker"),
    "http://localhost/*",
  );
});

test("the path is dropped, because the client is a SPA", () => {
  // The path changes under the content script as the user moves around the client, so
  // a grant tied to one would come and go with it.
  assert.equal(
    originPatternFor("https://gateway.example.com/target/3?a=b#c"),
    "https://gateway.example.com/*",
  );
});

test("the port is dropped, and that is a widening rather than a tidy-up", () => {
  // A match pattern cannot express a port at all. Asking for the host is the only
  // request Chrome will take, and it covers every port on it — which the popup says
  // out loud rather than leaving to be discovered.
  assert.equal(
    originPatternFor("https://gateway.example.com:8443/"),
    "https://gateway.example.com/*",
  );
});

test("an IPv6 literal keeps its brackets", () => {
  assert.equal(originPatternFor("http://[::1]:8080/"), "http://[::1]/*");
});

test("the scheme is kept, so http and https are different grants", () => {
  assert.equal(originPatternFor("http://gateway/"), "http://gateway/*");
  assert.equal(originPatternFor("https://gateway/"), "https://gateway/*");
});

test("everything Chrome would not grant is refused", () => {
  for (const url of [
    undefined,
    "",
    "not a url",
    "about:blank",
    "chrome://extensions",
    "chrome-extension://abc/popup.html",
    "file:///Users/andrew/notes.txt",
    "data:text/html,<p>hi",
    "ftp://gateway.example.com/",
    "javascript:void 0",
  ]) {
    assert.equal(originPatternFor(url), null, `${url} should be refused`);
  }
});

test("the label the popup shows keeps the port the pattern lost", () => {
  // Deliberately different from the pattern. Showing the pattern where the two differ
  // would quietly claim the grant is narrower than it is.
  assert.equal(
    hostLabelFor("https://gateway.example.com:8443/x"),
    "gateway.example.com:8443",
  );
  assert.equal(
    hostLabelFor("https://gateway.example.com/"),
    "gateway.example.com",
  );
  assert.equal(hostLabelFor("about:blank"), null);
  assert.equal(hostLabelFor(undefined), null);
});

test("a granted pattern is recognised, and a broad one is not", () => {
  assert.equal(isOriginPattern("https://gateway.example.com/*"), true);
  assert.equal(isOriginPattern("http://[::1]/*"), true);

  // The two the manifest declares as *optional*. They are never granted as such — the
  // popup only ever asks for one origin — so seeing one back means something else put
  // it there, and it is not a site to register a content script for.
  assert.equal(isOriginPattern("https://*/*"), false);
  assert.equal(isOriginPattern("http://*/*"), false);
  assert.equal(isOriginPattern("<all_urls>"), false);
  assert.equal(isOriginPattern("https://gateway.example.com/app/*"), false);
});
