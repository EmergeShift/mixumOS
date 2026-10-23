> English translation of the Chinese document. Original: [macos-verification.zh-CN.md](macos-verification.zh-CN.md). Technical review pending.

# macOS on-device verification checklist

These acceptance items cannot be covered on this Windows machine: the IOKit power
hook, the real-browser experience of the local viewer, and MixumART's native
Metal window shell. This page gives the steps and pass criteria for running them
on a real Mac.

Prerequisites:

- macOS 13+, Xcode Command Line Tools (`xcode-select --install`)
- Rust (aarch64-apple-darwin), Node 20+
- Docker Desktop for Mac or Apple Container (either one)
- A reachable mixumOS node (local container or LAN machine), with the `wss://<host>:7601`
  user token and certificate fingerprint

Collect the same evidence every time: command output (copy the text), screenshots
of the key pages, and the relevant `pmset -g log` lines.

## 1. IOKit power hook (lid-close migration)

```sh
cargo build --manifest-path rust/Cargo.toml --release -p mixum-ctl
./rust/target/release/mixum-ctl connect --addr wss://<host>:7601 --token <token> --fingerprint <fp>
./rust/target/release/mixum-ctl --token <token> \
  power --watch --remote wss://<host>:7601 --deadline-secs 30
```

Pass criteria:

1. After closing the lid or `pmset sleepnow`, the log shows **CanSystemSleep → allow
   only**; it must not trigger migration.
2. The later **WillSleep** event triggers the snapshot push/migration, and the system
   stays awake until it finishes or the 30 s deadline expires.
3. When migration finishes, `IOAllowPowerChange` releases the sleep (the Sleep
   timestamp in `pmset -g log` is later than migration completion).
4. Failure injection: pull the network cable / stop the remote, then close the lid.
   After the timeout the machine must still sleep normally (not hang), and the log
   must record the timeout.
5. The remote `mixum-ctl snap tree` (or the console snapshot tree) shows a new head
   whose time matches the lid close.

`power --simulate-sleep` runs the same path without a real lid close; use it as a
smoke test first.

## 2. Local window viewer (browser)

```sh
./rust/target/release/mixum-ctl serve --bind 127.0.0.1:7602 \
  --addr wss://<host>:7601 --token <token> --fingerprint <fp>
```

Verify once in Safari and once in Chrome:

1. The admin page lists windows; clicking one opens a separate viewer page. The
   title bar shows the window title and the free/agent badge.
2. The canvas renders PNG (or falls back to JPEG). A static window has no traffic
   (damage gating); pointer/keyboard input produces a new frame immediately.
3. The takeover button: after free → user, mouse/wheel/keyboard injection works;
   F5/F11/F12 still belong to the browser.
4. Disconnect (stop the node for 3 s, then start it): the viewer reconnects and
   restores the picture within 1 s.
5. After closing the viewer page, the page WebSocket disconnects and the remote
   attach is released (a static window returns to zero-frame traffic; the node log
   shows the detach).

## 3. MixumART native window shell (Metal)

```sh
cd MixumART/apps/desktop
npm install
npm run tauri dev
```

1. Connect to agentd, scan windows and open a separate window. On macOS the default
   is the **MTKView + Metal** native overlay; with `MIXUM_NATIVE_METAL=0` it must fall
   back to the WebView canvas (compare the two renders).
2. Input injection, IME (text-input), host→remote paste, remote→host clipboard
   (Cmd/Ctrl+Alt+V).
3. An XWayland app (e.g. xterm): window display and input; X11 text uses the built-in
   XIM.
4. Multiple comp sources: A (mixcomp) and B compositor windows must both appear with
   the correct labels.
5. Lid-close scenario: a window opened before the lid closes is still visible after
   reconnecting to the migrated cloud node (snapshot/resume path).

## 4. Result template

| Item | Result | Key evidence | Notes |
|---|---|---|---|
| Lid close: CanSleep allow-only | ✅/❌ | log excerpt | |
| Lid close: migration finish/timeout release | ✅/❌ | pmset timestamps | |
| Local viewer PNG + input injection | ✅/❌ | screenshot + frame count | Safari/Chrome |
| Viewer reconnect after disconnect | ✅/❌ | logs | |
| Metal native shell render | ✅/❌ | screenshot | compare with canvas |
| Clipboard/IME/XWayland | ✅/❌ | screen recording/screenshot | |
| Window resume after migration | ✅/❌ | window screenshot | |

Known limits: the real IOKit path and the Metal shell cannot be tested in CI. The
Apple Container backend currently implements discovery/listing only; create and
start/stop are still reserved by the contract. After a pass, write the results back
to the matching roadmap item.
