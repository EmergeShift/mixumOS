# Documentation map

English is the canonical language. Once a page is translated, the English text
lives at `docs/<name>.md` and the Chinese counterpart at
`docs/<name>.zh-CN.md`. Until then the Chinese page is the only text and is
linked below.

## Status

- **current** — describes code that exists today.
- **partial** — some paths shipped, some still stubs.
- **design** — describes intended behavior; the code is not merged.
- **research / PoC** — experiment notes, not a product promise.
- **plan / historical** — roadmap or release notes, not current behavior.

## Pages

| Page | Status | English | 中文 |
|---|---|---|---|
| [agent-bridge](docs/agent-bridge.md) | current | pending | [中文](docs/agent-bridge.md) |
| [agent-driver](docs/agent-driver.md) | current | [English](docs/agent-driver.md) | [中文](docs/agent-driver.zh-CN.md) |
| [apple-container](docs/apple-container.md) | current | pending | [中文](docs/apple-container.md) |
| [architecture](docs/architecture.md) | current | pending | [中文](docs/architecture.md) |
| [backends](docs/backends.md) | partial | [English](docs/backends.md) | [中文](docs/backends.zh-CN.md) |
| [clipboard](docs/clipboard.md) | design | [English](docs/clipboard.md) | [中文](docs/clipboard.zh-CN.md) |
| [console](docs/console.md) | current | [English](docs/console.md) | [中文](docs/console.zh-CN.md) |
| [CRIU feasibility](docs/criu-feasibility.md) | research | pending | [中文](docs/criu-feasibility.md) |
| [distro](docs/distro.md) | current | pending | [中文](docs/distro.md) |
| [gateway](docs/gateway.md) | design | pending | [中文](docs/gateway.md) |
| [host-integration](docs/host-integration.md) | current | pending | [中文](docs/host-integration.md) |
| [macOS verification](docs/macos-verification.md) | current | [English](docs/macos-verification.md) | [中文](docs/macos-verification.zh-CN.md) |
| [migration](docs/migration.md) | current | pending | [中文](docs/migration.md) |
| [CRIU + Wayland PoC](docs/poc-criu-wayland.md) | PoC | pending | [中文](docs/poc-criu-wayland.md) |
| [protocol](docs/protocol.md) | current | pending | [中文](docs/protocol.md) |
| [security](docs/security.md) | current | [English](docs/security.md) | [中文](docs/security.zh-CN.md) |
| [snapshots](docs/snapshots.md) | current | pending | [中文](docs/snapshots.md) |
| [Wayland magic](docs/wayland-magic.md) | current | pending | [中文](docs/wayland-magic.md) |
| [roadmap](docs/roadmap.md) | plan | pending | [中文](docs/roadmap.md) |
| [release notes 0.1.4](docs/release-notes-0.1.4.md) | historical | pending | [中文](docs/release-notes-0.1.4.md) |
| [release notes 0.1.5](docs/release-notes-0.1.5.md) | historical | pending | [中文](docs/release-notes-0.1.5.md) |

## Docs-first / not implemented

These pages describe work that is not in the current build:

- **gateway** — design finalized, no gateway code yet (§9 has the milestones).
- **clipboard** — protocol draft; the reference implementation lives in a fork,
  not in this tree.
- **CRIU feasibility / CRIU + Wayland PoC** — controlled experiments, not a
  turnkey migration path; cross-device restore is still open.
- **backends / nvx** — nvx is an early script, not a supported backend.
- **roadmap** — a plan, not a claim about today.

If a page says a feature exists, the code or tests should be the tie-breaker.
