> English translation of the Chinese document. Original: [roadmap.zh-CN.md](roadmap.zh-CN.md). Technical review pending.

# mixumOS roadmap

> Cadence: deadlines are measured in months. A stage is complete when "the design
> doc comes first, tests lock the behavior, and the distribution artifact is
> reproducible" — not when a line count is reached.

## Done (reference phase, Go reference implementation)

- [x] Wire protocol v1: envelope, ops, capability tokens (ed25519), binary frame header.
- [x] Snapshot engine semantics: content-addressed block store (zstd + AES-GCM),
      snapshot tree DAG (transparent pre-task snapshot / in-place rollback / fork / diff / GC).
- [x] Task engine: AgentDriver interface, shell/external/browser drivers, process-group
      freeze (SIGSTOP) coupled to snapshots.
- [x] Display broker: window registration, frame distribution, takeover state machine
      (agent→user→free).
- [x] WSS server (TLS 1.3) and cross-node block sync protocol (dedup push).
- [x] mixcomp C implementation compiles (wlroots 0.18, aarch64).
- [x] Container e2e: task execution → pre-task snapshot → rollback → fork full path PASS.
- [x] Full-stack integration test (two in-process nodes push snapshots to each other; dedup works).

## Stage 1: Rust protocol and snapshot kernel ✅

Goal: stand up the Rust workspace and make the language-independent kernel solid first.

- [x] `mixum-protocol`: envelope/ops/caps/frame header/tokens; unit tests match the
      reference semantics.
- [x] `mixum-snapshot`: block store (CAS + zstd + encryption at rest), tree DAG (sled),
      COW packing, CRIU driver, GC.
- [x] Cross-validation: a test-vector set locks semantics (tokens verify each other,
      manifests read each other, blocks import each other; after the Go reference was
      removed, pre-generated `tests/cross-protocol` vectors carry it).

Completion criterion (met 2026-10-05): `cargo test` all green + cross-vector tests pass.

## Stage 2: mixum-agentd (in-container daemon) ✅

- [x] WSS server (tokio-tungstenite + rustls, TLS 1.3 enforced).
- [x] Task engine and AgentDriver trait; external bridge implementation.
- [x] Display broker and takeover state machine; mixcomp control channel (protocol unchanged).
- [x] Container e2e with mixcomp (`tests/integration`, Rust binary).

Completion criterion (met 2026-10-05): e2e full path PASS against the Rust agentd
(handshake / system info / window stream / task / snapshot tree / rollback / fork).

## Stage 3: distribution shape (current, wrapping up)

- [x] PKGBUILD ×5 (mixum-agentd / mixcomp / mixum-ctl / mixum-base / wlroots0.18
      self-built), dynamic linking against system dependencies.
- [x] Own pacman repo tools (repo-add flow, arm64 + x86_64 dual repos).
- [x] rootfs/OCI assembly: pacstrap + pure-shell OCI packaging (mkosi replaced by
      equivalent scripts; bootable disk image is stage 5).
- [x] systemd units, sysusers, tmpfiles in packages.
- [x] x86_64 build path (binfmt + qemu-user emulated run, dual-arch e2e PASS).
- [x] Dual-arch rebuild measured: after mixum-agentd moved to in-container cargo
      builds, the full chain was rebuilt (2026-10-17, Docker Desktop/WSL2 path;
      x86_64 native + aarch64 qemu; five packages/repos/OCI for both arches, import,
      boot and handshake acceptance PASS, multi-arch index merged).
- [x] Two-node cross-node transfer e2e (lid close → cloud continue full chain:
      snap.create → snap.push → idempotent re-push → cloud restore materialization →
      head migration) and engine latency benchmarks (2026-10-17; numbers and repro in
      snapshots.md §9).

Completion criterion: `make dist` produces dual-arch OCI images and the pacman repo
in one command (met 2026-10-17; the cargo cache volume is implemented — incremental
rebuild 45 s; cross-compile options are an evolution item, see distro.md §7.3.1).

## Stage 4: agent embedding and GUI streaming experience

- [x] AgentDriver trait published as a crate (2026-10-18): the SPI is a separate
      `mixum-agent-driver` crate (zero internal deps; Task/TaskSpec/TaskView/Sandbox/
      EventEmitter/Registry/ShellDriver); agentd re-exports it so the API is unchanged;
      guide at `docs/agent-driver.zh-CN.md`, runnable example `hello_driver`
      (in-process and subprocess bridge paths).
- [x] mixcomp H.264 stream (openh264 encode, attach negotiation + WebCodecs decode,
      JPEG path retained; 2026-10-17, static content uses 1/84 of JPEG's 10 s traffic,
      dynamic content expected 1/5–1/20); adaptive frame rate and damage regions still
      pending.
