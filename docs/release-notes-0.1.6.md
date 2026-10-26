# mixumOS 0.1.6 — aarch64 + x86_64

Status: pre-release. Target date: 2026-10-27. Build commit: `bd24377`
(the release tag also carries this note). Previous release: 0.1.5.

> Chinese translation: [release-notes-0.1.6.zh-CN.md](release-notes-0.1.6.zh-CN.md).

## Highlights

- **Memory and hybrid snapshots are now a supported path.** A patched CRIU 4.1 is
  built inside the target rootfs, with LTO disabled and the parasite symbols kept.
  The snapshot engine now handles memory-only dump/restore, hybrid disk+memory
  restore, cgroup freeze ordering, restore timeouts, and `MIXUM_CRIU_BIN`.
- **The privilege boundary is explicit.** Tasks run as the unprivileged `mixum`
  user by default; root is a supervisor. A single sudo command can be requested
  through `mixum-sudo`/`privilege.*`, reviewed, approved, and executed without
  granting a general-purpose root shell. Pending approvals are replayed to a
  client that connects late.
- **Sandbox and replay hardening.** HOME-scoped Landlock write allowlists,
  cgroup lifetime fixes, replay environment filtering, `pre_exec` cgroup attach
  through a pre-opened descriptor, and real Landlock write-denial tests.
- **Gateway contract alignment.** WebSocket subprotocol negotiation now supports
  both `map.v1` and `mixum.v1`; hand-written gateway replies carry `ts`; RFC 0003
  node routing/control operations are implemented and tested.
- **One-click agent installation.** `mixum-ctl agent` installs Claude Code, Codex,
  Gemini CLI and OpenCode from inside the environment, writes a prompt template,
  and works as the task user with user-level npm prefixes. The base image now
  ships `nodejs`, `npm` and `curl` for out-of-box installs.
