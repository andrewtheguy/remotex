// The seam between the shell and the page it shows, spelled once.
//
// The payload types are imported straight out of the client rather than restated
// here. That is the whole point of the import: the Swift shell this replaced kept
// its own `NativeState.swift` and `NativeCommand.swift` in step with the client by
// hand, and they drifted. A rename in `nativeHost.ts` is now a type error in this
// tree, before anything is built.
//
// Type-only, so nothing of the client's is bundled into the main process — the
// import is erased.
//
// From `nativeHost.contract.ts` rather than `nativeHost.ts`, and that is not a
// detail: the second one imports React, so type-checking this tree against it
// needs the client's `node_modules` to be installed. That is true on any machine
// where somebody has worked on the client and false on a CI runner that installed
// only this package — green here, `Cannot find module 'react'` there.

export type {
  NativeCommand,
  NativeEvent,
  NativeState,
} from "../../../../frontend/src/nativeHost.contract.ts";

/** The IPC channels, and the whole of them. */
export const CHANNEL = {
  /** renderer → main: one `NativeEvent`, fire and forget. */
  event: "remotex:event",
  /** main → renderer: one `NativeCommand`. */
  command: "remotex:command",
  /**
   * renderer → main, **synchronously**, from the preload.
   *
   * `gateway.ts` in the client reads `window.__remotexGateway` at module load, so
   * the value has to exist before the first script in the document runs. Preload
   * execution is the only moment guaranteed to precede that, and a synchronous
   * answer is the only kind that is ready by the end of it.
   */
  gateway: "remotex:gateway",

  /** main → the launch page: what to show while there is no client. */
  shellStatus: "remotex:shell:status",
  /** the launch page → main: which of its two buttons was pressed. */
  shellAction: "remotex:shell:action",
  /** the configuration editor → main: read the file, or check and save it. */
  configRead: "remotex:config:read",
  configSave: "remotex:config:save",
} as const;

/** What the launch page shows. */
export type ShellStatus =
  /** The gateway is starting; there is nothing to say yet but the name. */
  | { phase: "starting"; branding: string }
  /**
   * It did not start, or it stopped. `message` is this app's sentence about it and
   * `log` is the gateway's own stderr, verbatim — never summarised, because it
   * names the target, the key or the line that is wrong.
   */
  | { phase: "failed"; branding: string; message: string; log: string };

/** The launch page's two buttons, plus the one the menu bar shares with it. */
export type ShellAction = "retry" | "configure";

/** The configuration editor's answer to Save. */
export type ConfigSaveResult =
  | { ok: true }
  /** The gateway refused it, in the gateway's own words. Nothing was written. */
  | { ok: false; error: string };