- [x] mixum-ctl power hook (2026-10-20): macOS IOKit listens for pre-lid snapshot
      upload, the "migrate before allowing sleep" invariant (three mock unit tests:
      success/failure/timeout must all ack); Linux systemd hook (`power
      --install/--remove/--print`) and CLI migration path e2e
      (`tests/integration/power-e2e.mjs`); macOS real-hardware verification awaits a
      real Mac.
- [x] mixum-ctl local window viewer service (2026-10-19): admin page (window list 2 s
      polling) + one independent view page per container window (title bar + takeover
      badge / 30 fps attach / first-frame resize / reconnect / cursor overlay),
      takeover + input injection (user-mode mouse/wheel/keyboard, stream WS allowlist
      control channel), `view-e2e.mjs` route + WS frame assertions and a real-browser
      run (canvas rendering + pointer injection producing a damage frame + screenshot).
- [x] Startup latency measured (2026-10-21): mixumd in-container start to connectable
      median 107 ms (cold) / 102 ms (warm), end-to-end docker run median 226 ms;
      unchanged-workspace hot snapshot 8.8 ms, 300-file snapshot restore 689 ms
      (`tests/integration/startup-bench.mjs`; methodology in docs/snapshots.md §9).
- [ ] Console PWA: task stream, snapshot tree (graph), window canvas (cursor mapping),
      takeover button.
- [x] 24 h stream stability soak (2026-10-21, FINAL PASS): recoverable multi-segment
      cumulative (11 segments, 1498 minutes; anomalies/disconnects/stalls all zero);
      two host reboots resumed from checkpoints; liveness detected via control-channel
      activity under damage gating (`tests/integration/soak-run.mjs` +
      `soak-client.mjs` checkpoint mode).

Completion criterion: phone sends an instruction → cloud agent opens Chrome → a
separate native-style window appears on the Mac, and takeover/rollback/fork work
throughout.

## Stage 5: cloud and multi-device

- [ ] k8s operator (kube-rs): MixumTask CRD, node-pool affinity (arm64/x86_64),
      snapshot volume mounts.
- [x] gateway M1/M2 (2026-10-18, gateway-e2e full chain): node outbound registration,
      envelope routing (text + binary), user/node token issuance, device pairing/
      revocation, TLS, console node selector.
- [x] gateway M3 (2026-10-19, gateway-m3-e2e): `NodeRegistry` trait + k8s Lease shared
      registry (heartbeat renewal), inter-replica WS mesh bidirectional cross-replica
      routing, cross-replica cleanup of dropped nodes; manifest
      `cloud/k8s/60-gateway.yaml`.
- [x] Feasibility of cross-kernel memory snapshot migration (2026-10-20 controlled
      experiment PASS): source WSL 6.18 + CRIU 4.2.1 → target Debian 6.1.0-47 + same
      CRIU; after the target control PASSed, cross-kernel restore PASSed twice (count
      0→1→3 continued); the earlier "infeasible" conclusion was a mixed environment
      (version mix/file fd/cwd/root mode/modules) and was corrected; coverage = a
      minimal static process; the precise kernel fingerprint gate remains the
      conservative default and is evaluated per scenario
      (`docs/criu-feasibility.md` §4.4/§7).

## Ongoing

- Security: threat model updated per stage (security.md).
- Compatibility: mixumd↔mixcomp private channel ignores unknown record types + warning
  counter (2026-10-19, EmergeShift#8 implementation side; spec side synced in MAP
  display.md).
- `display.attach` subscriber-insertion vs second-lookup race (PR#6 leftover ②,
  eliminated on 2026-10-19 with a single lock interval; `attach` no longer does a
  second lookup).
- Docs: write implementation/design deviations back to the design docs each stage.
- mixcomp upstream tracking: evaluate wlroots 0.19 changes.

## 2026-10-19: agentd adopts MAP v1

- `session.hello` (keeps the `hello` alias), `map.v1`/`mixum.v1` dual subprotocols.
- task.event data unified as objects (`state`/`log`/`step`/`progress`/`error`/`input`).
- New implementations: `task.reply` human-input channel, `session.resume` (≥256 event
  replay ring), `mem.*` and `builtin` provider, `map1.` token prefix (old prefix still
  accepted).
- IDs switched to ULID (envelope/task/snapshot/memory).
- Acceptance: `scripts/map-sdk-e2e.sh` (TS SDK zero-adaptation full loop + legacy
  dialect); spec at `Mixum-Agent-Protocol/site/src/content/docs/spec/`.
