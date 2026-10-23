> English translation of the Chinese document. Original: [apple-container.zh-CN.md](apple-container.zh-CN.md). Technical review pending.

# Apple Container runtime bring-up record

> Date: 2026-10-18. A complete on-device record of mixumOS on the macOS host's
> Apple Container: install/start, sharing model, CRIU availability, a full MAP
> handshake, two-node whole-tree migration, and the sync zero-block-node defect
> found and fixed along the way. All numbers are local measurements; repro commands
> are included.

## 1. Test machine

| Item | Value |
|---|---|
| Model/chip | Apple M1 (arm64), 8 cores, 16 GB |
| Host OS | macOS 26.6 (25G72) |
| Container runtime | Apple `container` CLI 1.5.0 (brew; not Docker Desktop) |
| Guest kernel | kata-containers 3.32.0 → vmlinux 6.18.35 (-197-debug, 30 MB) |
| Image under test | mixumos 0.1.0 aarch64 OCI (built 2026-10-04; Arch rootfs + mixumd + mixcomp) |
| Host client | @mixum/map TS SDK (running the source directly with tsx) |
| Reference | Docker unavailable; dist rebuilt on the Windows/WSL2 machine (see distro.md) |

## 2. Install and start

```bash
brew install container
container system kernel set --recommended   # pull the kata kernel
container system start                      # first run asks interactively; scripts can echo Y |
```

Startup time (running `true` in the container end to end):

| Case | Time |
|---|---|
| Cold start (first infrastructure build) | 26.4 s |
| Warm start ×3 | **1.00 / 1.07 / 1.00 s** |

Apple's "<1 s" claim basically holds on an M1. The machine form had a transient
"Operation not supported by device" error on first start; retrying fixed it and it
does not block.

## 3. Sharing model measurement (cost of extra instances)

**Disk**: the kernel is 29 MB shared globally; the image blob (492 MB) is per
image; multiple containers from the same image use APFS clonefile copy-on-write —
`df` measured **0 MB physical growth** for a new alpine container. Note that `du`
showing 1.3 GB per container is a clone-counting artifact (`st_blocks` counts the
full size); use `df` for the real disk number. Inside the container directory,
`initfs.ext4` (640 MB nominal / 180 MB actually allocated) and the rootfs are both
clones.

**Memory** (measured with container stats):

| Container | Actual usage | Quota |
|---|---|---|
| mixumOS node (agentd + mixcomp fully on) | **37.7 MiB** | 1 GiB |
| Second mixumOS instance (bare) | 35.6 MiB | 1 GiB |
| alpine machine idle | 5.6 MiB | 8 GiB |

The quota is a ceiling, not a reservation (the host commits lazily by usage); the
rootfs uses demand paging, so a 764 MB unpacked image running bare uses only 35 MB.
**Conclusion: multiple instances on the same base share the kernel/image/blocks;
each instance's private cost is ≈40 MB of memory plus private writes.**

**Difference between the two shapes**: ephemeral containers (`container run`, the
normal MAP Node) share fully; a machine (persistent environment) gets its own sparse
virtual disk (~1 GB nominal) that grows independently with writes; `homeMount=rw`
(automatic `$HOME` mount) was confirmed via inspect.

## 4. CRIU availability (the macOS branch of the snapshot fallback ladder)

Reading the kernel config directly inside the container (`/proc/config.gz` is
readable, 3761 lines):

```
# CONFIG_CHECKPOINT_RESTORE is not set
```

- **The default kata kernel has no CRIU** → the macOS route starts memory/hybrid
  snapshots at L3 (cooperative checkpoint); see MAP RFC 0002 for the four-level
  ladder.
- All other CRIU kernel dependencies are present (CGROUP_FREEZER / USER_NS /
  IO_URING / SECCOMP / MEMFD / the full virtio set) — only this one switch is
  missing.
- `container system kernel set --binary` supports a **custom kernel** → **L1 is
  feasible**: take the kata-locked 6.18.35 source + the config the runtime exports
  (`/tmp/kata-6.18.35.config`), turn on `CHECKPOINT_RESTORE=y`, cross-compile on
  WSL2 (arm64), and the product is the `mixum-kernel` package; the same vmlinux
  serves Apple Container / WSL2 / QEMU micro-VM.
- Kernel choice conclusion: **the kata base is the container/VM guest hot path**
  (sub-second boot + 30–80 MB less resident per instance); **the native Arch kernel
  is the full disk-image shape** (stage 5, 2–5 s boot, CRIU included).

## 5. Importing the mixumOS image and the full MAP path

Import path (OCI layout tar → local registry → apple container):

```bash
container run -d --name mapreg --publish 127.0.0.1:15000:5000 registry:2
skopeo copy --dest-tls-verify=false oci-archive:mixumos-0.1.0-aarch64.oci.tar \
  docker://127.0.0.1:15000/mixumos:0.1.0
container image pull --platform linux/arm64 --scheme http 127.0.0.1:15000/mixumos:0.1.0
container run -d --name map-node --publish 127.0.0.1:17601:7601 \
  127.0.0.1:15000/mixumos:0.1.0
```

