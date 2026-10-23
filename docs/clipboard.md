> English translation of the Chinese document. Original: [clipboard.zh-CN.md](clipboard.zh-CN.md). Technical review pending.

# Clipboard Sync (Route A) Design Draft

> **Status: design draft, not implemented.** This document only describes the protocol and implementation boundaries; the code has not been merged, and the reference implementation exists only in a fork.
> **Status: design / not implemented.** The protocol is described here; the code is not merged.
Status: draft, pending upstream review. This document only describes the protocol and implementation boundaries and contains no code changes.
The reference implementation is in the fork (`1179e72`, restored after sync at `475da2c`); see
`tests/integration/clipboard-e2e.mjs` for the e2e test.

## 1. Background

Upstream currently has a read-only `display.clipboard` request: agentd runs `wl-paste`
with a 2s timeout, always connecting to the B compositor (`XDG_RUNTIME_DIR=/run/mixum`,
`WAYLAND_DISPLAY=mixum-art-0`), and returns the current selection. It solves the MixumART
(Route B) "pull once" requirement, but as a general clipboard capability it has four gaps:

- It only covers the B compositor; windows of mixcomp (Route A) cannot use it;
- Pull only, no write, so the console cannot paste text into a container;
- No change push, so every copy requires polling;
- It is not bound to a window, so it cannot route by owner, and cannot apply window ACLs
  or agent-mode gating.

This document defines **Route A**: the comp that owns the window natively manages the seat
selection, and agentd only performs protocol routing and permission decisions. Route B keeps
its pull request as a compatibility path.

## 2. Goals and Non-Goals

Goals:

1. Bidirectional text clipboard between console ↔ container GUI, routed by window;
2. Permissions, window ACLs, and agent-mode gating follow the same rules as input injection;
3. With multiple comps (mixcomp and the B compositor coexisting), no cross-window or
   cross-comp leakage;
4. A purely additive protocol; old clients and old comps are unaffected;
5. Verifiable in CI container e2e tests (`wl-copy`/`wl-paste` as real clients).

Non-goals (v1):

- Rich MIME (images, files, `text/html`);
- Clipboard history, persistence, and cross-node sync (the gateway only passes through; the
  sync domain is within a node);
- Multi-seat / multi-user concurrency policy;
- Fallback UI when browser clipboard permission fails (handled by the console).

## 3. Route Comparison

| | A: comp-native (recommended) | B: agentd proxy (current) |
|---|---|---|
| Selection ownership | Each comp's seat; window semantics are naturally correct | agentd as an independent Wayland client |
| Coverage | Each comp can implement its own | Fixed B compositor socket |
| Write | `wlr_data_source` / Smithay selection | Requires spawning another `wl-copy` process |
| Change notification | The comp reports directly | Polling only |
| Routing/permissions | By window owner and ACL | No window context |
| Dependency | Implemented inside the comp | agentd depends on `wl-clipboard` |

Decision: add Route A; the pull semantics of B's `display.clipboard` remain unchanged and are
not extended.

## 4. Protocol Additions

### 4.1 Downstream Write

```json
{"op":"display.clipboard.set","payload":{"window":3,"text":"hello"}}
```

Replies `{ "ok": true }`. Rules:

- Requires the `display.input` capability, and the token's window ACL must cover `window`;
- Rejected (`conflict`) when the window is in `agent` mode, per the same rules as
  `display.input`;
- `text` must be UTF-8 with a length of 1..=4 MiB; an empty string or an over-limit value
  returns `bad_request` (clearing the selection is not part of v1 semantics and is left to a
  later explicit `clear`);
- A nonexistent window returns `not_found`; `conflict` when the comp does not support Route A;
- Success only means the text has been delivered to the owner comp's control channel; it does
  not guarantee the application has read it.

### 4.2 Upstream Change Event

```json
{"op":"display.clipboard.changed","payload":{"window":3,"text":"hello"}}
```

Pushed to sessions that have subscribed to `display.windows` **and whose token window ACL
covers `window`** (the subscription itself requires `display.view`). `display.view` alone is
not enough: clipboard contents are more sensitive than window imagery, and being able to see
the full screen must not imply being able to read the clipboard of every window. `window` lets
clients do window-level synchronization and echo suppression; it carries no comp id (clients
are unaware of comps). Upstream content over 4 MiB is truncated and discarded; no partial text
is pushed.

Naming note: **it is named `display.clipboard.changed`**. Upstream already has a request of
the same name, `display.clipboard`; reusing it as a push event would make the same op mean both
"pull request" and "asynchronous event", leaving clients to distinguish them only by
`id`/`replyTo`, which invites misinterpretation. The fork reference implementation reused
`display.clipboard` and must be renamed before landing.

### 4.3 Compatibility Path

`display.clipboard` (no parameters, returns `{text}`) is retained, with its semantics
explicitly being "read-only pull from the B compositor", and does no window routing. When
Route A is available, clients should prefer the window-level operations; the two do not affect
each other.

## 5. Data Flow

Downstream (console → container application):

1. The console reads the browser clipboard on focus/paste;
2. It sends `display.clipboard.set`, and the gateway passes it through to the target node
   according to `envelope.node`;
3. agentd validates capability/ACL/mode and locates the comp control channel by window owner;
4. It sends a private message to the comp: `{"type":"clipboard.set","id":3,"text":"..."}`;
5. The comp creates a built-in data source, sets it as the seat selection, and announces
   `text/plain;charset=utf-8` and `text/plain`; the container application can paste directly.

Upstream (container application → console):

1. A copy in the application triggers `request_set_selection`, and the comp's data-control
   path accepts it;
