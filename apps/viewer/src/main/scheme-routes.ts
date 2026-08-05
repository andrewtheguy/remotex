// Where a `remotex://app/…` request lands, and what this window may load.
//
// Apart from `scheme.ts` because none of it needs Electron, and the traversal
// question is the one worth answering in a test rather than in a browser: a `..`
// that got through would hand out anything the user can read.

import { isAbsolute, relative, resolve } from "node:path";
import { SHELL_PATH_PREFIX } from "../shared/contract.ts";

export const SHELL_SCHEME = "remotex";
export const SHELL_HOST = "app";
export const SHELL_ORIGIN = `${SHELL_SCHEME}://${SHELL_HOST}`;

/** The client itself. */
export const CLIENT_URL = `${SHELL_ORIGIN}/index.html`;

/** The shell's own documents, which are served beside the client, not from it. */
export function shellPageUrl(page: string): string {
  return `${SHELL_ORIGIN}${SHELL_PATH_PREFIX}${page}`;
}

export interface ShellRoots {
  /** The SPA. */
  web: string;
  /** The shell's own documents, under `/_shell/`. */
  shell: string;
}

/**
 * Resolve one request path against a root, or `null` if it escapes.
 *
 * `null` for a path that cannot be decoded at all, and for one carrying a NUL: both
 * are refusals rather than errors. A malformed `%` throws out of
 * `decodeURIComponent`, and a NUL throws out of every `fs` call that would touch the
 * result — either way an unhandled rejection inside the scheme handler, where a
 * request simply hangs instead of being answered.
 */
export function resolveUnderRoot(
  root: string,
  pathname: string,
): string | null {
  let decoded: string;
  try {
    decoded = decodeURIComponent(pathname).replace(/^\/+/, "");
  } catch {
    return null;
  }
  if (decoded.includes("\0")) {
    return null;
  }
  const target = resolve(root, decoded);
  const rel = relative(root, target);
  if (rel === "" || rel.startsWith("..") || isAbsolute(rel)) {
    return null;
  }
  return target;
}

/** Where a request lands, before any file is touched. */
export function routeShellRequest(
  url: string,
  roots: ShellRoots,
): { file: string } | { status: number } {
  const parsed = new URL(url);
  if (parsed.host !== SHELL_HOST) {
    return { status: 404 };
  }
  const path = parsed.pathname === "/" ? "/index.html" : parsed.pathname;
  if (path.startsWith(SHELL_PATH_PREFIX)) {
    const file = resolveUnderRoot(
      roots.shell,
      path.slice(SHELL_PATH_PREFIX.length),
    );
    return file ? { file } : { status: 403 };
  }
  const file = resolveUnderRoot(roots.web, path);
  return file ? { file } : { status: 403 };
}

/**
 * Whether a URL is this app's to load.
 *
 * The client never navigates: it is a single document that talks over a socket. So
 * anything that *would* navigate — a link in remote clipboard text, a redirect, a
 * `javascript:` URL typed into the inspector, even the gateway's own origin — is
 * something going wrong, and the answer to all of it is the same one.
 */
export function isShellUrl(url: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.protocol === `${SHELL_SCHEME}:` && parsed.host === SHELL_HOST;
  } catch {
    return false;
  }
}
