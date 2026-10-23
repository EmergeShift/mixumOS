> English translation of the Chinese document. Original: [release-notes-0.1.5.zh-CN.md](release-notes-0.1.5.zh-CN.md). Technical review pending.

# mixumOS 0.1.5 — aarch64

Status: pre-release. Released 2026-10-22. Build commit: `266b719`
(history-rewrite predecessor: `7fbbe0e`; the release tag also carries this note).

> **2026-10-24 history rewrite**: author/committer emails normalized to
> `AshReo <ash@emergeshift.com>`; commit trees unchanged; the `v0.1.5` tag moved
> from `e8afa8f` to `0ba15ae`. Full old → new map in the release assets
> `COMMIT-MAP.txt` / `commit-map.tsv`.

## Highlights

- **macOS / Apple Container kernel path opened**: `linux-mixum-micro` (kata 6.18.35
  base) boots on Apple Container with CRIU + Landlock/diag enabled; the README adds
  "switch back to the original kernel" and the capability-loss note.
- **CRIU + Wayland PoC**: dump/restore of the whole Sway + foot session tree succeeds;
  the engine supports multiple dump roots (task + mixcomp), fixes the Wayland
  half-connection and same-host rollback process-tree cleanup.
- **`.mart` migration loop closed**: mixumOS `snap.export/import` + MixumART
  export/import/new-environment migration + `migration.json` (image digest, container
  spec, guiSessions); a 336 B `.mart` migrated between two real agentd instances and
  restored file content verified.
- **GUI session manifest**: snapshots record `guiSessions`; after restore, windows are
  replayed from the manifest.
- **Audio / IME / drag-upload**: `audio.attach/detach` + s16le frames, text-input-v3
  host IME text, canvas drag-in `file.put`; the image entrypoint starts a PulseAudio
  null sink automatically when systemd is absent.
- **Landlock + cgroup v2 freezer**: sandbox write allowlist and freeze priority
  landed.
- **Clipboard Route A design**: review conclusions on event ACL, capability
  handshake, empty-string semantics and op naming.

## New / fixed

### Kernel

- `linux-mixum-micro`: 48-bit VA/PA, `CHECKPOINT_RESTORE`, `SECURITY_LANDLOCK`,
  `INET/UNIX/PACKET/NETLINK_DIAG`, FUSE/vsock/9p; Apple Container measured
  `uname -r = 6.18.35-mixum-micro`.
- `linux-mixum` full: 62 MB `Image` + modules; QEMU virt + minimal initramfs measured
  `FULL_KERNEL_BOOT_OK`.
- Build scripts: case-sensitive `/build` inside the container, argument passthrough,
  kata config base, host extraction avoiding Docker symlink EPERM.

### Snapshot / CRIU

- `Engine` supports `restore_prepare`, `set_dump_roots`, multiple `--tree`.
- agentd scans `/proc/*/comm` to include mixcomp as a dump root.
- CRIU args carry `--ext-unix-sk --file-locks --ghost-limit`.
- Fixed the occasional NotFound from parallel snapshots sharing `/tmp/mixum-criu-`.

### Migration

- `mixum-mart` vendors `map-snapshot` (Apache-2.0); CI no longer depends on a private
  repo.
- `migration.json` records image tag + `@sha256` digest, env, ports, mounts, command,
  source node/branch, `guiSessions`.
- MixumART "new environment migration": remap ports from the source spec, create/start
  the container, read admin-token, write the connection profile, import `.mart`.
- MapClient now sends `hello` and accepts `hello.ok` / `session.hello.ok`, fixing old
  images' `unauthorized: 请先 hello`.

### Audio / input / files

- `audio.attach/detach`, `0x02` s16le frames (48 kHz stereo, 20 ms).
- AudioHub reaps `sh+parec` by process group; start/stop generations isolate old
  readers.
