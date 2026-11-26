## What and why

<!-- What does this change, and why? Link the issue it fixes, if any. -->

## Checks

Run these before requesting review (see [CONTRIBUTING.md](../CONTRIBUTING.md)):

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `npm test` (end-to-end suite)
- [ ] Docs, the man page (`UPDATE_MAN=1 cargo test manual_page`) updated if behavior changed, and `CHANGELOG.md` as [its rules](../CONTRIBUTING.md#changelog) describe

Leases and private repository data are removed from everything in this pull request.
