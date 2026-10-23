# Contributing to mixumOS

Thanks for helping. mixumOS is documentation-first: a behavioral change should
update the relevant `docs/` page and a test that locks it.

## Before you start

- Read [`docs/architecture.md`](docs/architecture.md) and [`docs/security.md`](docs/security.md).
- Protocol changes belong in the [MAP](https://github.com/EmergeShift/Mixum-Agent-Protocol) repository first (spec + schema + vector).
- Security issues: see [`SECURITY.md`](SECURITY.md). Do not open a public issue.

## Development

```bash
cd rust
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cd ../console
npm ci && npm run build
```

The C compositor builds on wlroots 0.18; CI compiles it in Alpine. Platform
differences belong in the relevant crate, not in shared protocol code.

## Pull requests

- One logical change per PR; keep the diff reviewable.
- Add or update a test for observable behavior.
- Update `docs/` for user-visible changes.
- No CLA; contributions are accepted under Apache-2.0.

## Commit style

Imperative subject, reason in the body. Example:

```
fix(display): avoid double NotFound on attach/close race

The broker read the window a second time after inserting the subscriber...
```
