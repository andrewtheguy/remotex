// Whether this page is the one its gateway serves.
//
// The bundle is compiled into the gateway, so the two versions only differ in a
// page that outlived the gateway it was loaded from: a tab left open across an
// upgrade. Such a page speaks the previous release's wire to a gateway that has
// no older wire to answer in, so it stops and says so before it lists a target
// or opens a session, and a reload is what fetches the gateway's own bundle.
//
// The gateway states its version on `GET /api/targets` and `POST /api/session`
// (src/server.rs, `state_version`).

/** The header an answer states the gateway's version in. */
export const VERSION_HEADER = "X-Zdesk-Version";

/**
 * What to tell the user when `res` came from a gateway of another version than
 * this page's, or null when they match. An answer that states no version is not
 * this page's gateway either.
 *
 * Asked of the answers that are the gateway's own to give, its refusals
 * included: a stale page is offered neither the login (401) nor a takeover
 * (409), only the reload. Any other status is null, because it may be a proxy's
 * and is reported as what it is.
 */
export function versionMismatch(
  res: Response,
  page: string = __APP_VERSION__,
): string | null {
  if (!res.ok && res.status !== 401 && res.status !== 409) {
    return null;
  }
  const gateway = res.headers.get(VERSION_HEADER);
  if (gateway === page) {
    return null;
  }
  return gateway === null
    ? `This page is v${page} and the gateway did not state its version. Reload the page.`
    : `This page is v${page} and the gateway is v${gateway}. Reload the page.`;
}
