> English translation of the Chinese document. Original: [migration.zh-CN.md](migration.zh-CN.md). Technical review pending.

# Cross-device migration (`.mart` migration)

> Status: design draft, with partial implementation. The goal is to make "switch
> devices and keep working" one action inside MixumART, not a manual file drag.

## 1. What is migrated

A mixumOS node has four layers, handled separately:

| Layer | Migrated? | Carrier |
|---|---|---|
| Workspace files | ✅ | `.mart` (MAP snapshot format) |
| Snapshot tree / node metadata | ✅ | `migration.json` (tree nodes, origin, kernel/arch, guiSessions) |
| Container definition (image, env, ports, cmd) | ✅ | the `container` section of `migration.json` |
| Process memory (CRIU) | ❌ cross-device | only considered on the same machine/kernel; not cross-device |
| GUI windows | ✅ replay | `guiSessions` → relaunch the app on the target |

**Migration is not "moving a live container"**: the goal is "recreate the container
from the same image + restore the workspace + replay the windows".

## 2. Migration bundle

`MigrationBundle` (managed inside MixumART; users do not unpack it by hand):

```
migration.json          # metadata (see below)
workspace.mart          # workspace file snapshot (MAP .mart v1)
image.oci.tar           # optional: OCI layout when the target cannot pull directly
```

`migration.json`:

```json
{
  "version": 1,
  "source": {
    "host": "mac-studio",
    "runtime": "apple_container",
    "node": "map-node",
    "exportedAt": "2026-10-22T03:00:00Z"
  },
  "container": {
    "image": "ghcr.io/emergeShift/mixumos:0.1.4",
    "imageDigest": "sha256:...",
    "env": { "MIXUM_JPEG_QUALITY": "90" },
    "ports": [{ "host": 17601, "container": 7601, "protocol": "tcp" }],
    "mounts": [],
    "command": []
  },
  "snapshot": {
    "node": "01J...",
    "branch": "main",
    "label": "migration point",
    "kernel": "6.18.35-mixum-micro",
    "arch": "arm64",
    "createdAt": 1760000000000,
    "origin": "mac-studio",
    "guiSessions": [
      {
        "appId": "foot",
        "title": "shell",
        "width": 640,
        "height": 400,
        "command": "/usr/bin/foot",
        "args": ["--hold"],
        "cwd": "/var/lib/mixum/work",
        "env": { "WAYLAND_DISPLAY": "wayland-0" }
      }
    ]
  }
}
```

`.mart` carries only files (the MAP spec explicitly has no compression/encryption/
CRIU); the tree and container definition live in `migration.json`. For single-file
offline transfer MixumART can pack both into a `tar.zst`, but **the default is API
streaming**.

## 3. Protocol extension

Reuse the existing file transfer (`file.put` + `FILE` binary frames +
`file.put.commit`; `file.get`). Add two ops (a mixumOS extension until they enter MAP
v1; fields stay backward-compatible):

### `snap.export`

Request:

```json
{ "node": "01J...", "format": "mart" }
```

Reply:

```json
{ "file": "mixum-01J....mart", "size": 12345678, "sha256": "..." }
```

The file lands in `work_root/transfers/`; the client downloads it with `file.get`.
Export decompresses CAS zstd blocks into the `.mart` raw blocks (block hashes are
plain SHA-256 and can be reused).

### `snap.import`

Request:

```json
{ "file": "mixum-01J....mart", "origin": "mac-studio" }
```

Reply:

```json
{ "node": "01J...", "branch": "main", "chunks": 42, "bytes": 12345678 }
```

Import validates magic/version/header length/rkyv manifest/every block hash; missing
blocks are written to the local CAS, the node is attached to the current head (or a
specified parent), and the caller then triggers `snap.restore`.

## 4. End-to-end flow (from MixumART's point of view)

### Export

1. Connect to the source environment (WSS) and confirm agentd supports `snap.export`.
2. Choose a snapshot node → `snap.export {format:"mart"}`.
3. `file.get` streams `.mart` into the MixumART cache (with SHA-256 verification).
4. Collect the container definition (image digest, env, ports, mounts, cmd) and
   `guiSessions`.
