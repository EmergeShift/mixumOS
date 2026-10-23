> English translation of the Chinese document. Original: [snapshots.zh-CN.md](snapshots.zh-CN.md). Technical review pending.

# Snapshot engine design

> Status: semantics were validated by the reference implementation and landed in
> Rust (`rust/crates/mixum-snapshot`, §9); this page remains the specification
> authority for snapshot semantics.

## 1. Three kinds of snapshot

| kind | Content | Technique | Typical time | Use |
|---|---|---|---|---|
| disk | workspace filesystem | directory packed into CAS (§2) | scales with the delta; pre-task snapshots are millisecond-scale (small hot data) | transparent pre-task snapshot, ordinary save points |
| memory | full state of the task process tree | CRIU dump | seconds | task process migration/restore |
| hybrid | disk + memory | both | sum of both | lid-close migration, YOLO full-state |

The "transparent" part of a pre-task snapshot comes from two things: the task
processes are frozen during the packing window (SIGSTOP the process group, or the
cgroup v2 freezer when available), so they do not notice; and packing only touches
the delta (after block-level dedup, unchanged files cost nothing).

## 2. Content-addressed block store (CAS)

- Blocks: fixed 1 MiB nominal chunks, the tail may be shorter; **hash = plaintext
  SHA-256**, independent of compression/encryption, so cross-device dedup is not
  affected.
- On disk: `objects/<h[:2]>/<h[2:]>`, zstd (fastest level) compressed, written to a
  temporary file then atomically renamed.
- Optional encryption at rest: AES-256-GCM, key = HKDF-SHA256(masterKey,
  salt="mixum/snap/v1:<storeID>", info="chunk-aead"), nonce prepended, AAD binds
  storeID + block hash (prevents block transplant); masterKey 32 B, generated on
  first use at `dataDir/master.key` (0600).
- Read verification: after decompression the recomputed hash must equal the block
  name; a mismatch reports corruption.
- GC: node reachability collects all referenced blocks → Sweep deletes unmarked
  blocks (§5).

## 3. File manifest

```
Manifest { version, files: [FileEntry], dirs: [DirEntry] }
FileEntry { path, mode, size, symlink?, chunks: [hash], ino? }
```

- PackDirectory: walk the workspace, chunk regular files into CAS, record symlink
  targets, record only the type for other kinds (sockets etc.); the skip list
  excludes the block store itself.
- Materialize: create directories first; write each file as "temporary + chmod +
  rename" atomically.
- Path safety: when a manifest path comes from an untrusted source (remote push),
  reject `..` and absolute paths.
- diff: compare two manifests for added/removed/changed files and new-block stats.

## 4. Snapshot tree (git-style DAG)

- Node `TreeNode`: `{ id, parent?, branch, label?, kind, auto?, createdAt, origin?,
  disk: Manifest?, memory: Manifest?, kernel, arch, sizeBytes, chunks }`; stored in a
  single-file KV store (the reference used bbolt; Rust chose sled, §9).
- **Branch is a node property, head is a pointer.** A new snapshot always hangs under
  the current head and becomes that branch's new head.
- **In-place rollback**: move head back to an old node. New snapshots produced by
  continued work naturally become new children of the old node — coexisting with the
  old timeline, i.e. a tree fork.
- **Explicit fork**: open a new branch at a node (head points at it, workspace
  unchanged); name collisions get `-2`, `-3`.
- **Return to any future snapshot**: nodes are never lost by rollback; just restore.
- On container init, an empty `genesis` snapshot is taken automatically and is the
  root of the whole tree.
- Delete: only leaf nodes, to prevent dangling references.
- Restore safety gate: nodes with CRIU memory images (memory/hybrid) require an exact
  kernel; pure disk nodes crossing kernels only log a warning and proceed (file
  semantics do not depend on the kernel). `arch` is still strictly checked.
- During restore the host freezes all task process groups and unfreezes after
  `materialize`, so `clear_root` does not race running processes.
