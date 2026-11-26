# Agent instructions

Before opening or updating a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md) and follow it without being asked:

- Run its checks and fix what they report.
- Update the docs, and the man page with `UPDATE_MAN=1 cargo test manual_page`, when behaviour changes.
- Update `CHANGELOG.md` as its [Changelog](CONTRIBUTING.md#changelog) section describes: one short, user-facing bullet per change, in the style of the released sections. When you change something still under `Unreleased`, edit its bullet instead of adding another. Reread the whole `Unreleased` section before you finish.