- mixcomp `text_input.c` validates the same Wayland client, preventing illegal enter.
- Canvas drag-in files upload via `file.put` in chunks; sha256 for ≤64 MiB.
- Image entrypoint starts the PulseAudio null sink automatically
  (`/etc/mixum/pulse.pa`).

## Verification

- In-image package versions:

```text
mixum-base 0.1.5-1
mixum-agentd 0.1.5-1
mixcomp 0.1.5-1
mixum-ctl 0.1.5-1
```

- Apple Container boot: `uname -r = 6.18.35-mixum-micro`.
- Migration e2e (0.1.5 image, two agentd):

```text
mart 336 bytes → target import → snap.restore
target file content: data
```

- Audio: `pactl` sees the `mixum` sink; `parec --device=mixum.monitor` captured
  65,668 bytes of s16le in 2 seconds.
- CRIU + Wayland PoC: Sway+foot dump→kill→restore; `swaymsg -t get_tree` still has
  `app_id: foot`.
- QEMU full kernel: serial `FULL_KERNEL_BOOT_OK`, `uname -a` is 6.18.35-mixum.
- Rust: `mixum-agentd` 49, `mixum-protocol` 23, `mixum-ctl` 14, `mixum-snapshot` 19;
  clippy `-D warnings`; console `npm run build`; mixcomp compiles against wlroots
  0.18.
- CI (main): `rust` / `rust-windows` / `console` / `mixcomp` all green.

## Known limits

- **The CRIU binary is not packaged in the image**: the kernel has
  `CHECKPOINT_RESTORE`, but `criu` must be installed in the container; Arch's CRIU
  4.2.1 parasite-SIGSEGVs on this kernel, and the PoC used Debian CRIU 3.17.1.
  Memory/hybrid snapshots are not turnkey yet.
- `linux-mixum` full cannot boot directly on Apple Container (it needs its own
  initramfs/rootfs); QEMU/bare metal work.
- This release produces aarch64 only; x86_64 was not built. The full arm64 kernel uses
  defconfig + fragments; x86_64 can use `--base arch` to pull Arch's official config.
- Cross-device automatic container creation still needs the target MixumART to run;
  current support is direct transfer to an already-connected target environment.
- Audio D-Bus warnings come from the container having no system bus; they do not
  affect null-sink capture.
- Clipboard Route A is still a design draft; B's read-only `display.clipboard` is
  unchanged.

## Upgrade notes

- After upgrading the kernel on macOS, new containers use `linux-mixum-micro`
  automatically; running containers must be restarted to switch. See the README's
  "macOS / Apple Container kernel" for switching back and capability loss.
- Old images only have the `hello` op; the new host client is compatible, so no
  container change is needed.
- For memory snapshots, first check `criu --version` in the container and use the
  kernel-fingerprint/privilege requirements.

## Release assets

| File | SHA256 |
|---|---|
| `mixumos-0.1.5-aarch64.oci.tar` | `acf7c5512c9c22eb312ceecf67bca02fbfd7bb5cf83fd01f4afa28aaf43909af` |
| `mixumos-0.1.5-aarch64-rootfs.tar.zst` | `e849a52aa70ddce251f7ef8233b3adde3e8b65201d5072885f0265a7c39cf7cb` |
| `mixumos-0.1.5-aarch64-repo.tar.zst` | `16b0d2ed9518f56da8c1f22da43b5b0dc8b91c9c129152adf05fe5a0bfe7d567` |
| `BUILDINFO-0.1.5` | `12723a81f747b82caf3c8d745f0fe7272f3b6bf5d956000449926d5f66bccecb` |
| `linux-mixum-micro-6.18.35-arm64.Image` | `859426725efd63099ce4106a84ad221f32b47ea0d9a2b738c2cc1f4635f9ca3b` |
| `linux-mixum-6.18.35-arm64.Image` | `c95413d257db24b6c75d8fd0009cedafa4d669c21ccb929dc73a32819d35ed10` |

Registry digest: use `mixumos:arm64` after importing into the local registry (the OCI
tar manifest digest build log: `85699307...`).
