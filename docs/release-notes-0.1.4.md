> English translation of the Chinese document. Original: [release-notes-0.1.4.zh-CN.md](release-notes-0.1.4.zh-CN.md). Technical review pending.

# mixumOS 0.1.4 — aarch64

Status: pre-release. Released 2026-10-20. Build commit: `8a6b04d`
(the release tag also carries this note).

## Highlights

- **Route B gains XWayland**: X11 toplevels, override-redirect menus, pointer/keyboard,
  X11↔Wayland clipboard, Unicode text and XIM input all work.
- **Automatic X11 environment**: agentd tasks, external drivers and login shells set
  `DISPLAY` and `XMODIFIERS=@im=mixum` from `/run/mixum/xwayland.display`; X11 apps no
  longer need hand-passed arguments.
- **File transfer**: new `file.put` / `file.put.commit` / `file.get` and the 0x05 file
  binary channel; `mixum-ctl file put/get` uploads/downloads directly, and upload
  returns an absolute path an agent can read.
- **Host-side experience**: MixumART can drag host files onto a surface, upload them,
  and inject the remote path into the target window; X11 XIM uses the built-in
  `@im=mixum` server.
- **Gateway M3**: k8s Lease node registry, bidirectional inter-replica routing,
  cross-replica dropped-node cleanup.

## Added

- `file.put` / `file.put.commit` / `file.get`, file block channel `0x05`, with chunked
  upload, size/optional sha256 verification, and `workRoot/transfers` landing.
- `mixum-ctl file put <local> [--name NAME]`: upload and print the remote absolute path.
- `mixum-ctl file get <remote> [--out LOCAL]`: download and verify size.
- `xorg-xwayland` is in `mixum-base`; the image provides `/usr/sbin/Xwayland`.
- B compositor XWayland/X11Wm, override-redirect menu composition/input, X11 clipboard
  bridge, X11 Unicode text injection, XIM server.
- Automatic X11 environment: agentd LocalSandbox/external driver and
  `/etc/profile.d/mixum-x11.sh`.
- MixumART canvas file drop: upload the file and send the remote path as text to the
  surface.

## Changed

- `display.input` forwards the Wayland `text` field.
- `display.clipboard` fetches the remote selection.
- The image adds an `xorg-xwayland` dependency (image size grows; 0.1.4 aarch64 OCI is
  ~500 MiB).
- Gateway M3 node registration/routing merged.

## File transfer usage

```sh
# host → node; stdout is the remote path, suitable for agent scripts
path=$(mixum-ctl file put ./input.bin --name input.bin)

# node → host
mixum-ctl file get "$path" --out ./result.bin

# structured output
mixum-ctl file put ./input.bin --json
```

Uploads default to `workRoot/transfers/<name>`. File operations require the `admin`
capability; `file.get` only allows paths under `workRoot`.

## Verification

- Inside `map-node-019` (0.1.4 image), `mixum-ctl 0.1.4` uploaded/downloaded a 3 MiB
  random file with matching sha256.
- X11: XClock window, Xev pointer/keyboard, override window click, clipboard, and
  `xterm` `read` receiving `XIM中文`.
- Image has `Xwayland 24.1.14`; a login shell measured `DISPLAY=:0`,
  `XMODIFIERS=@im=mixum`.
- 0.1.4 image confirmed `mixum-ctl 0.1.4`, `mixum-agentd 0.1.4`, `mixum-base 0.1.4`;
  `/usr/sbin/Xwayland` exists; a login shell measured `DISPLAY=:0`,
  `XMODIFIERS=@im=mixum`.

## Known limits

- File drop is a host→surface text/file-content channel; **full cross-VM XDND /
  Wayland data-source file drag-and-drop is still unimplemented** — after dropping a
  file, the injected content is the remote path text.
- `file.*` currently requires the `admin` capability; fine-grained file capabilities
  are not split yet.
- Non-login `docker exec` does not source the profile; use `sh -lc` or start through
  agentd.
- The CRIU cross-kernel verification PR is not in this release; cross-kernel is still
  strictly rejected by `arch/kernelRelease`.
- This release is aarch64; no x86_64 image was produced.
- The matching MixumART host is pinned to main `f7a835a` (XWayland/XIM/file drop).

## Upgrade notes

- After upgrading from 0.1.3, X11 apps can start directly through agentd/login shell;
  old scripts that explicitly set `DISPLAY`/`XMODIFIERS` remain compatible.
- The image grew because of the `xorg-xwayland` dependency tree; if you only use Route
  A, you can keep the B compositor off.

## Release assets

| File | SHA256 |
|---|---|
| `mixumos-0.1.4-aarch64.oci.tar` | `e599410b01ebc3cd64be4f58be7f699444b85c1048a65b8779cc02fdfc3e40f5` |
| `mixumos-0.1.4-aarch64-rootfs.tar.zst` | `555b31e867de92836cafa568d09fcd8fd19543c95d0fa8cbaae3ee121d00c7e3` |
| `mixumos-0.1.4-aarch64-repo.tar.zst` | `924be7072642294f82f3d5118b903e0419a59fad1da8708ece84baaa2aceb21b` |
| `BUILDINFO` | `830e43322c3bb653279b5033cf126569e739e33087f343158e84c765465587b2` |

Registry digest: `9b6d1983c45d` (`127.0.0.1:15000/mixumos:0.1.4`).