- **GUI session manifest (OS extension)**: node metadata may carry `guiSessions`,
  recorded by the `spawn_gui` path (command, args, cwd, minimal env, pid); at snapshot
  time it is merged with mixcomp's window table by pid to produce app_id/title/
  geometry. After restore, agentd first terminates the local old processes (SIGTERM,
  wait, then SIGKILL on timeout), then relaunches from the manifest; relaunch uses the
  same Landlock/cgroup isolation as `spawn_gui`. **Automatic replay happens only when
  the node's origin is empty (local) and the snapshot is disk**: memory/hybrid restore
  already brings processes back via CRIU, so replay would duplicate windows;
  sync-pushed/`.mart`-imported nodes are skipped (empty origin is rejected on import),
  so commands in untrusted metadata are never executed. The relaunched process gets
  `env_clear()` and only PATH/HOME/USER/DISPLAY/WAYLAND_DISPLAY/XDG_RUNTIME_DIR/
  MIXUM_WORK; `MIXUM_*` tokens are neither snapshotted nor inherited. GUI processes
  launched by external drivers are not in the manifest.

### A user-facing example

```
20:00 snapshot n2 (pre-task)
22:00 workspace changes, snapshot n3
        main:  genesis ── n1 ── n2 ── n3
22:30 roll back to n2 (inplace), continue → n4
        main:  genesis ── n1 ── n2 ── n3
                                    └─ n4   ← parallel timeline
23:00 decide the 22:00 parameters were better, restore(n3, fork, "tuning-aggressive") → new branch
        tuning-aggressive: n3 → …
```

## 5. GC

The reachable set = blocks referenced by all existing nodes (only leaves can be
deleted, so there is no "deleted node still referenced" intermediate state). Sweep
removes unmarked CAS blocks. "Protect blocks from the last 10 minutes" is implemented
(`engine.rs::GC_PROTECT_WINDOW`, skip by mtime), so blocks written by an in-flight
push/import are not collected by a concurrent GC.

## 6. Memory snapshots (CRIU)

- Probe: enabled when the `criu` binary exists; exposed as `sys.info.criuAvailable`.
- dump: `criu dump --tree <pid> --images-dir <dir> [--leave-running]`, then pack the
  image directory into CAS again (attached to the node's memory manifest).
- restore: `criu restore --images-dir <dir> --shell-job`.
- Compatibility fingerprint: the node records `arch/kernelRelease` and restore checks
  them strictly (conservative gate). **Controlled experiments (2026-10-20) verified
  that a minimal static process can restore across kernels** — source 6.18 + CRIU
  4.2.1 → target 6.1.0-47 + same CRIU version; after the target control PASSed,
  cross-kernel restore PASSed twice (process count continued). The earlier
  "infeasible" result was a mixed environment and was corrected; method and evidence
  are in `docs/criu-feasibility.md` §4.4. Coverage is only a minimal static process;
  the precise fingerprint gate remains the default and is evaluated for relaxation
  per scenario.
- v1 limits: pure memory snapshots cannot be restored (disk is required); external
  fds held by processes (Unix socket to mixumd) are normalized by the task engine
  before the snapshot.
- Same-host rollback: restore first calls the host's `restore_prepare` (agentd
  terminates the current task process tree), then materializes files and runs
  `criu restore`; cross-device memory/hybrid is still refused.
- The task leader pid is written by `LocalSandbox` into a shared AtomicI32 and read by
  the snapshot engine before memory/hybrid creation; with no running task, a memory
  snapshot reports an error immediately.

## 7. Cross-device transfer

Protocol in `protocol.md` §6. Additional semantics:

- The origin field records the source hostname; the tree shows "where this branch came
  from".
- Push is idempotent: the second push has an empty missing list, zero-byte transfer;
  a new snapshot with identical content (automatic lid-close snapshot) also transfers
  zero bytes, commit still lands in the tree, and the cloud does not miss a new head.
- Transfer sends plaintext blocks only; the hash is the checksum; encryption at rest
  is each side's business.
- "Mac lid close → cloud continues" chain: mixum-ctl listens for power events →
  hybrid snapshot → `snap.push` to the cloud → the cloud container restores that node
  → the task continues; the reverse path is symmetric. On macOS the IOKit
  notification buys a few seconds before the lid closes; details in
  `host-integration.md`.

## 8. Go reference implementation map (removed)

The Go reference implementation was removed (2026-10-16); the table is kept for
concept provenance. Behavior is defined by `rust/crates/mixum-snapshot`.

| Concept here | Old reference/ location |
|---|---|
| CAS + encryption | internal/snap/chunk.go |
| manifest/pack/materialize/diff | internal/snap/manifest.go |
| tree DAG | internal/snap/tree.go |
| engine (snapshot/restore/fork/GC) | internal/snap/engine.go |
| CRIU driver | internal/snap/memory.go |
| backend probe | internal/snap/backend.go |
| push/import (protocol side) | internal/server/handlers_snap.go |
| behavior tests | internal/snap/engine_test.go, internal/server/server_test.go |

