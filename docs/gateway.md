> English translation of the Chinese document. Original: [gateway.zh-CN.md](gateway.zh-CN.md). Technical review pending.

# gateway: cloud hub design (multi-container routing, device pairing, token issuance)

> **Status: design finalized; M1–M3 implemented (see §9); single- and dual-replica
> e2e pass.** v1 remains single-user/self-hosted; multi-tenancy, persistent
> revocation, end-to-end encryption and node-side signature verification are not done.

## 1. Positioning and topology

The recording vision — "phone sends an instruction → cloud agent opens Chrome → a
separate native window appears on the Mac" — is missing a hub: clients (phone/Mac
console) are on the public internet, while nodes (mixumd + mixcomp containers) sit
behind containers/NAT and cannot reach each other. gateway is the only public WSS
entry point:

```
phone PWA ──┐                      ┌── WSS outbound registration ── mixumd node A (cloud container)
            ├── WSS/TLS ── gateway ┼── WSS outbound registration ── mixumd node B (home machine)
Mac PWA ────┘                      └── WSS outbound registration ── mixumd node C (k8s Pod, stage 5)
```

Three design red lines:

1. **Nodes connect outbound**: containers are behind NAT; mixumd always initiates the
   WSS registration to gateway; gateway never dials back.
2. **No mixumd protocol breakage**: gateway only routes envelopes; business semantics
   (task.* / snap.* / display.*) pass through unchanged; mixumd does not know clients
   exist.
3. **gateway makes no business decisions**: rollback/fork/takeover all happen on the
   node; gateway only answers "where should this envelope go".

## 2. Identity and tokens (issuance root moves up)

Today mixumd self-signs ed25519 tokens (`mixum1.*`, claims=sub/caps/iat/exp/jti/
taskId/windowIds). gateway becomes the **token issuance root**; mixumd's bootstrap
token is retired:

| Token | sub | caps | Use |
|---|---|---|---|
| user token | `user:<id>` | full user capabilities | the human's primary credential, issued by gateway bootstrap |
| node token | `node:<name>` | `gateway.route` | mixumd registers its outbound connection; this one use only |
| device token | `device:<id>` | a subset of user capabilities | issued to phone/Mac via device pairing; revocable |

- Signature algorithm/prefix unchanged (ed25519 + `mixum1.*`); gateway verifies.
- Node tokens are limited to the `sub` prefix; the gateway registration channel only
  accepts `node:*`. When a node forwards envelopes to the local mixumd socket, mixumd
  still does local verification (token passthrough or local bootstrap token; v1 uses
  the latter: gateway→node is treated as a trusted internal network, no second
  verification on the node; see §7 security boundary).
- Revocation: gateway keeps a jti revocation table (in memory for v1; persistence is
  left to the k8s stage).

## 3. Node registration and keepalive

New gateway ops (not in the protocol.md main table for v1; a separate §gateway):

| op | direction | payload | reply |
|---|---|---|---|
| `gateway.register` | node→gw | `{ name, token(node token), version }` | `{ lease: <seconds> }` |
| `gateway.heartbeat` | node→gw | `{}` | `{ ok: true }` |

- Node-side change (**the only new mixumd surface**): a `--gateway <url>` flag plus an
  outbound relay module — it plugs gateway in as an "extra trusted client connection"
  into the existing Session distribution, without understanding gateway semantics.
- Keepalive: no heartbeat within the lease means offline; exponential-backoff
  reconnect (same policy as the console client) avoids a registration storm during
  k8s rolling updates.
- Duplicate names: `gateway.register` rejects a same-name online node (the old
  connection must unregister before re-entry), so a misconfigured container cannot
  displace a same-name production node.
- gateway only keeps an online table (name → connection handle + version + last
  heartbeat); v1 is in memory, not persisted.

## 4. Client envelope routing

After a client (phone/Mac PWA) connects to gateway, the envelope gains an optional
field:

```json
{ "v": 1, "id": "...", "op": "task.submit", "node": "home-01", "payload": {...} }
```

- envelope adds `node: String` (serde default empty string + skip_serializing, purely
  additive; mixumd ignores it). gateway uses the raw JSON to distinguish "field
  absent" from "field present but invalid": node names follow the registration rules
  (**1..=64 characters, no colon**); empty string/null/non-string/too long all return
  `bad_request`, never silently falling back to automatic routing.