Three pitfalls, recorded one by one:

1. **pull forces HTTPS by default for a registry**; a local plaintext registry must
   pass `--scheme http` explicitly (otherwise `bad protocol version`).
2. **run must use the full reference** (`127.0.0.1:15000/mixumos:0.1.0`); the bare
   name `mixumos:0.1.0` resolves to docker.io.
3. **Both official Arch images fail platform validation** (suspected buildx
   attestation manifests) — our own OCI packaging must stay apple-container
   friendly (a simple OCI index, no attestation); the mkoci.sh output was verified
   compliant.

Full-path result: `mixumd --config /dev/null` starts (prints READY token) → mixcomp
headless (pixman) → the host TS SDK over `wss://127.0.0.1:17601/ws` completes the
handshake / `sys.info` (**os=Arch Linux**, backend=directory-reflink) / `snap.tree`
(genesis) / `task.submit` → queued→running→log→completed. `sys.info.compositor=false`
contradicts the mixcomp log; to be investigated (field semantics or timing).

**Version gap (old 0.1.0 ↔ MAP naming; evidence for RFC 0001 "adapter trivial")**:
subprotocol `mixum.v1`↔`map.v1`, handshake op `hello`↔`session.hello`, state event
`data: "queued"`↔`{state:"queued"}`. On 2026-10-19 agentd adopted the MAP names
while still accepting the old subprotocol/handshake names; the TS SDK offers a
`legacy: "mixum-v1"` option to talk to old images directly, so the smoke script no
longer needs a WebSocket subclass.

## 6. Snapshot size measurement (real node)

| Snapshot point | Logical node size | New blocks | CAS on disk |
|---|---|---|---|
| genesis (empty) | 0 B | 0 | 44 K |
| Write 5 MiB text | 5.0 MiB | 5 | text compressed to ~1 MB |
| Write another 10 MiB random (worst case) | 15.0 MiB | 10 | +10 MB |
| **Take another with zero changes** | 15.0 MiB | **0** | **zero growth** |

Engine latency benchmarks (300×2 KiB workspace) are in snapshots.md §9: transparent
pre-task snapshot 9.3 ms / restore materialization 1.6 s. **Conclusion: snapshot size
= amount changed, independent of the number of historical snapshots.**

## 7. Two-node whole-tree migration (evidence for "roll back to any version after migration")

A second container (map-node-2, same image) as the target:

```text
== A→B whole-tree push (topological snap.push per node) ==
  genesis/empty-baseline → 0 blocks;  text-5m → 5 blocks 5 MB;
  random-10m → 10 blocks 10 MB;       no-change → 0 blocks
== rollback verification on B ==
  restore(text-5m)  → workspace: text-5m.txt          ← random-10m.bin gone
  restore(random-10m) → workspace: random-10m.bin + text-5m.txt
```

B's CAS is 11 M, identical to A; the tree is isomorphic (same ids, same parent
chains), and restore semantics match local. **What migrates is not "the latest
state" but the whole timeline.** Practical note: the `remote` format is
`wss://host:7601` (no `/ws`; the pusher appends the path itself).

## 8. Defect fix: zero-block nodes never reach the target tree

**Symptom**: in the push above, empty-baseline/no-change (0 blocks) returned success
statistics, but B's tree had no such node; text-5m was forced to be marked
"remote root".

**Root cause** (`mixum-agentd/src/sync.rs`): the pusher only committed "when there
were blocks" — `sent==0` skipped `snap.push.commit`, and tree insertion happens in
commit. The missing-block list for a zero-block node and for an idempotent re-push
is always empty, so the node never arrives. The old loopback test only asserted push
statistics, which hid it.

**Fix** (two paired changes, dev `253b3ef` / module/agentd `4975781`):

1. `push_node` always sends commit — tree insertion does not depend on whether
   blocks were transferred.
2. The receiver's commit is idempotent for an existing same-id node and returns the
   existing id (`Tree::add` rejects duplicate ids; without the pair it would always
   emit BranchExists on re-push, breaking the "re-push is free" promise).

**Regression tests**: two sync.rs unit cases (zero-block tree insertion / same-id
idempotence) + one sync_loopback end-to-end case (a zero-block node must land in the
tree and an idempotent re-push must succeed — red under the old implementation).
Verification: dev full suite 15+4 passed, module/agentd 14+3 passed, clippy
`-D warnings` clean.

## 9. Remaining work and next steps

- [ ] Build the L1 kernel (kata 6.18.35 + `CHECKPOINT_RESTORE=y` on the WSL2 build
      machine); after building, run `criu check` + a real dump/restore.
- [ ] Rebuild the image (with this fix and H.264), replace map-node, re-test window
      streaming and whole-tree migration (all zero-block nodes should land; no more
      "remote root" compromise).
- [ ] Verify the machine form's systemd (the first-start transient error blocked
      completion; the container form uses PID1=entrypoint and is unaffected).
- [ ] Investigate the `sys.info.compositor=false` vs mixcomp log contradiction.
- [ ] `snap.push` whole-tree sugar (or a single `mixum-ctl tree-sync` command).
