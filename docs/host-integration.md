> English translation of the Chinese document. Original: [host-integration.zh-CN.md](host-integration.zh-CN.md). Technical review pending.

# Host integration

`mixum-ctl` is the protocol-v1 host client. It connects to mixumd over
`wss://host:port/ws`, sends `hello`, then encodes every control request as an
envelope and correlates replies by `replyTo`. Server-pushed `task.event`s go to the
event channel; binary video frames go to the frame channel.

The first connection computes the SHA-256 fingerprint of the server's end-entity
certificate and stores it in `~/.mixum/known-hosts` with mode 0600. The same address
must later present the same fingerprint; `--fingerprint` lets the first pairing
specify it explicitly. Token files are also 0600, and the client never writes tokens
to logs.

Fingerprints are pinned per address. Resolution order: explicit `--fingerprint`, the
known-hosts entry for that address, then the fingerprint of a paired device
(`remote`) in the config. With multiple nodes, `connect` to a new node does not
affect the verification of other addresses; after certificate rotation, run
`connect --fingerprint` against the target address again to re-pair.

The lid-close migration path: a power event (or `mixum-ctl power --simulate-sleep`)
calls `snap.create` (`hybrid`, `auto=true`), then `snap.push` sends the node and
missing blocks to the remote. The peer handles block dedup and tree insertion per
protocol §6; a second push should produce no block traffic.

For manual cross-node push, the source token uses the global `--token`; the target
token uses `snap push ... --remote-token <token>`, so the two endpoints' tokens do
not get mixed up.

`connect --token-path <file>` only stores the token file path; `connect --token
<value>` is used for the current handshake only and is never written to config.

Simulated lid close also supports `power --simulate-sleep --remote <addr>
--remote-token <token>`; only when the target token is omitted does it fall back to
the source connection token.

On macOS the official integration point is IOKit's `IORegisterForSystemPower`; the
callback does not require root. `power --watch` semantics:
`kIOMessageCanSystemSleep` (idle countdown) is allowed through without migration;
`kIOMessageSystemWillSleep` triggers lid-close migration (the same path as
`--simulate-sleep`), and the system stays awake until migration completes or the
deadline expires (`--deadline-secs`, default 30 s) before returning
`IOAllowPowerChange` — even on failure or timeout it still allows sleep, so the
machine cannot hang. Linux uses the systemd `sleep.target` system-sleep hook:
`power --install` writes `/usr/lib/systemd/system-sleep/mixum-ctl` (the pre stage
runs migration synchronously, 20 s deadline, log at `/var/log/mixum-power.log`);
`power --print` previews and `power --remove` uninstalls. The hook runs as root; point
`MIXUM_CONFIG_DIR` at the user's `~/.mixum`, and run `connect` first so remote and
token_path are configured.

Recommended file transfer usage for agents:

```sh
# host → node; stdout prints only the remote path, easy for an agent to parse
path=$(mixum-ctl file put ./input.bin --name input.bin)
mixum-ctl task submit "处理 $path" --agent shell --param cmd="cat $path"

# node → host
mixum-ctl file get "$path" --out ./result.bin
```

`file.put` defaults to `workRoot/transfers/<name>` and returns the absolute path;
`--json` gives a structured result. Drag-and-drop into a Tauri window uses the same
channel; after upload the remote path is injected into the target window as text.

## Local viewer

The local viewer listens on `127.0.0.1:7602` by default. The admin page (`/`) polls
the window list every 2 s; clicking a window opens `/view/<id>` in a **separate
popup** — one independent view per container window. The title bar shows the window
title and takeover state badge; `display.attach` subscribes to png at fps 30 (with
damage gating, a static window has zero traffic; attach forces a first frame), draws
full frames and damage patches using the protocol's 27-byte frame header, resizes to
the content size after the first frame, and reconnects automatically within 1 s after
a disconnect. The view page supports **takeover and input injection**: the
takeover/release button goes through `display.takeover` (force takeover of an
agent-owned window needs confirmation); in user mode, mouse (motion/button/wheel) and
keyboard (keysym + modifiers; F5/F11/F12 stay with the browser) are injected through
`display.input`, with coordinates mapped from the canvas scale to physical pixels.

## IME and text input

After clicking inside the canvas, the **host system IME** can be used for Chinese and
other non-ASCII text: the page positions a hidden input at the click point, receives
the composition result, and on `compositionend` submits
`display.input {type:"text"}`. mixcomp hands it to the focused window through
text-input-v3's `commit_string` (composition and candidates happen on the host; no IME
is needed inside the container). This only covers Wayland-native apps that implement
text-input-v3; XWayland/X11 clients use XIM and are not covered yet.

Uplink control envelopes go through the stream WS's allowlisted channel (only
`display.input`/`display.takeover`/`display.list`/`audio.attach`/`audio.detach`; the
server holds the full credential, and high-privilege ops such as snapshots are not
allowed). `display record` saves the selected window's JPEG frames as
`frame-0000.jpg`, etc.

## Audio

The console window toolbar's audio button subscribes to container audio through
`audio.attach` (s16le 48 kHz stereo, 20 ms chunks) and queues it in WebAudio; clicking
mute again or leaving the canvas sends `audio.detach`. The container needs an audio
service and a capture command (server `audioCommand`, e.g. `parec`). The dist image
provides a null sink `mixum` via `pulseaudio-mixum.service`, and the default
`audioCommand` captures from `mixum.monitor`; the view page also has an audio button
and volume slider. The client advances its playback clock from audio timestamps:
60 ms lead by default, stepping by PTS; if it falls behind/accumulates more than
500 ms or timestamps are abnormal, it resets the scheduling point. The toolbar also
has an A/V offset slider (-200 to +200 ms) to align picture and sound. Badges show
cumulative frames, current lead and dropped frames.

## Security boundary

TLS 1.3 only; self-signed certificates rely on fingerprint pinning, not the system CA;
TOFU and config files must not live in a shared directory; the view service binds to
loopback only, and port 7602 must not be exposed to the LAN in production.
