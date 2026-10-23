# Contributing

Thanks for helping improve this project. This guide keeps changes reviewable and
reproducible.

## Ground rules

- Open pull requests against **`main`** from your fork. Do not push to `main`.
- Keep one logical change per pull request; split unrelated refactors.
- The PR body must start with `Base: main`.
- Do not commit credentials, tokens, private keys, customer data, or full logs.
- Add or update tests for behavior changes. State what you ran in the PR body.
- Cross-repository changes need a tracking issue named
  `CS-YYYYMMDD-<slug>` and the PR title prefixed with `[CS-...]`.

## Development

```bash
# Rust workspace
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# Console PWA
cd ../console
npm ci
npm run build
```

Container and compositor paths need a Linux host with the packages listed in
`docs/distro.md`. The integration scripts under `tests/integration/` are the
source of truth for end-to-end checks.

## Pull request checklist

- [ ] `Base: main` is the first line of the PR body
- [ ] Tests/build for the touched components pass
- [ ] No secrets or personal paths in code, docs, or fixtures
- [ ] Public behavior and docs are updated together
- [ ] New dependencies are justified and license-compatible

## Commit messages

Use a short imperative subject with a scope, for example:

```text
fix(agentd): reject stale control markers
feat(desktop): add gateway node selection
docs: clarify snapshot restore limits
```

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, the same license as this repository.
