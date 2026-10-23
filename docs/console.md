> English translation of the Chinese document. Original: [console.zh-CN.md](console.zh-CN.md). Technical review pending.

# Web console (console)

> Status: v1 delivered (2026-10-04), with the full path measured and passing
> against the e2e container.
> This document describes the console's architecture, the key points of its
> protocol-layer implementation, and how to integrate with it in development.

## Role

The console is the user entry point of mixumOS: submit tasks, watch a GUI
window of the agent operating in real time, and manage the snapshot tree. It
is a PWA, used directly from an iOS/Android browser or added to the home
screen; desktop browsers work equally well (a single page with three columns).

Tech stack: Vite + TypeScript + React + zustand. Dependencies are deliberately
restrained: no router, no UI component library, all styles hand-written
(`src/styles.css`), and the build output is pure static files (relative paths,
so it can be hosted under any path).

## Layout

Three columns on desktop: tasks (submission form + list + event stream) |
window canvas | snapshot tree.
On narrow screens (≤980px, phones) it collapses into three bottom tabs, with
the same set of components toggled in and out of view.

## Protocol layer (src/protocol/)

Aligned with `docs/protocol.md` v1 and the mixum-agentd implementation; the
key points:

- **envelope**: JSON in text frames, `v/id/op/seq/ts/payload`; requests and
  replies are correlated with `id`/`replyTo`; the `op.err` payload
  `{code, message}` is mapped to `ProtocolError` so the UI can handle it
  programmatically;
- **handshake**: subprotocol `map.v1` (the server also accepts the legacy
  `mixum.v1`); the op is `session.hello` (the legacy `hello` alias still
  works), the `heartbeat` from `hello.ok` drives the `sys.ping` heartbeat, and
  two consecutive failures disconnect and reconnect;
- **event shape**: `task.event.data` is always an object (`state`/`log`/`step`/
  `progress`/`error`/`input`); for compatibility the console also reads bare
  strings from older nodes;
- **binary frames**: the first byte is a channel tag; `0x01` video is parsed
  with a 27-byte little-endian frame header (protocol §5), and
  `cursorX/Y/flags` are handed straight to the canvas for overlay;
- **reconnect**: exponential backoff (starting at 1s, capped at 10s); after a
  successful reconnect it automatically re-subscribes to `task.events` and
  `display.windows`, and re-runs `display.attach` on windows that had a stream
  attached;
- **window add/remove**: `display.windows` pushes a single Window;
  `title === "__closed__"` is treated as the window closing (mixumd pushes
  with that title when it closes a window, see agentd display.rs).

High-frequency video frames do not go through zustand: `WindowCanvas`
subscribes to `client.onFrame` directly, and the store only holds
low-frequency state such as tasks, the window list, and the snapshot tree.

## Window canvas

- Stream attachment is `display.attach` (15fps); received frames keep only the
  latest one, and drawing coalesces frames with `requestAnimationFrame` —
  frames arriving during a draw are silently dropped (only the newest is
  drawn); a 120ms timer is a fallback so frames also play in embedded or
  background environments where rAF is restricted;
- canvas size = the frame header's width/height (physical pixels), scaled by
  CSS to fit the container;
- **the cursor comes from the frame header** (flags bit1); a white outlined
  arrow is drawn as an overlay rather than inferring it from the local pointer;
- takeover: `display.takeover claim/release`; when `mode=agent` a "force
  takeover" is offered (confirmation dialog, `force: true`); after takeover
  the canvas takes focus and mouse/keyboard events are sent back:
  - coordinate mapping: event position × (physical size / canvas CSS size),
    clamped to the window bounds;
  - `pointer_motion` is sent coalesced by rAF; the scroll wheel maps to
    `pointer_axis`;
  - keys map to XKB keysym names (`Enter→Return`, `ArrowUp→Up`, etc., see
    `src/util/keysym.ts`), modifier keys in `mods: ctrl/alt/shift/logo`;
- known platform limitation: a browser cannot access the peer's certificate,
  so the TOFU fingerprint pinning in protocol §1 cannot be implemented purely
  in the browser; during development the vite relay covers for it, and in
  production it is solved on the hosting side (mixum-ctl); this is noted in
  console/README.

## Snapshot tree

The node array from `snap.tree` builds its own DAG: the layer number is the
longest path to the root, nodes within a layer are sorted by (branch's first
node time, creation time), links are cubic Béziers, and rendering is SVG
(`src/util/treelayout.ts`, no graphics library). A node shows label/branch/
kind/auto marker/time/size; the current head is highlighted with a blue box
plus a head badge; branch colours come from a low-saturation palette.

How tree semantics are expressed (snapshots.md §4): a rollback (inplace) does
not delete nodes — after a rollback the new snapshot hangs under the old node,
rendered side by side with the old timeline as two branches; per the protocol,
a fork does not create a new node (the head is a pointer), and a node box's
branch attribute and the current branch name are shown as they are.

Operations: rollback (inplace), fork (a dialog to enter the branch name,
`snap.restore strategy=fork`; the server automatically appends -2, -3 on
conflict); the tree is fetched on connect and on manual refresh, the protocol
has no snapshot push, so a refresh is needed after a new snapshot is produced.

## Task view

Submission form (command, agent, key-value parameters, snapshot strategy
pre/kind/onFailure); `task.list` + `task.event` maintain the list, coloured by
state machine; selecting a task shows its live event stream (log in a
monospace font with auto-scroll; step/progress/error coloured by category);
`type=window` events provide a button to jump to the window canvas.

## PWA

- `manifest.json`: standalone, a restrained theme colour (`#f4f4f2`), three
  icons (192/512/maskable, generated with zero dependencies by
  `scripts/gen-icons.mjs`);
- `sw.js`: caches only the app shell — navigation requests are network-first
  with cache fallback, hashed static assets are cache-first; protocol data all
  goes over WebSocket and never over HTTP, so no data is cached;
- iOS meta: `apple-mobile-web-app-capable`, status bar style, title,
  apple-touch-icon; after adding to the home screen it opens in a standalone
  window.

## Development integration

See `console/README.md`: the two routes — the vite development relay (default)
or a direct connection with a trusted certificate — the fakecomp demo
container, and the `scripts/e2e-check.mjs` smoke script.

## Scope and remaining items

Scope: this module only adds `console/` and this document; `mixcomp/`, `rust/`,
`dist/`, `tests/` are read-only.

Remaining items:

- TOFU fingerprint pinning is not possible purely in the browser (platform
  limitation); awaiting the mixum-ctl hosting solution;
- the snapshot tree has no push, so a new snapshot requires a manual refresh;
- H.264/WebCodecs implemented (2026-10-17): attach negotiates the codec
  (probes VideoDecoder + isConfigSupported, Annex-B decoded directly without a
  description), the output VideoFrame is drawn straight with drawImage (zero
  copy); on decode failure it automatically falls back and reconnects as jpeg;
  the JPEG path is unchanged. For the e2e dual-codec probe see
  [tests/integration/h264-e2e.mjs](../tests/integration/h264-e2e.mjs).