- **No node field**: with one node, route to the only online node; with multiple
  nodes, return `bad_request` ("please specify node") — no implicit default, avoiding
  ambiguity.
- **Request/reply correlation**: gateway keeps `replyTo → (client session, origin
  node)` and forwards node replies back; when a node is offline, pending requests get a
  uniform `internal` error.
- **Subscription ops** (task.events / display.windows): subscriptions bind to the
  client session; events are forwarded as-is from the origin node.
- **Binary frames** (0x01 video): gateway does not parse them; it forwards by the
  "client's attached-window table → origin node". On attach it records
  (client, windowId) → node; frames are addressed in reverse. Same-name window id
  conflicts across nodes are isolated per node at attach time.
- **Backpressure**: reuse the node's per-subscriber bounded queue semantics; the
  gateway forwarding queue is also bounded (64), dropping on overflow to preserve
  real time — the frame-stream real-time-first principle is unchanged.

## 5. Device pairing

Credential bootstrap for a new phone/Mac:

| op | direction | payload | Notes |
|---|---|---|---|
| `gateway.pair.create` | user token→gw | `{}` | generate a 6-digit pairing code, valid 5 minutes |
| `gateway.pair.claim` | no credential→gw | `{ code, deviceName }` | exchange for a device token (`{ token, deviceId }`) |

- Flow: the user taps "add device" on a trusted device to get a pairing code → the new
  device enters it → gateway issues a `device:<id>` token (capabilities = a subset of
  the user's, excluding `admin`/`snap.push` by default) → the new device persists it in
  localStorage.
- Device list and revocation: `gateway.device.list/revoke` (user token); revocation
  enters the jti revocation table.
- v1 pairing codes are single-use and expire in 5 minutes; rate-limited by source IP
  (10 failures/minute, excluding ephemeral ports); over the limit returns `conflict`.
- The first message and token verification failures return semantic codes:
  `bad_request` (invalid request), `forbidden` (token/name mismatch), `conflict`
  (duplicate-name registration, rate limit), `not_found` (node offline), `internal`
  (forwarding/lease/backend failure).

## 6. Security boundary (honest list)

- **gateway sees plaintext**: TLS terminates at gateway, so envelopes and frame content
  are visible to it. The premise is self-hosting — gateway and nodes belong to the
  same user trust domain. End-to-end encryption (client and node negotiate a session
  key; gateway forwards ciphertext only) is a v2 evolution.
- **No second signature verification on the node**: gateway→node is treated as a
  trusted internal network (the node socket listens only on localhost/container
  network). v2 can add local verification: gateway forwards the original token and the
  node verifies it.
- Rate limiting: pairing entry is limited by source IP to 10 failures/minute; other
  unauthenticated connections have no general rate limit in v1.
- Audit: pairing success/failure, device revocation, token revocation and node
  registration go to stdout logs (v1).

## 7. Relationship with the k8s operator (stage 5)

gateway's node registry is the operator's reconciliation target:

- a node = a MixumTask CRD instance; when the operator creates a Pod it injects a node
  token, and the Pod's mixumd `--gateway` registers automatically;
- gateway v1's in-memory registry → stage 5 swaps in a k8s API/etcd backend (the
  `NodeRegistry` trait is reserved; the in-memory implementation is the default);
- snapshot volume mounts: the node mounts a pre-provisioned snapshot volume at startup,
  reusing the existing snap.pull semantics.

## 8. Client changes (console PWA)

- Settings gains a "node" selector (from `gateway.node.list`; hidden when there is one
  node).
- Pairing entry: "add device" (generate a code) and first-run "enter code".
- Envelopes carry the currently selected node (absent when none is selected; automatic
  routing in the single-node case).
- Existing direct-to-mixumd use **stays unchanged** (the protocol ignores an unknown
  node field).

## 9. Implementation milestones

- **M1 (✅ landed 2026-10-18)**: gateway binary (`mixum-gateway` crate, reusing
  mixum-protocol) + node outbound registration + `envelope.node` routing (text and
  binary) + user/node token issuance (bootstrap admin token printed at startup,
  `gateway.token.issue`, `gateway.node.list`); e2e two-node topology full chain passed
  (`tests/integration/gateway-e2e.mjs`: registration/token/task submit/event-stream
  isolation/window-table isolation/frame routing by attachment). M1 transport is
  plaintext ws (TLS in M2); mixumd adds `--gateway/--gateway-name/--gateway-token`
  outbound relay (`rust/crates/mixum-agentd/src/gateway.rs`).

