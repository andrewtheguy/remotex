// A High Performance notice over the desktop holds input back: while the gateway's
// `resizing` or `screenUnavailable` is true the page sends the remote no key, and
// a key held when the notice went up is released.
//
// The gateway's word is the test's own. The session socket is passed through this
// file, which keeps the gateway's `resizing` and `screenUnavailable` from the page
// and says them itself, so when the notice is up is decided here and not by a
// Mac's display settling. That makes it run against whatever target the run is
// configured for: the page follows the word and never asks which engine said it.
//
// Everything asserted is a system decision: the `key` frames the page sent, parsed
// by this file, and whether the notice is in the DOM. The order of the frames is
// the proof, never how long anything took.
import { expect, type Page, test, type WebSocketRoute } from "@playwright/test";

import { leaveSession, logInAndConnect } from "./support";

type Notice = "resizing" | "screenUnavailable";
const NOTICES: Record<Notice, string> = {
  resizing: "Resizing…",
  screenUnavailable: "Screen not available",
};

interface Key {
  code: string;
  pressed: boolean;
}

const down = (code: string): Key => ({ code, pressed: true });
const up = (code: string): Key => ({ code, pressed: false });

interface Session {
  /// Every `key` frame the page sent on the session socket, in order.
  keys: Key[];
  /// Say `notice` to the page as the gateway would.
  say: (notice: Notice, active: boolean) => void;
}

/// Pass the session socket through, recording the keys the page sends and
/// replacing the gateway's word on both notices with this spec's. Registered
/// before navigation so nothing is missed.
async function holdSession(page: Page): Promise<Session> {
  const keys: Key[] = [];
  let toPage: WebSocketRoute | null = null;
  await page.routeWebSocket(
    (url) => url.pathname === "/ws",
    (ws) => {
      const server = ws.connectToServer();
      toPage = ws;
      ws.onMessage((payload) => {
        if (typeof payload === "string") {
          const message: Record<string, unknown> = JSON.parse(payload);
          if (message.type === "key") {
            keys.push({
              code: message.code as string,
              pressed: message.pressed as boolean,
            });
          }
        }
        server.send(payload);
      });
      server.onMessage((payload) => {
        if (typeof payload === "string") {
          const type: unknown = JSON.parse(payload).type;
          if (typeof type === "string" && type in NOTICES) {
            return;
          }
        }
        ws.send(payload);
      });
    },
  );
  return {
    keys,
    say: (notice, active) => {
      if (!toPage) {
        throw new Error("the page has opened no session socket");
      }
      toPage.send(JSON.stringify({ type: notice, active }));
    },
  };
}

/// Press and release `key` until the page has sent it: the desktop takes the
/// keyboard in an effect, which a key typed the moment it is on screen can beat.
async function typeUntilSent(
  page: Page,
  keys: Key[],
  key: string,
  code: string,
): Promise<void> {
  await expect(async () => {
    await page.keyboard.press(key);
    expect(keys).toContainEqual(down(code));
  }).toPass();
}

test.describe("a notice over the desktop", () => {
  test.afterEach(async ({ page }) => {
    await leaveSession(page);
  });

  for (const notice of Object.keys(NOTICES) as Notice[]) {
    test(`${notice} releases what was held and sends no key until it lifts`, async ({
      page,
    }) => {
      const { keys, say } = await holdSession(page);
      await logInAndConnect(page);
      const shown = page.getByText(NOTICES[notice]);
      await expect(shown).toBeHidden();
      await typeUntilSent(page, keys, "a", "KeyA");

      // Held into the notice: the release is the page's, not the keyboard's.
      await page.keyboard.down("Shift");
      await expect.poll(() => keys).toContainEqual(down("ShiftLeft"));
      say(notice, true);
      await expect(shown).toBeVisible();
      await expect.poll(() => keys.at(-1)).toEqual(up("ShiftLeft"));
      const before = keys.length;

      // That release is the listeners coming off, so these go nowhere.
      await page.keyboard.up("Shift");
      await page.keyboard.press("b");

      say(notice, false);
      await expect(shown).toBeHidden();
      await typeUntilSent(page, keys, "c", "KeyC");
      const after = keys.slice(before).map((key) => key.code);
      expect(after).not.toContain("KeyB");
      expect(after).not.toContain("ShiftLeft");
    });
  }
});
