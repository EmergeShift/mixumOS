> English translation of the Chinese document. Original: [security.zh-CN.md](security.zh-CN.md). Technical review pending.

# mixumOS Security Design

> Principle: the attack surface is narrowed to “one TLS channel + one set of capability tokens + one Unix socket”;
> cryptographic primitives have zero third-party dependencies (standard library/system library); all security decisions can be audited from a single directory.

## 1. Threat Model (v1)

| Asset | Adversary | Defense |
|------|------|------|
| Task data/workspace | Network eavesdropper | TLS 1.3 enforced; block transfer is plaintext but hash-verified + end-to-end TLS |
| Control plane (tasks/snapshots/takeover) | Unauthorized connections | Capability token (ed25519 signature) + least-privilege capability grants |
| Window contents | Unauthorized viewing | display.view capability + optional window id allowlist (claims.windows) |
| Input control | Unauthorized takeover | display.input capability + broker takeover state machine (hard reject in agent mode) |
| Data at rest (snapshot store) | Disk theft | AES-256-GCM block-level encryption (configurable), masterKey 0600 |
| Privilege escalation inside the container → host | Malicious instructions | Container is unprivileged (default); separate volumes for workspace and snapshot storage; CRIU privileged variant marked separately |
| Host-side container impersonation | Malicious mixumd | Certificate fingerprint pinned via TOFU; disconnect and alert on fingerprint change |

Explicitly not defended (on record): kernel vulnerability escape (left to the container runtime and host hardening),
physical attacks, and sensitive data residency in memory snapshots (CRIU image encryption is listed for Phase 5).

## 2. Identity and Tokens

- Server identity = ed25519 seed (`dataDir/identity.key`, 0600, generated on first startup) + self-signed certificate;
- Capability token: `map1.<payload>.<sig>` (the legacy `mixum1.` is still accepted during the transition period); for claims, see protocol.md §3;
- Grant principle: console/mobile tokens grant only task.run+observe+display.view+input;
  snapshot sync tokens are issued separately with a separate TTL;
- jti is reserved for the revocation list; reuse of long-lived tokens across connections is a design decision (TLS prevents transport replay);
  if single-use tokens are needed, it suffices to set exp to minute-level precision at issuance;
- The bootstrap management token is generated and written to disk on first startup (0600); operators derive service tokens from it.

## 3. Transport

- TLS 1.3 only (ECDSA P-256 self-signed); plaintext WS exists only in tests (explicitly marked in code);
- Client fingerprint pinning: the fingerprint is displayed on first connection and pinned after confirmation; `auth.PinVerifier`
  verifies it at the handshake layer and disconnects on a mismatch;
- WS frame limits: 32 MiB for control / 16 MiB for binary records; connection-level token-bucket rate limiting;
  slow consumers (frame channel continuously full) are disconnected after 2 seconds to prevent them from dragging down the broker.

### 3.1 gateway Multiple Replicas (M3)

- The shared registry is stored in a k8s Lease and contains only node metadata (name/owner mesh address/version),
  not node tokens, task contents, or frame data;
- The inter-replica mesh consists of in-cluster plaintext WS + a shared token injected via Secret, and listens only on
  ClusterIP/headless Service; cross-cluster or public-network deployments must add their own TLS/tunnel
  (v1 does not provide end-to-end encryption between replicas; documented);
- Replicas must share `identity.key` (Secret mount) as the token issuance root; otherwise a token signed by A
  will not verify on B; likewise, the TLS certificate must be shared for wss to be used reliably;
- The mesh session id (`ms-*`) is issued only to the node relay as a loopback marker; a node cannot use it to escalate privileges across replicas;
  node tokens are still validated locally on the owner replica by the `node:*` prefix;
- **M3 multiple replicas do not share** `revoked`/`devices`/`pairs`/`bootstrap_jti`: this
  state is independent within each replica's `--data-dir`. Cross-replica revocation, pairing-code claim, and bootstrap
  rotation are therefore not guaranteed to be consistent; the M3 Service uses ClientIP affinity to reduce the probability of crossing Pods,
  but this is not a security boundary. When cross-replica revocation/pairing is required, authentication state must be placed in a shared backend
  (Phase 4), or the operation must be performed only through the single-replica management entry point.

## 4. Snapshot Subsystem

- Block hash = plaintext SHA-256, fully verified on read (guards against silent corruption and block swapping);
- GCM AAD binds storeID + block hash (prevents cross-store block replay);
- Remote import path: manifest paths reject `..`/absolute paths; when the parent of an imported node does not exist,
  it becomes a remote root (marked with origin), producing no dangling references across trees;
- The push capability (snap.sync) is separate from the snapshot management capability; the push peer can receive but cannot manage.

## 5. Inside the Container

- mixumd/agentd do not run as root (v2 target; v1 permits root due to overlayfs mount detection,
  and the distribution units constrain it with DynamicUser; see distro.md);
- mixcomp control socket `/run/mixum/display.sock`: 0700 directory,
  reachable only by processes inside the container; the window stream reaches the host directly without traversing any network port
  (frames travel over mixumd's WSS, where capability authorization is enforced);
- Agent processes launched by the external driver: separate process group, minimized environment
  (MIXUM_INSTRUCTION/TASK_ID/AGENT_BRIDGE), and no inherited shell history;
- Snapshot freezing preferentially uses the **cgroup v2 freezer** (`cgroup.freeze=1`, atomically covering the entire
  process tree, including grandchildren that have escaped the process group); when the kernel or delegation does not permit it, it falls back to process-group SIGSTOP;
- Filesystem write allowlist: on Linux 5.13+, **Landlock** is applied in `pre_exec`;
  tasks can write only to the workspace, `/tmp`, `/dev`, `/run`, and pre-created tool directories under HOME
  (`.cache/.local/.config/.npm/.cargo/.rustup/.mixum`, extendable with
  `MIXUM_LANDLOCK_EXTRA_WRITABLE`), with everything else read-only; GUI session replay and
  `spawn_gui` follow the same isolation path, and child processes, after `env_clear()`, receive only allowlisted variables.
  Linux negative testing covers rejection of writes outside the workspace and to the HOME root directory; when the kernel has not enabled
  Landlock, it falls back to the container boundary (unprivileged + separate workspace volume). Semantic interception of
  high-risk commands is performed on the MixumART side; the OS layer is responsible only for last-resort rollback and resource boundaries.

## 6. Evolution Slots (Phase Markers)

- [Phase 4] Token revocation list (jti → embedded KV (same sled as the snapshot tree) or redis);
- [Phase 4] Keyframe encryption for the H.264 path (session-level key, negotiated during handshake);
- [Phase 5] CRIU image encryption and integrity;
- [Phase 5] gateway multi-tenancy: device pairing QR code → short-lived token exchanged for a long-lived token.
