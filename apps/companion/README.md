# RemoteX Companion

A Chrome MV3 extension beside the remotex web client. The design, and the reasoning
behind every decision here, is [`docs/companion-extension.md`](../../docs/companion-extension.md).

It adds the two things a page in a Chrome **app window** cannot do for itself:

- the system clipboard while the window is unfocused or minimised
- resizing that window to the remote's framebuffer

It does nothing in an ordinary tab, and nothing outside `http://*.remotex.localhost/*`,
which is the one host it serves and the only one it may ask Chrome for.

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

Set `dev_subdomain` in the gateway's `[server]` config, which is what sends a loopback
browser to `http://<label>.remotex.localhost:<port>/`, then open that in an app window
(Chrome menu → *Install page as app…*). There is nothing to enable: the host is in the
manifest, so the companion is running the moment the page loads.

Updating is unzipping or copying the next build over the same folder and pressing
Reload.

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

No options page, no `chrome.storage`, no grant flow and no `chrome.permissions` call.
One host permission, one `content_scripts` entry, the same pattern in both, and
`shared/origin.ts` saying it a third time for code that has a URL rather than a pattern
in hand. See the design doc for what that costs — chiefly that a gateway reached at any
other address gets no companion at all.

Not Firefox: no `chrome.offscreen`, and no app windows.