2. The comp reads the data pipe until EOF (limit 4 MiB; a 2s read timeout is recommended);
3. The comp reports `{"type":"clipboard","id":3,"text":"..."}`;
4. The broker verifies that the window owner matches the comp the message came from, and
   broadcasts the event;
5. Sessions subscribed to `display.windows` receive `display.clipboard.changed`;
6. The console writes to the browser clipboard; it uses "the most recent round of text" for
   echo suppression, avoiding an infinite console→container→console round trip.

## 6. Multi-Comp Owner Routing

- The window table is keyed by `window.id` and records `owner` (the comp connection id),
  reusing the owner rules of the existing `stream.start`/`display.input`/video frame paths;
- Downstream messages go only to the owner; when the owner disconnects the window is cleaned
  up, and no selection needs to be preserved;
- Upstream `clipboard` messages must be owner-validated; reports from non-owners are dropped
  and counted in `protocol_errors` (**the reference implementation currently lacks this check
  and it must be added before landing**);
- This assumes `window.id` is unique within the broker: first claimant wins, and same-id
  updates from another comp are ignored. This assumption already exists in the current window
  table and is not changed here;
- Comp capabilities: **v1 requires** a comp to report
  `{"type":"capabilities","clipboard":true}` after connecting (agentd↔comp private protocol,
  not part of MAP). The broker records this flag; a comp that has not declared the capability
  replies `conflict` when it receives `clipboard.set`, avoiding "a successful reply with no
  effect". Without the capability handshake, no `set` is sent and no `changed` is broadcast,
  so old comps cannot produce false success.

## 7. Implementation Constraints (wlroots 0.18 / mixcomp)

The following are pitfalls the reference implementation hit; they apply equally when the B
compositor implements the same semantics:

1. `wlr_seat_set_selection` destroys the old data source internally (emitting the destroy
   signal and freeing the mime); the comp must not destroy it manually again and can only sync
   the pointer in the `seat.events.set_selection` callback;
2. `wlr_data_source_init` resets `base` as a whole (including `mime_types`), so MIME must be
   added after init;
3. The downstream serial must use `wl_display_next_serial(display)`. Once a self-incrementing
   small counter exceeds the display serial, subsequent client `set_selection` requests wrap
   through unsigned underflow and are silently discarded by wlroots as stale requests,
   presenting as "always fails after the first success";
4. `wlr_data_control_manager_v1` must be created; otherwise `wl-copy`/`wl-paste` and
   inter-application copy have no hook point;
5. Upstream reads use a non-blocking fd on the event loop; completion requires EOF, with
   4 MiB truncation and discard; a 2s read timeout is recommended to prevent fd leaks when the
   source holder never writes;
6. Single-selection assumption: one comp, one seat; multiple seats are left for later.

## 8. Security and Privacy

- `set` reuses `display.input` and adds no new capability bit; events go only to
  `display.windows` subscribers that already passed `display.view`;
- Window ACL and agent-mode gating happen before delivery; a rejected request triggers no comp
  side effects;
- A 4 MiB limit in both directions, counted in bytes (UTF-8); over-limit content is
  rejected/truncated at the protocol layer;
- Server and comp logs record only length and outcome, never content;
- The gateway does not parse the payload and routes only by `node`; device tokens have no
  `snap.manage`/`admin` by default, and clipboard does not broaden their permissions.

## 9. Test Plan

Unit (agentd):

- Downstream routing follows the owner; a disconnected owner returns `conflict`;
- `clipboard` reports from non-owners are dropped;
- Agent-mode rejection, ACL rejection, missing-capability rejection;
- The 4 MiB boundary and empty-string semantics;
- A comp without `capabilities.clipboard` returns `conflict`.

e2e (real Wayland clients inside the container):

- Downstream: after the console sends, `wl-paste` inside the container reads back the same
  text;
- Upstream: after `wl-copy` inside the container, the event arrives within 1s with identical
  text;
- Ownership: writes still work after the user takes over (user mode); writes are rejected
  while the agent holds the window;
- Multiple comps: open a window in each of A and B; cross-writes do not leak between windows
  (requires the B compositor side implementation);
- Regression: damage gating, H.264 negotiation, and `display.windows` subscription are
  unaffected.

Browser: an end-to-end case for reverse sync of console focus/paste, verifying that echo
suppression sends back only once.

## 10. Landing Breakdown

Recommended split into commits within a single upstream PR:

1. `mixum-protocol`: add the op constant and documentation (this file + `protocol.md` §4.4);
2. `mixum-agentd`: broker owner routing, upstream owner validation, server permissions and
   event push, unit tests;
3. `mixcomp`: `clipboard.c`, `stream.c` dispatch, data-control manager, Makefile dependencies
   (`wlroots` already provides them);
4. `console`: `clipboardSet`, event listeners, and echo suppression;
5. `tests/integration/clipboard-e2e.mjs` and documentation write-back.

Reference implementation: fork `1179e72` (original), `475da2c` (Route A restored after sync),
`tests/integration/clipboard-e2e.mjs`.

## 11. Review Conclusions (2026-10-22)

1. The upstream event uses **`display.clipboard.changed`**; the fork's reuse implementation
   must be renamed before merge.
2. **A comp capability handshake is required**; comps that do not declare `clipboard` always
   get `conflict` and never produce false success.
3. **Empty strings are rejected**; clearing the selection is not in v1 and will later be
   defined with an explicit `clear`.
4. Events must be additionally filtered by the token window ACL and must not rely on
   `display.view` alone.
5. Still to be scheduled: the Route A implementation for the B compositor; until then, `set`
   on its windows returns `conflict`. Rich MIME (`text/html`/images) is left for v2.
