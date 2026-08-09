// Which windows count as app windows: the gate for the companion seam and the client
// help that recommends installing a tab as an app.
import assert from "node:assert/strict";
import { test } from "node:test";
import { createAppWindowStore, isAppWindow } from "./appWindow.ts";

/** A window whose display mode is exactly one of these. */
function inMode(mode: string) {
  return (query: string) => ({ matches: query.includes(`: ${mode})`) });
}

/**
 * A window whose display mode moves, with the listeners a real `matchMedia` hands out.
 *
 * `change` is fired at every query rather than only the ones whose answer moved, which
 * is the pessimistic version of what a browser does and the one that would expose a
 * store that trusted the event instead of re-reading.
 */
function movableWindow(initial: string) {
  let mode = initial;
  const handlers: (() => void)[] = [];
  return {
    match: (query: string) => ({
      get matches() {
        return query.includes(`: ${mode})`);
      },
      addEventListener: (_type: "change", handler: () => void) => {
        handlers.push(handler);
      },
      removeEventListener: (_type: "change", handler: () => void) => {
        const at = handlers.indexOf(handler);
        if (at >= 0) {
          handlers.splice(at, 1);
        }
      },
    }),
    /** The display mode changing, with the `change` events not dispatched yet. */
    set(next: string) {
      mode = next;
    },
    /** The queued handlers running, which is a turn of the event loop later. */
    dispatch() {
      for (const handler of [...handlers]) {
        handler();
      }
    },
    /** What Chrome does to this document on *Install page as app…*, or on full screen. */
    become(next: string) {
      mode = next;
      for (const handler of [...handlers]) {
        handler();
      }
    },
    listening: () => handlers.length,
  };
}

test("the three app display modes are app windows", () => {
  for (const mode of ["standalone", "minimal-ui", "window-controls-overlay"]) {
    assert.equal(isAppWindow(inMode(mode)), true, mode);
  }
});

test("a tab is not, in a window or full screen", () => {
  assert.equal(isAppWindow(inMode("browser")), false);
  // The trap this is written as an allow-list to avoid. A plain tab reports
  // `display-mode: fullscreen` the moment it goes full screen, but it has not become
  // the app window the companion is allowed to serve. Read as "not browser", this
  // would have said yes.
  assert.equal(isAppWindow(inMode("fullscreen")), false);
});

test("a browser that answers nothing is not an app window", () => {
  assert.equal(
    isAppWindow(() => ({ matches: false })),
    false,
  );
});

test("a tab that becomes an app window says so, without a reload", () => {
  // The bug this exists for. *Install page as app…* reparents the live document into
  // the new window rather than reloading it, so a store answered once at load would
  // insist it was a tab until the user closed the window and launched the app again.
  const browser = movableWindow("browser");
  const store = createAppWindowStore(browser.match);
  let told = 0;
  store.subscribe(() => {
    told += 1;
  });

  assert.equal(store.get(), false);
  browser.become("standalone");
  assert.equal(store.get(), true);
  assert.equal(told, 1, "subscribers are told once, when it happens");
});

test("an app window that goes full screen is still an app window", () => {
  // The other half, and the reason this latches rather than tracking. Full screen
  // replaces the display mode with `fullscreen`, and a window that stopped counting
  // would take the companion seam down with it mid-session.
  const installed = movableWindow("standalone");
  const store = createAppWindowStore(installed.match);

  assert.equal(store.get(), true);
  installed.become("fullscreen");
  assert.equal(store.get(), true);
  installed.become("browser");
  assert.equal(store.get(), true);
});

test("a tab going full screen is not an app window, and is still listening", () => {
  // A plain tab reports `display-mode: fullscreen` the moment it goes full screen. It
  // is given no chords at all, and it may still be installed as an app afterwards.
  const browser = movableWindow("browser");
  const store = createAppWindowStore(browser.match);
  let told = 0;
  store.subscribe(() => {
    told += 1;
  });

  browser.become("fullscreen");
  assert.equal(store.get(), false);
  assert.equal(told, 0);

  browser.become("standalone");
  assert.equal(store.get(), true);
  assert.equal(told, 1);
});

test("a store that starts latched watches nothing, and one that latches stops", () => {
  // Nothing to wait for once the answer is in: an app window's store never subscribes
  // at all, and a tab's detaches when it is promoted. There is no third answer for a
  // later `change` to deliver.
  const installed = movableWindow("standalone");
  createAppWindowStore(installed.match);
  assert.equal(installed.listening(), 0);

  const browser = movableWindow("browser");
  createAppWindowStore(browser.match);
  assert.equal(browser.listening(), 3, "one per app display mode");
  browser.become("standalone");
  assert.equal(browser.listening(), 0);
});

test("an unsubscribed handler is not told", () => {
  const browser = movableWindow("browser");
  const store = createAppWindowStore(browser.match);
  let told = 0;
  const stop = store.subscribe(() => {
    told += 1;
  });
  stop();
  browser.become("standalone");
  assert.equal(told, 0);
});

test("a get between the change and its handlers still tells the subscribers", () => {
  // The ordering that costs everything if the latch is mistaken for the announcement.
  // The display mode changes, something reads the store before the queued handlers run
  // — a React render is enough — and the latch is already true by the time `promote`
  // gets its turn. A promote that read the latch as "already announced" would return
  // having told nobody, and every subscriber would sit on the wrong answer for the life
  // of the window.
  const browser = movableWindow("browser");
  const store = createAppWindowStore(browser.match);
  let told = 0;
  store.subscribe(() => {
    told += 1;
  });

  browser.set("standalone");
  assert.equal(
    store.get(),
    true,
    "the re-read latches without waiting for an event",
  );
  assert.equal(told, 0, "nothing has been dispatched yet");

  browser.dispatch();
  assert.equal(told, 1, "and the subscribers are told exactly once");
  assert.equal(
    browser.listening(),
    0,
    "with the listeners detached on the way",
  );
});
