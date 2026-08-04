// `remotex://app`: the origin the client lives at.
//
// The page is served out of this bundle rather than fetched from the gateway, and
// the reason is `localStorage`. The gateway listens on whatever port the kernel
// hands it, so loading from `http://127.0.0.1:<port>` would put the client at a new
// origin on every launch — and the three preferences it remembers would be dropped
// each time, silently, with nothing on screen to say a preference had been lost.
// This origin holds still.
//
// The cost is that the page then calls its own gateway cross-origin, which is what
// `shell_origin_cors` in `src/server.rs` answers, for this exact literal string and
// no other.
//
// The routing itself is in `scheme-routes.ts`, where it can be tested.

import { existsSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { net, protocol } from "electron";
import {
  routeShellRequest,
  SHELL_SCHEME,
  type ShellRoots,
} from "./scheme-routes.ts";

/**
 * Declare the scheme before the app is ready, which is the only time this may be
 * called.
 *
 * All four privileges are load-bearing, and each buys one thing:
 *
 * - `standard` makes it a real origin, which is what `localStorage` is keyed on;
 * - `secure` makes it a secure context, which WebCodecs requires — without it the
 *   client can decode neither audio nor a video stream;
 * - `corsEnabled` and `supportFetchAPI` are what let a document here reach the
 *   gateway on loopback at all.
 */
export function declareShellScheme(): void {
  protocol.registerSchemesAsPrivileged([
    {
      scheme: SHELL_SCHEME,
      privileges: {
        standard: true,
        secure: true,
        supportFetchAPI: true,
        corsEnabled: true,
      },
    },
  ]);
}

/**
 * Serve the bundle at `remotex://app`.
 *
 * An unknown path is a 404 rather than the index: this client has no router, so a
 * silent fallback would turn a hashed asset that failed to ship into a blank window
 * instead of an error naming the file.
 *
 * The file's type comes from `net.fetch`, which reads it the way the browser already
 * does. Nothing here keeps a table of extensions.
 */
export function serveShellScheme(roots: ShellRoots): void {
  protocol.handle(SHELL_SCHEME, async (request) => {
    const routed = routeShellRequest(request.url, roots);
    if ("status" in routed) {
      return new Response(null, { status: routed.status });
    }
    if (!existsSync(routed.file)) {
      return new Response(`Not found: ${request.url}`, {
        status: 404,
        headers: { "content-type": "text/plain; charset=utf-8" },
      });
    }
    return await net.fetch(pathToFileURL(routed.file).toString());
  });
}