5. Build `migration.json`; the two form a bundle (cached locally by default, not
   uploaded to a third party).

### Import

1. The user chooses the target runtime (Apple Container / Docker / WSL2).
2. Make the image available on the target: pull by digest; on failure import
   `image.oci.tar` from the bundle.
3. Create the container from the `container` section (reusing the existing
   `CreateSpec`).
4. Start the container and wait for agentd READY.
5. `file.put` the `.mart`, then `snap.import`, then `snap.restore {node}`.
6. Replay windows on the target from `guiSessions` (Task 5).
7. MixumART switches its connection profile to the new environment and re-attaches
   the window page.

### Failure and rollback

- Image unavailable: keep the source environment, report "target is missing the
  image", and do not create a half-built container.
- Arch mismatch: `snap.restore` refuses (the current arch is strictly checked);
  report that the same architecture or a rebuild is required.
- `.mart` verification failure: drop the temporary file; neither side changes.
- Restore failure: the target container stays in the "created" state and can be
  retried or removed.
- Import is idempotent: identical blocks are not rewritten; an existing node id
  returns the existing node.

## 5. Security

- `.mart` contains workspace files only; no tokens, certificates or `admin-token`.
- Transfer is WSS (TLS 1.3 + capability token); file SHA-256 is verified end to end.
- Path traversal: the `.mart` manifest rejects `..` and absolute paths (MAP spec).
- The target validates magic/version/length limits before import to avoid zip-bomb
  style headers.
- Optional: encryption of `.mart` is future MAP work; v1 does not pretend to support it.

## 6. Compatibility

- Same arch + same image: fully supported (files + tree + sessions).
- Cross-arch: only source/data can move, not binaries or processes; the `arch` check
  refuses.
- Different kernel: disk snapshots are allowed (PR #39); memory/hybrid are still
  refused.
- Docker Desktop: usable as source/target, but has no CRIU.
- `.mart` version: v1; future compression/encryption evolves through the header
  version, and the importer rejects unknown versions.

## 7. Implementation split

| Repository | Content |
|---|---|
| `mixumos` | `mixum-mart` crate (pack/open/verify/import), `snap.export`/`snap.import`, CAS bridge, tests |
| `MixumART` | `migration.json` generation, bundle cache, transfer orchestration, target container create/restore, UI action |
| `MAP` | `.mart` spec and SDK (`map-snapshot`); mixumOS follows a fixed MAP version |

## 8. Acceptance

1. Unit: `.mart` roundtrip, missing-block import, bad-hash rejection, path traversal
   rejection.
2. Integration: export→import→restore between two containers on the same machine;
   files and tree match.
3. Cross-runtime: WSL2 (same arch) → Apple Container; windows reopen from the manifest.
4. Failure injection: target missing image, arch mismatch, truncated `.mart`.
5. Performance: 1 GB workspace export/import time, peak memory, transfer dedup ratio.

## 9. Implementation progress (2026-10-22)

- **mixumOS**: the `mixum-mart` bridge + `snap.export` / `snap.import` are
  implemented; `.mart` roundtrip, corruption rejection and transfer path traversal
  all have tests.
- **MixumART**:
  - `file.get` streaming download and `snap.export/import` calls;
  - snapshot-row "Export", migration bundle cache list, import;
  - **source environment → target environment direct migration** (export→download→
    upload→import, no manual files on disk);
  - on export, read the source container with `container inspect` and generate
    `<node>.migration.json` (image reference + `@sha256` digest, env, ports,
    command, source node/branch, `.mart` checksum).
- **Not done**:
  - the target does not yet create/start the container from `migration.json` (the
    target environment must already exist);
  - `guiSessions` collection and replay;
  - mounts inspection (Apple Container currently returns empty; Docker/WSL2 pending);
  - cross-device `.mart` still needs both MixumART sides to reach their own agentd.