### 9.1 Node channel private convention (gateway ↔ mixumd relay, not public protocol)

- gateway assigns a `gatewaySession` to each client connection; the relay opens a
  loopback plaintext WS to the local mixumd per session (127.0.0.1, reusing
  `server.serve_plain_listener`) — mixumd subscriptions/broadcasts are session-scoped,
  so per-session loopback naturally isolates multiple clients.
- gateway→node text: envelope + `gatewaySession`; `gateway.session.open/close` control
  the session lifecycle (relay is idempotent).
- node→gateway text: same with `gatewaySession`; gateway strips it before returning to
  the client. Binary: `u32 LE session-id length + session-id bytes + raw frame`.
- A session opens lazily on first routing to a node (duplicate opens ignored
  idempotently by the relay) and closes when the client disconnects or the node
  reconnects (clients reconnect/re-subscribe per existing semantics).
- **M2 (✅ landed 2026-10-18)**: device pairing (`gateway.pair.create/claim`, 6-digit
  code, 5 minutes, single-use) + device registry (`gateway.device.list/revoke`, jti
  revocation effective immediately) + gateway TLS (`--listen wss://`, self-signed cert
  persisted in --data-dir, TOFU; relay pins with `--gateway-fingerprint`, default
  first-use TOFU to `dataDir/gateway-fingerprint`) + console node selector and
  pairing/device UI (in Settings); e2e adds pairing/device hello/revocation-invalid
  assertions.
- **M3a (✅ landed 2026-10-19)**: registry extracted into `NodeRegistry`
  (`register/remove/get/online/renew`) + `MemoryRegistry` default. The registry stores
  metadata only; the outbound socket stays in `Gateway::local`; `Gateway::with_options`
  injects the backend, instance name and mesh address. M1/M2 behavior unchanged;
  gateway-e2e 15 regressions pass.
- **M3a' (✅ landed 2026-10-19)**: k8s Lease backend — one
  `coordination.k8s.io/v1` Lease per node (`holderIdentity`=replica instance; annotations
  store node/owner/ownerAddr/version/since); same-name unexpired leases are rejected,
  expired ones can be taken over; the relay renews every 20 s
  (`gateway.heartbeat`, 60 s lease). Manifest at `cloud/k8s/60-gateway.yaml`
  (2 replicas + RBAC + headless mesh Service; replicas share the identity Secret as the
  issuance root).

> Deployment note: replicas must share **identity** (same issuance root) but each
> replica must use an **independent, writable data-dir**. Mount identity and state
> separately: identity Secret read-only, `--data-dir` per Pod as emptyDir/PVC. M3's
> revoked/devices/pairs remain per-Pod local state; cross-replica revocation/pairing is
> not guaranteed; see docs/security.md. `data-dir/gateway-state.json` contains the
> replica's bootstrap jti; bootstrap startup rotation revokes the old jti; if several
> replicas share a data-dir, a later-starting replica revokes the bootstrap token a
> earlier replica just issued. `gateway-m3-e2e` reproduces this with "independent state
> + copied identity file".
- **M3b (✅ landed 2026-10-19)**: inter-replica private WS mesh — `mesh.hello` shared
  token auth, `mesh.text/binary/close` forwarded per `ms-<origin-instance>-<seq>`
  session; nodes connect only to their owner, clients may connect to any replica.
  `tests/integration/gateway-m3-e2e.mjs` verifies the shared Lease registry, B→A and
  A→B bidirectional task/event streams, and cross-replica dropped-node cleanup.

Test plan: `gateway-e2e.mjs` single-replica M1/M2 full chain
(registration/token/routing/isolation/frames/pairing/revocation);
`gateway-m3-e2e.mjs` dual-replica + kind Lease (shared registry, bidirectional
cross-replica routing, dropped-node cleanup). Both have landed and pass.

## 10. Open questions

1. Registry sharing with multiple gateway replicas (v1 single instance; k8s stage
   solves it).
2. Emergency response to a leaked node token: revoke + reissue process (manual, v1).
3. gateway memory watermark for frame forwarding (many clients/windows) — M1 targets
   single-user self-hosted scale; no cross-user multi-tenancy.
4. Pairing-code UI on narrow devices such as watches (later).
