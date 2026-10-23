> English translation of the Chinese document. Original: [architecture.zh-CN.md](architecture.zh-CN.md). Technical review pending.

# mixumOS architecture

> Status: design finalized (2026-10-04).
> This page answers: what mixumOS is, what it is made of, which language each
> component uses, and why.

## One-sentence definition

mixumOS is an **agent operating-system distribution** based on Arch Linux: it turns
a container into an agent's runtime, turns every agent action into a system event
that can be rolled back, forked or migrated, and streams the GUI the agent operates
as native windows to the user's phone or computer.

## Two hard rules

1. **A distribution, not a pile of binaries.** Every component is packaged with a
   PKGBUILD into our own pacman repository; the image (assembled rootfs / OCI) is an
   output of the distribution, not the distribution itself. When a library gets a
   security update we bump one shared object instead of rebuilding the world.
2. **The agent is a first-class citizen.** Our agent is written in Rust and the
   runner core is Rust too; the agent embeds into the runner as a crate and shares
   types and lifecycle. Third-party agents use the external bridge protocol (see
   `agent-bridge.md`).

## Component overview

```
┌─────────────────────────── user side ────────────────────────┐
│  iOS / Android (PWA console)   macOS / desktop (mixum-ctl + console) │
└──────────────┬────────────────────────────┬─────────────────┘
               │ WSS (TLS 1.3 + capability token) │
┌──────────────▼────────────────────────────▼─────────────────┐
│                      mixum-gateway (cloud)                    │
│            k8s Ingress / operator (MixumTask CRD)             │
└──────────────┬──────────────────────────────────────────────┘
               │ scheduling (arm64 / x86_64 node pools)
┌──────────────▼──────────────────────────────────────────────┐
│                     container: mixumOS rootfs                 │
│                                                              │
│  mixum-agentd (Rust)        mixcomp (C, wlroots 0.18)        │
│  ├─ task engine (AgentDriver)  ├─ per-window isolated capture (client buffer) │
│  ├─ snapshot engine (block+tree+CRIU) ├─ virtual-pointer/keyboard injection │
│  ├─ display broker (takeover state machine) └─ Unix control channel ↕ upstream frames/downstream input │
│  └─ WSS server (protocol in protocol.md)                      │
│                                                              │
│  pacman repo: pkgs/mixum-agentd  pkgs/mixcomp  pkgs/mixum-ctl  │
└──────────────────────────────────────────────────────────────┘
```

## Language split (and why)

| Component | Language | Why |
|---|---|---|
| mixum-agentd (core daemon) | **Rust** | Same ecosystem as the agent; the agent embeds as a crate; tokio carries ws/process management/streams; a proper distro component (dynamic linking, packaged by pacman) |
| mixcomp (Wayland compositor) | **C** | Must sit directly on the wlroots C API; the wlroots ecosystem is the most mature; the modifications are contained in one auditable directory |
| mixum-ctl (host CLI) | **Rust** | Same workspace as agentd, shares the protocol crate |
| Console (PWA) | **TypeScript** | The only sensible browser-side answer; iOS/Android use it directly |
| Cloud operator | **Rust** | Same workspace as the core; kube-rs for k8s |
| Protocol/SDK | **TS + Rust crates** | The two client shapes |

**The Go reference implementation (removed 2026-10-16).** The wire protocol,
snapshot-tree semantics (rollback/fork), takeover state machine and cross-node sync
were first locked by the Go reference implementation's unit tests and container e2e,
and served as the executable spec for the Rust rewrite. Once the Rust implementation
landed, the reference implementation had done its job and was removed; the behavior
is now carried by Rust unit tests and the `tests/cross-protocol/` regression vectors
(tokens/snapshots, pre-generated).

## Key data flows

### Task flow (user instruction → agent runs)

1. The user sends an instruction from the console/phone and picks a runner (local
   container or cloud).
2. mixum-agentd receives `task.submit`, takes a transparent pre-task snapshot
   (freeze all task processes, pack the workspace, millisecond scale), then hands
   off to the AgentDriver.
3. The AgentDriver (embedded agent crate or external bridge) executes commands and
   opens GUI apps through the Sandbox.
4. GUI windows are isolated and captured by mixcomp; frames stream to the user, with
   cursor position mapped in the frame header. The display broker supports multiple
   comp connections (mixcomp and a B compositor can coexist), records the owner per
   window, and routes `stream.start/stop`, `display.input` and video frames by owner.
5. Process events (`task.event`) stream in real time; the user can take over at any
   point (`display.takeover`).
6. When the task ends, state and artifacts are persisted; on failure it can roll
   back to the pre-task snapshot according to policy.

### Snapshot flow (cross-device migration)

1. Before the Mac lid closes, mixum-ctl's power hook triggers a hybrid snapshot
   (disk + CRIU memory).
2. The snapshot is stored as content-addressed blocks; only blocks missing on the
   remote are pushed (zstd compression + dedup).
