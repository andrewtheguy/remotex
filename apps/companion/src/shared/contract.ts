// The client's wire types, reached for from one place.
//
// A type-only import across the repo, exactly as `apps/viewer/src/shared/contract.ts`
// does it and for the same reason: a rename in the client is a `tsc` error here rather
// than a popup that silently stops rendering.
//
// **`companion.contract.ts` and nothing under it.** That file imports only
// `nativeHost.contract.ts`, which imports only `protocol.ts`, and none of the three
// touches React — which matters because `bun run check` for this tree runs in CI on a
// machine where `frontend/node_modules` does not exist. Importing `companion.ts`
// instead would pull React in and fail there and only there.

export type {
  CompanionCapabilities,
  CompanionCommand,
  CompanionEvent,
  HostRemoteSize,
  NativeState,
} from "../../../../frontend/src/companion.contract.ts";

export {
  describeRemoteSize,
  EXT_SOURCE,
  isPageMessage,
  PAGE_SOURCE,
} from "../../../../frontend/src/companion.contract.ts";
