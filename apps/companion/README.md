# RemoteX Companion

A Chrome MV3 extension beside the remotex web client. The design, and the reasoning
behind every decision here, is [`docs/companion-extension.md`](../../docs/companion-extension.md).

It adds the two things a page in a Chrome **app window** cannot do for itself:

- the system clipboard while the window is unfocused or minimised
- resizing that window to the remote's framebuffer

It does nothing in an ordinary tab, and nothing on a site the user has not turned on.

## Install

```sh
bun install
bun run build          # → dist/
```

`Load unpacked` in `chrome://extensions` takes a **directory**, and Chrome re-reads
that same path on every browser start. So point it at a copy that lives somewhere
permanent rather than at `dist/`, which the next build deletes:

```sh
cp -R dist ~/Applications/remotex-companion
```

Then open the gateway in an app window (Chrome menu → *Install page as app…*), click
the toolbar icon, and turn the site on. Chrome asks for the permission in its own
words; nothing here stores a host list.

Updating is unzipping or copying the next build over the **same** folder and pressing
Reload. Chrome derives an unpacked extension's ID from the directory path, and the
grants are keyed by that ID — a different path is a different extension with nothing
turned on.

## Develop

```sh
bun run check          # biome + tsc --noEmit
bun test tests
bun run pack           # dist-crx/remotex-companion-<version>.zip
```

`bun run check` is the one that matters most, and not for the reason it looks like:
`src/shared/contract.ts` type-imports the client's wire types and
`src/shared/geometry.ts` re-exports the viewer's arithmetic, so a rename in either
tree fails here. The release job runs it on a machine where neither of those trees has
its `node_modules`, which is why `frontend/src/companion.contract.ts` must never import
React and `apps/viewer/src/main/geometry.ts` must never import anything at all.

The icons are committed PNGs; `icons/make-icons.sh` regenerates them from the two SVGs
and needs `rsvg-convert`. Nothing in the build rasterizes anything.

## What is not here

No options page, no `chrome.storage`, no host list and no matcher. The sites this
extension runs in are Chrome's granted origins, read back with
`chrome.permissions.getAll()`; the content script is registered per grant rather than
declared in the manifest, so there is no ambient access anywhere. See the design doc
for why that was chosen over a list in the manifest, and what it costs.

Not Firefox: no `chrome.offscreen`, and no app windows.
