> English translation of the Chinese document. Original: [wayland-magic.zh-CN.md](wayland-magic.zh-CN.md). Technical review pending.

# mixcomp: the modified Wayland compositor design

> Status: the C implementation compiles and runs in the e2e container (wlroots 0.18,
> aarch64); window streaming, input injection and takeover semantics passed
> acceptance. This page records what was modified, why, and where the boundaries
> are; the last section is the runtime bring-up log.

## 1. The problem

An agent operates Chromium inside a container; the user on a Mac/phone wants to see
**one Chromium window**, in real time, with a cursor, takeover-able, as if it were a
native app — not a VNC view of a whole Linux desktop. This is WSLg's window
experience + Parallels coherence mode, containerized.

## 2. Modifications at a glance (vs a normal compositor)

| Dimension | Normal compositor | mixcomp |
|---|---|---|
| Output | one physical display, windows stacked | **no output rendering**; each window reads the client buffer directly |
| Window management | placement/focus/decorations | no placement; the window is the capture unit |
| Cursor | wlr_cursor rendered into the scene | not rendered; coordinates travel in frame-header metadata and the host overlays them |
| Input | real input devices | virtual-pointer/keyboard protocol injection + per-window routing |
| Lifecycle | follows the session | follows the container; one frame timer per window |

## 3. Capture pipeline (v1 final)

```
xdg_toplevel map
  → per-window wl_event_loop timer (33 ms ≈ 30 fps cap)
      → wlr_surface_send_frame_done()   ← always sent; the client uses it to throttle
      → if there is a subscriber:
          surface->buffer (wlr_client_buffer)
            → texture (buffer's own, or wlr_texture_from_buffer)
            → wlr_texture_read_pixels(preferred_read_format)
            → JPEG (libjpeg-turbo, BGRA/RGBA extended colour gamut by DRM format)
            → push frame over the control channel (§5)
```

Design tradeoffs:

- **Why not a per-window headless output?** That was the early v1 plan, rejected:
  headless output + scene composition adds swapchain and pixel-readback complexity
  (0.18 removed `wlr_output_read_pixels`), while single-window capture only needs the
  client buffer's texture — one less composition layer, one less platform-specific
  spot.
- **Cost**: subsurfaces/popups (dropdown menus) are not in the main buffer and are
  lost in v1; if stage 4 really needs them, re-evaluate "per-window scene tree + own
  swapchain readback".
- **Why does the client keep drawing frames?** A common misconception: frames stop
  when nobody subscribes. `frame_done` is unconditional; while the task runs, the GUI
  app stays alive, and the picture in a snapshot is alive too.
- Resolution cap 8192²; the defensive readback buffer grows on demand.

## 4. Input injection (v1 final)

mixcomp connects to **its own** Wayland socket and becomes its own client:

```
mixumd control message {type:"input", kind, …}
  → input.c translates to protocol requests (zwlr_virtual_pointer_v1 / zwp_virtual_keyboard_v1)
  → wlroots generates standard input events (same path as real devices)
  → main.c event routing: seat notification for focus_window
      (enter → motion → button/axis/key; cross-window injection switches focus automatically)
```

- Protocol instead of internal APIs: device constructors such as `wlr_pointer_init`
  are not public API in 0.18; the protocol path behaves like ydotool/wtype with zero
  upstream coupling.
- Modifiers: client-side `xkb_state`, full press → key → release sequences; keymap
  uploaded via memfd (default evdev/pc105/us).
- Key-name resolution: keysym names (e.g. `Return`) are looked up in the client keymap
  to evdev codes.
- Cursor coordinates: known at injection time (x,y are window-local), written into the
  frame header, not rendered as a real cursor.
- Takeover arbitration is not in mixcomp: the mixumd broker decides who may inject
  (see the last section and protocol.md §4.4); the compositor only accepts approved
  messages.

## 5. Control channel with mixumd