3. The cloud container imports the node into its own snapshot tree (origin marks the
   source) and the task continues.
4. The next time the user opens the Mac, a reverse pull brings it back locally.

## Directory layout

```
mixumos/
├── docs/                  design documents (this directory)
├── rust/                  Rust workspace (core; the main rewrite)
│   └── crates/
│       ├── mixum-protocol protocol layer: envelope/ops/capability tokens/frame header
│       ├── mixum-snapshot snapshot engine: block store/tree DAG/CRIU/sync
│       ├── mixum-agentd   in-container daemon (task engine/display broker/WSS)
│       ├── mixum-ctl      host CLI (power hook/snapshot push)
│       └── mixum-operator cloud k8s operator (later)
├── mixcomp/               C/wlroots compositor (see wayland-magic.md)
├── dist/                  distro build (see distro.md)
│   ├── pkgs/              PKGBUILD (mixum-agentd / mixcomp / mixum-ctl
│   │                      / mixum-base / wlroots0.18 built locally)
│   ├── repo/              pacman repository tools
│   └── os/                rootfs and image build (pacstrap + OCI packaging;
│                          mkosi is replaced by equivalent scripts; bootable disk images later)
├── console/               PWA console (later)
├── cloud/                 k8s manifests (later)
├── tests/
│   ├── integration/       container e2e (Rust)
│   └── cross-protocol/    cross regression vectors (tokens/snapshots, JSON)
└── proto/v1/              machine-readable protocol forms (JSON Schema, later)
```

## Non-goals (explicitly out of scope for v1)

- Multi-user / multi-tenant isolation (v1 is single-user, multi-device; the tenant
  model is later in the gateway layer);
- H.264/AV1 hardware encoding for the graphics protocol (v1 uses JPEG; the WebCodecs
  upgrade path is left open);
- Non-container shapes (bare-metal installer); Windows/macOS as container hosts
  (they are clients only).

## Rust implementation record

`rust/crates/mixum-agentd` implements the daemon described here and in
`docs/protocol.md`: WSS (TLS 1.3), capability-token handshake, request routing,
50/s (burst 100) rate limiting, task engine, external line-delimited JSON bridge and
window broker, all using the protocol crate's types. The snapshot backend is wired
to `rust/crates/mixum-snapshot` (directory CAS + tree, assembled via the
`real-snapshot` feature; a default in-memory stub keeps branches independently
buildable). Cross-node sync (protocol §6) is implemented: `push.import`
reconciliation, 0x03 block-stream hash verification into CAS, commit into the tree
with a remote-root marker, and a WSS client on the push side (self-signed
certificate verification skipped, same as the old Go `client.go`).

The Go reference implementation was removed (2026-10-16); the table below remains as
a behavior-provenance map:

| Behavior | Old Go reference | Rust implementation | Difference |
|---|---|---|---|
| envelope, tokens, video frame header | `reference/internal/wire`, `auth` | `mixum-protocol` | |
| task state and events | `reference/internal/task` | `mixum-agentd::task` | |
| snapshot tree | `reference/internal/snap` | `mixum-snapshot` adapter (`RealSnapshotEngine`) | |
| cross-node sync | `handlers_snap.go`, `client.go` | `mixum-agentd::sync` | `snap.pull` has no handler, same as the old reference (pull = reverse call to the peer's push) |
| display control channel | `reference/internal/display` | `mixum-agentd::display` | |
| admin REST/metrics | later capability | not implemented | explicitly excluded by the task |

Assembly semantics: `--config` accepts both space and equals forms; an empty config
falls back to defaults; the snapshot engine dataDir=`dataDir/snapshots`,
root=`workRoot`, skip=`.mixum`; the bootstrap admin token lasts 10 years; the
snapshot freeze callback freezes all task process groups.

Verification record (2026-10-05):

- e2e: `tests/integration/Dockerfile.e2e-rust` (mixum-agentd + mixcomp + foot,
  musl arm64), the in-container Go e2eclient passed the full path — handshake/
  system info (backend=directory)/window stream ≥3 frames/task/snapshot tree/
  rollback/fork. The original e2eclient's task.submit timeout with a window present
  was its own concurrent-read defect (recorded in `wayland-magic.md` §8.7; the Go
  server reproduces it too), and the fixed client (single read loop) reports
  `E2E PASS` against both Rust and Go servers;
- real-file rollback: create a file in the container → snap.create (chunks=1,
  size=19) → roll back to genesis → file disappears;
- cross-implementation push: the Rust pusher pushes a node created by the Rust
  engine into the Go reference container; the node appears in the Go tree (id/branch
  preserved; root has no remote-root marker); the two-node loopback is in
  `rust/crates/mixum-agentd/tests/sync_loopback.rs`;
- 42 unit tests (including the real engine) + 18 protocol-crate tests all green;
  clippy `-D warnings` clean in both configurations.