- **First x86_64 image.** The x86_64 build path is fixed (CRIU `!lto`, `mixum-ctl`
  clears makepkg's CFLAGS/LDFLAGS for `ring`) and the release includes an x86_64
  OCI/rootfs alongside aarch64.
- **Docs and distribution polish.** English-first documentation with Chinese
  translations, refreshed README/brand, GPL-2.0 kernel notices, security and
  contribution files, and CJK fonts in the image.

## Snapshot and CRIU

- Added a patched `criu-mixum 4.1` package and built CRIU inside the assembled
  aarch64 target rootfs, so the parasite matches the image glibc instead of the
  rolling build host.
- Memory-only restore no longer pre-freezes/unfreezes the old cgroup; hybrid
  restore freezes the disk view first, then dumps/restores CRIU state.
- CRIU restore detaches with `-d` to avoid RPC timeouts on long-running tasks.
- Single-task dumps no longer pass `--ext-unix-sk`; sandbox-task parasite handling
  remains under investigation.
- Added `MIXUM_CRIU_BIN`, memory remount handling and bounded dump timeouts.
- Snapshot tags persist `base`/`baseDigest`; restore validates the same base.
- CRIU control markers now carry `generated_at`; stale markers and residual
  counter processes are rejected before a controlled run.

## Privilege and sandbox

- Tasks default to the `mixum` user; root only supervises and executes approved
  single commands.
- `mixum-sudo` requests are queued as `privilege.request`; `privilege.list`
  replays pending requests; approval executes only the approved command.
- HOME tool directories are explicit Landlock write targets; HOME itself remains
  read-only.
- cgroup freezer handles are reference-counted; per-task signals no longer use the
  global freezer.
- GUI replay is limited to local disk snapshots, filters recorded environment
  variables, waits for the old process, and uses the same isolation path as
  normal GUI launches.
- `pre_exec` only writes to a pre-opened `cgroup.procs` descriptor; real Landlock
  denial tests replaced the old `/proc/self/status` skip.

## Gateway

- WebSocket subprotocol selection prefers `map.v1` and falls back to `mixum.v1`.
- Hand-written gateway replies now include `ts`, fixing Rust/`MapClient` clients
  that timed out on `gateway.node.list`.
- RFC 0003 routing/control operations are implemented, including node-scoped
  tokens, pairing, device listing/revocation, and typed errors.

## Agent installation and image

- `mixum-ctl agent list/install/template` supports Claude Code, Codex, Gemini CLI
  and OpenCode, with `--dry-run` and `--cmd` overrides.
- Installs use `$HOME/.local` and `$HOME/.opencode/bin` instead of requiring root;
  missing tools produce an explicit non-root error.
- `mixum-base` now depends on `nodejs`, `npm` and `curl`; the image includes CJK
  fonts and Noto fallbacks.
- x86_64 build fixes: CRIU package uses `!lto`; `mixum-ctl` unsets makepkg
  CFLAGS/CXXFLAGS/LDFLAGS for `ring`.

## Docs and project

- English-default documentation with Chinese translations for the main docs.
- README, brand assets, security/contribution files, and GPL-2.0 kernel notices
  refreshed for the public repository.
- Release notes for 0.1.4 and 0.1.5 translated.

## Validation

- Built aarch64 locally with `make dist ARCH=aarch64`.
- Built x86_64 in GitHub Actions with `make dist ARCH=x86_64`.
- Package versions in the image:

```text
mixum-base 0.1.6-1
mixum-agentd 0.1.6-1
mixcomp 0.1.6-1
mixum-ctl 0.1.6-1
criu-mixum 4.1-1 (aarch64 image)
```

- Rust workspace tests pass on the release commit (`cargo test --workspace`).

## Known limitations

- The x86_64 image does not include the target-rootfs CRIU build yet; memory and
  hybrid snapshots remain aarch64-first in this release.
- Memory/hybrid snapshot support is still pre-release; test with the matching
  kernel and CRIU version before relying on it.
- Cross-kernel CRIU remains a controlled experiment, not a general migration
  guarantee.
- The x86_64 image is built but not exercised on Apple Container (Apple Container
  is macOS/arm64 only); validate it with Docker on x86_64.
- `mixum-ctl agent install` downloads upstream packages/scripts and therefore needs
  network access inside the environment.

## Upgrade notes

- New environments should use the `0.1.6` image; existing containers keep their
  current image until recreated.
- The aarch64 image now contains CRIU; older 0.1.5 images required installing it
  manually.
- Agent installs on 0.1.6 can use the bundled `nodejs`/`npm`/`curl`; custom images
  must still provide them.
- Restart environments after upgrading so the new entrypoint and packages are used.

## Release assets

| File | SHA256 |
|---|---|
| `mixumos-0.1.6-aarch64.oci.tar` | `ba4fe9d800f5a89c1fef0ecfae7bcafd1dd46f0e73e32f8bbc7a4c465c619493` |
| `mixumos-0.1.6-aarch64-rootfs.tar.zst` | `92f2c6e2d5dfa665b23e1bc7e88f201f88db36f0810d28386f6a0caa8e03d09d` |
| `mixumos-0.1.6-aarch64-repo.tar.zst` | `3ba8c30d2cf5f1f2b7bd1f57b548dd0c3b7ddf4f176230632e8b65f7000c86d8` |
| `mixumos-0.1.6-x86_64.oci.tar` | `1968504645d43236f3e2b94992d160625079a81c852ac281f9c57027510fc0c0` |
| `mixumos-0.1.6-x86_64-rootfs.tar.zst` | `1604d2fb341e330972e669bf1b845027f1131a4c830477f63be62c00f2298193` |
| `BUILDINFO-0.1.6` | `27722fa8eb5c6a7297cb3ad646c268c83d3bcfe517b795623e1fdcabe20dbf4f` |

## Contributors

Thanks to the three people who carried this release:

- **AshReo** ([@ashreo](https://github.com/ashreo)) — release, kernel/CRIU, snapshots, sandbox, gateway, docs
- **yubing0721** ([@yubing0721](https://github.com/yubing0721)) — gateway, agent install, build fixes, distribution
- **MASOCHIST / LimDN** ([@LimDN](https://github.com/LimDN)) — display attach/routing, snapshot tree locking, early backend work

And to everyone who tested early images and reported failures.