mixcomp actively connects to `/run/mixum/display.sock` (mixumd listens). Record
format: `4-byte little-endian length + 1-byte type`; type `0x01` inline JSON control,
`0x02` video frame (payload is the frame format in protocol.md §5).

- Uplink: `window.open` / `window.update` (title, app_id, size) / `window.close`.
- Downlink: `stream.start {id,fps}` / `stream.stop` / `input {…}` / `input.mode` /
  `snapshot.freeze` / `snapshot.thaw` (pause/resume capture cadence during snapshot).
- Reconnect: driven by a 200 ms polling timer; drop frames to preserve real-time when
  sending is congested.

## 6. Build and distribution integration

- Dependencies: wlroots0.18, libjpeg-turbo, json-c, libxkbcommon (all pacman system
  deps, dynamically linked; see distro.md).
- Known build traps (for packaging):
  - pc-name differences: Alpine `wlroots-0.18` / Arch `wlroots0.18`; the Makefile
    auto-detects.
  - `xdg-shell-protocol.h` must be generated by wayland-scanner at build time (do it
    in build()).
  - `-DWLR_USE_UNSTABLE` is required.
- Multi-backend structure: multi + headless sub-backend (virtual-pointer/keyboard
  device events go through multi, same shape as sway); the anchor output is created on
  the headless sub-backend, giving clients that require at least one monitor (e.g.
  terminals) something to see.

## 7. Boundaries and next steps

- Clipboard: wlr_data_device_manager is attached; forwarding logic is stage 4.
- Concurrent multi-window streaming: per-window timers support it naturally; the
  frame rate is adjusted by the broker's `stream.start` per subscriber (adaptive in
  stage 4).
- X11 apps: via XWayland later (v1 only promises Wayland-native apps; Chromium
  qualifies).
- wlroots 0.19 upgrade: the capture pipeline sits at the client-buffer layer, so API
  changes should affect it little.

## 8. Runtime bring-up log (2026-10-04)

mixcomp ran stably in the e2e container (Alpine 3.21 + wlroots 0.18.1, aarch64):
the e2eclient printed "window stream OK (≥3 frames)"; all twelve
`tests/integration/displaytest` items passed — frame-header cursor coordinates
visibly change with pointer_motion injection ((20,10)→(100,50)), and takeover
claim/release/unknown-action conflict/nonexistent-window error codes all match
expectations. `-Wall -Wextra` warnings are zero. Below are the per-item root causes so
nobody steps on them again.

### 8.1 Injected client segfault (hand-written protocol table signature order)

gdb located the crash in `wl_proxy_marshal_flags` while the registry callback created
the virtual pointer. Root cause: input.c's hand-written
`zwlr_virtual_pointer_manager_v1` protocol table declared `create_virtual_pointer` as
`"no"`, while upstream XML is `"on"` (seat first, new_id last). libwayland's
`wl_argument_from_va_list` also consumes one vararg for `'n'` (new_id) (the generated
stub passes NULL and `create_outgoing_proxy` overwrites it); with the wrong order the
`'o'` slot reads out of bounds into register garbage. Serialization dereferences the
garbage pointer, whose contents vary with register state — exactly NULL takes the
clean `err_null` path, non-NULL dereferences and segfaults, explaining "sometimes
crashes, sometimes not". The same table also had: manager destroy opcode off by one
(upstream is 1, not 2), frame/axis_source opcodes swapped, and keymap declared `"ihu"`
while the call passes (fd,size,format) — libwayland treats size as an fd per `"ihu"`,
gets EBADF, and drops the keymap request.

