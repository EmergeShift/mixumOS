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

## Coverage (2026-10-24)

- English default: 18 of 21 pages (`agent-bridge`, `agent-driver`, `apple-container`,
  `architecture`, `backends`, `clipboard`, `console`, `distro`, `host-integration`,
  `macOS verification`, `migration`, `poc-criu-wayland`, `release-notes-0.1.4`,
  `release-notes-0.1.5`, `roadmap`, `security`, `snapshots`, `wayland-magic`).
- Architecture translation is still pending; the remaining pages are Chinese or
  mixed. The map below is the queue, not a claim that the work is done.

## Pages

| Page | Status | English | 中文 |
|---|---|---|---|
| [agent-bridge](docs/agent-bridge.md) | current | [English](docs/agent-bridge.md) | [中文](docs/agent-bridge.zh-CN.md) |
| [agent-driver](docs/agent-driver.md) | current | [English](docs/agent-driver.md) | [中文](docs/agent-driver.zh-CN.md) |
| [apple-container](docs/apple-container.md) | current | [English](docs/apple-container.md) | [中文](docs/apple-container.zh-CN.md) |
| [architecture](docs/architecture.md) | current | [English](docs/architecture.md) | [中文](docs/architecture.zh-CN.md) |
| [backends](docs/backends.md) | partial | [English](docs/backends.md) | [中文](docs/backends.zh-CN.md) |
| [clipboard](docs/clipboard.md) | design | [English](docs/clipboard.md) | [中文](docs/clipboard.zh-CN.md) |
| [console](docs/console.md) | current | [English](docs/console.md) | [中文](docs/console.zh-CN.md) |
| [CRIU feasibility](docs/criu-feasibility.md) | research | pending | [中文](docs/criu-feasibility.md) |
| [distro](docs/distro.md) | current | [English](docs/distro.md) | [中文](docs/distro.zh-CN.md) |
| [gateway](docs/gateway.md) | design | pending | [中文](docs/gateway.md) |
| [host-integration](docs/host-integration.md) | current | [English](docs/host-integration.md) | [中文](docs/host-integration.zh-CN.md) |
| [macOS verification](docs/macos-verification.md) | current | [English](docs/macos-verification.md) | [中文](docs/macos-verification.zh-CN.md) |
| [migration](docs/migration.md) | partial | [English](docs/migration.md) | [中文](docs/migration.zh-CN.md) |
| [CRIU + Wayland PoC](docs/poc-criu-wayland.md) | PoC | [English](docs/poc-criu-wayland.md) | [中文](docs/poc-criu-wayland.zh-CN.md) |
| [protocol](docs/protocol.md) | current | pending | [中文](docs/protocol.md) |
| [security](docs/security.md) | current | [English](docs/security.md) | [中文](docs/security.zh-CN.md) |
| [snapshots](docs/snapshots.md) | current | [English](docs/snapshots.md) | [中文](docs/snapshots.zh-CN.md) |
| [Wayland magic](docs/wayland-magic.md) | current | [English](docs/wayland-magic.md) | [中文](docs/wayland-magic.zh-CN.md) |
| [roadmap](docs/roadmap.md) | plan | [English](docs/roadmap.md) | [中文](docs/roadmap.zh-CN.md) |
| [release notes 0.1.4](docs/release-notes-0.1.4.md) | historical | [English](docs/release-notes-0.1.4.md) | [中文](docs/release-notes-0.1.4.zh-CN.md) |
| [release notes 0.1.5](docs/release-notes-0.1.5.md) | historical | [English](docs/release-notes-0.1.5.md) | [中文](docs/release-notes-0.1.5.zh-CN.md) |

## Docs-first / not implemented

These pages describe work that is not in the current build:

- **gateway** — design finalized, no gateway code yet (§9 has the milestones).
- **clipboard** — protocol draft; the reference implementation lives in a fork,
  not in this tree.
- **CRIU feasibility / CRIU + Wayland PoC** — controlled experiments, not a
  turnkey migration path; cross-device restore is still open.
- **backends / nvx** — nvx is an early script, not a supported backend.
- **migration** — export/import and `.mart` verification are implemented; target
  container creation from `migration.json` and `guiSessions` replay are still open.
- **roadmap** — a plan, not a claim about today.

If a page says a feature exists, the code or tests should be the tie-breaker.