## 9. Rust implementation record

The Rust implementation is in `rust/crates/mixum-snapshot` and follows this page's
spec. The tree persistence backend is **sled**: it provides an embedded ordered KV,
transactional single-key updates and cross-platform file storage, and its interface
fits Rust better than bbolt's transaction callback. Snapshot nodes, branch heads and
the current branch live in the `nodes`, `heads` and `meta` trees. Stage 1 does not
depend on an external database service.

Mapping to the Go reference:

| Capability | Rust implementation | Notes |
|---|---|---|
| CAS | `store.rs` | zstd, plaintext SHA-256, optional AES-256-GCM; HKDF salt, `chunk-aead` info and AAD match Go |
| manifest | `manifest.rs` | fixed 1 MiB chunks, directories/symlinks, atomic materialize, path traversal rejection, diff |
| tree DAG | `tree.rs` | genesis, head, rollback, fork auto-numbering, leaf delete, branch rename |
| engine | `engine.rs` | disk/memory/hybrid, freeze hook, restore, GC; pure-memory restore refused |
| CRIU | `criu.rs` | probe `criu` at startup; still compiles on non-Linux; returns a clear error when unavailable |

Rust node IDs use a local random hex suffix; time fields remain Unix milliseconds;
JSON fields use the protocol's camelCase. Go's bbolt transaction storage and Rust's
sled file format are not interoperable, but node/manifest/block wire formats and
semantics are locked by cross vectors. GC's push-race protection is implemented
(§5, 10-minute mtime window).

### Engine latency benchmarks (2026-10-21, Linux container release build)

Repro: `cargo run --release -p mixum-snapshot --example bench`.

| Workspace | Cold pack | No change (median) | Partial change | Restore materialize |
|---|---|---|---|---|
| code: 300 × 2 KiB small files | 2388 ms | 8.8 ms | 373 ms | 689 ms |
| assets: 50 MiB single file | 74.6 ms | 44.9 ms | 90.4 ms | 122 ms |

Conclusion: **the "transparent pre-task snapshot" holds on the hot path** (unchanged
workspace 8.8 ms / large file 45 ms), large-file throughput ~640 MB/s. The weak spot
is still small-file workspaces: per-block "temp file + rename" and sled single-key
commit cost ≈2.3 ms/file (pack) / ≈2.7 ms (restore); a few hundred small files reach
seconds (the first measurement on 2026-10-17 was 1651 ms cold / 1593 ms restore; this
round restore dropped to 689 ms; numbers vary with machine and cache, use same-
environment reruns). Optimization directions (not implemented): group blocks into
batched writes, manifest reuse (reuse old blocks when ino/size/mtime are unchanged),
sled batch transactions. Cross-node transfer (lid close → cloud continue) is verified
by the two-node e2e full chain, see `tests/integration/sync-e2e.mjs`.

### Startup latency benchmarks (2026-10-21, Windows 11 + Docker Desktop/WSL2)

Repro: `node tests/integration/startup-bench.mjs --runs 10` (image
`mixumos:0.1.0-x86_64`, entrypoint explicitly `/usr/bin/mixumd --config /dev/null`;
timestamps from `docker logs -t` and `State.StartedAt`, not affected by polling
interval).

| Case | container→ready (min/median/p95) | run→ready (median/p95) |
|---|---|---|
| Cold start (new container) | 82 / 107 / 118 ms | 226 / 236 ms |
| Warm restart (docker start) | 90 / 102 / 110 ms | 175 / 185 ms |

- `container→ready` = delay from mixumd process start to listening on 7601
  (including snapshot engine open, certificate and token generation); `run→ready` =
  host-initiated docker run/start to connectable, end to end (including Docker
  CLI/daemon/container creation). Values are medians (10 runs; host load affects
  absolute values; rerun with `--runs 10` on the same machine).
- Cold and warm in-container latency is close (107 vs 102 ms), so most of the cost is
  mixumd startup; the end-to-end minus in-container difference (119 ms cold / 73 ms
  warm) is Docker CLI/daemon/container creation.
- Combined session restore: snapshot materialize (300 small files 689 ms / 50 MiB
  single file 122 ms) + agentd start (~107 ms) → roughly 0.23–0.80 s; GUI
  (mixcomp/foot) startup and host cold cache are excluded.