Fix: drop hand-written protocol tables; the Makefile uses wayland-scanner on
`mixcomp/protocol/*.xml` to generate client headers and private code; injection calls
go through generated stubs, and new_id vararg placement/opcode order are guaranteed
by the generator. The XML is from the same source as wlroots 0.18.2's protocol/
directory — upstream wayland-protocols removed virtual-keyboard, and Alpine 3.21 ships
no package with that XML, so vendoring is the only stable source. The keymap fd is
copied by libwayland during marshalling, so close it after the request is sent (the
old comment's "dup at sendmsg" was wrong and is corrected).

### 8.2 wl_seat name event triggers wl_abort

The injected client binds wl_seat v5+, so the server must send a name event; since
libwayland 1.22, `wl_closure_invoke` aborts on a NULL listener slot (older versions
warned and skipped). The listener had `.name = NULL`, so the injected client crashed
on the first dispatch. Fix: add a no-op name handler.

### 8.3 foot "no monitors available" (wl_output global no longer auto-created)

Since wlroots 0.18, enabling an output no longer auto-creates the wl_output global; the
compositor must call `wlr_output_create_global` explicitly. The anchor output existed
but clients could not see it. Fix: create the global explicitly after a successful
commit.

### 8.4 Client disconnected on first frame ("Unknown buffer type")

wlroots 0.18 removed `wl_display_init_shm`, but libwayland-server still exports the
symbol for compatibility (legacy implementation). mixcomp called it by mistake; the
wl_shm global's buffer implementation pointer landed inside libwayland, and wlroots's
`wlr_buffer_try_from_resource` only accepts its own implementation, reporting Unknown
buffer type and disconnecting the client. A minimal xdg client can reach configure, so
the bug only surfaced with a real client that interacts deeply with wlroots (foot).
Fix: use `wlr_shm_create_with_renderer`.

### 8.5 Windows never register, configure never appears

Two 0.17→0.18 semantic changes stacked:

1. `xdg_shell.events.new_surface` now fires only once at get_xdg_surface, when role is
   always NONE; the role-bearing entry is the `new_toplevel` signal (same as tinywl
   0.18). The old filter `role == TOPLEVEL` on new_surface silently dropped every
   window.
2. After the initial commit the compositor must send an empty configure; only after
   the client acks and attaches its first buffer is the surface mapped (0.18 no longer
   sends the first configure for you).

Fix: hook new_toplevel; the window commit callback calls
`wlr_xdg_toplevel_set_size(xdg_toplevel, 0, 0)` for `initial_commit` (the client
chooses the size, matching mixcomp's no-placement positioning).

### 8.6 Injected events never received, coordinates always 0

1. In 0.18 the virtual pointer/keyboard devices do not go through the backend's
   new_input; listen to each manager's `new_virtual_pointer` / `new_virtual_keyboard`
   signals separately. The code had hooked backend new_input, so routing listeners
   were never attached.
2. `wlr_pointer_motion_absolute_event` coordinates changed from layout-absolute to
   normalized [0,1] values (the event no longer carries extent); routing must convert
   back to window-local coordinates using the window size.

### 8.7 Client-side constraints of the e2e test path

(Recorded 2026-10-04; the old Go e2eclient was removed with the reference
implementation; constraint evolution is in `tests/integration/`.) The old e2eclient
(gorilla/websocket) had a 4096 default read buffer; a larger single frame triggered a
host-side bufio out-of-bounds panic, and its frame-loop read deadline could invalidate
the connection; concurrent reads of the same ws from the frame and reply loops are
also unreliable. Workaround: the entrypoint starts foot with
`initial-window-size-pixels=300x200`, so a frame is ~1.5 KiB; injection/takeover
acceptance moved to `tests/integration/displaytest` (single read loop, 1 MiB read
buffer, wire layer hand-written per protocol.md, doubling as cross-protocol
validation). displaytest also found two agentd-side issues: the error code for
injecting into a nonexistent window — the Rust implementation now returns `not_found`
per §4 semantics (2026-10-17; previously all injection errors mapped to conflict).
Window agent-mode switching is fully wired in the Rust implementation (checked
2026-10-17): task.rs active count 0↔1 → bin callback → `broker.set_tasks_active`
(free↔agent) → inject hard-refuses in agent mode, claim needs force; mixcomp only
records `input.mode` (arbitration is in mixumd, a v1 decision).
