# Changelog

All notable changes to git-graft are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `add --include PATTERN` and MCP `include` copy chosen untracked or ignored files, such as `.env`, into a new graft; `'*'` copies all of them. Nothing is copied by default.
- A `.worktreeinclude` file at the root of the checkout sets default include patterns for every graft.
- `git graft local-files` and the MCP tool `graft_local_files` list the checkout's untracked and ignored files, so an agent can choose what to bring.
- `status`, `merge` and `remove` don't count included copies as changes until the graft edits them.
- `git graft push NAME --lease TOKEN [--remote REMOTE] [--to BRANCH]` pushes a graft's commits to a branch on a remote of the primary repository, for work that goes through a pull request. It uses the primary's remote configuration, credentials and `pre-push` hook, never forces, and leaves the graft active. Without `--to` it refuses to update the remote's default branch, as recorded in `refs/remotes/REMOTE/HEAD`. SSH runs in batch mode unless an SSH command is configured. New error codes: `push_rejected`, `push_failed`, `remote_not_found`, `detached_head`, `default_branch` and `nothing_to_push`.
- MCP hosts started with `mcp --allow-push` offer the `graft_push` tool. It is a separate opt-in from `--allow-exec`.

### Changed

- On Linux filesystems that cannot clone files, such as ext4, a graft with local history now hard-links the primary's Git object files instead of exporting a copy of them, as `git clone --local` does. On a repository with 186 MB of objects, `add` takes 1.0 s instead of 16 s and a graft uses 80 MB instead of 324 MB. The graft and its primary then name the same object files, which Git never rewrites in place. Such a graft can read every object in the primary, as local history already does on Btrfs, XFS and APFS; set `graft.history` to `isolated` to keep the previous contents.
- `doctor` reports `history_sharing`: whether a graft with local history shares the primary's Git object files, and by which method (`clonefile`, `reflink` or `hardlink`). Like `source_storage`, it probes the state filesystem only and says so in `scope`.

## [0.2.0] - 2026-11-22

**Upgrade from 0.1.0:** after a restart, 0.1.0 can refuse every command with `graft namespace ownership is unverified`. 0.2.0 fixes this and upgrades existing state automatically. Don't downgrade afterwards.

**Behaviour changes that can affect scripts:**

- Merges into the checked-out branch now go ahead around uncommitted edits to other files.
- Some refusals return specific codes instead of `operation_failed`: `destination_has_local_changes`, `local_changes` and `dependencies_not_shareable`.

### Changed

- Merge and undo keep your uncommitted edits, as `git merge` does, and refuse only when an edit touches a file the merge changes.
- `remove` and `merge` list a graft's uncommitted, untracked and ignored files with their status.
- Grafts follow the primary's line endings, so `core.autocrlf=true` projects clone files instead of copying them.
- `add` right after a commit is faster: about 294 ms instead of 441 ms on 3,000 files.
- Several `add` commands now run in parallel instead of queuing: four at once take 1.0 s each instead of 2.0 s.
- `deps share` explains why a project can't share its dependencies and what to do instead.

### Fixed

- Commands no longer fail after a restart changes the disk's device number.
- A merge or undo no longer overwrites an ignored file created during the merge.
- A retry of an interrupted merge or undo now finishes on Linux when a file replaced a directory, or the reverse.
- `remove` and `merge` no longer count Graft's own unchanged shared `node_modules` as local work.
- Files committed with CRLF line endings are no longer reported as modified.
- The first `git status` in a new graft no longer reads file contents.

## [0.1.0] - 2026-11-21

First public release.

### Added

- `git graft add` creates a standalone Git repository next to your checkout. On APFS, Btrfs and XFS its files are copy-on-write clones, so a graft stores only what you change.
- `git graft merge` merges a graft into `main` or another branch and archives it; `--undo` reverses the merge.
- `git graft deps share` shares an installed `node_modules` with new grafts without duplicating it on disk.
- Archived grafts can be reopened with `restore`; leases prove which agent owns a graft.
- Also: `list`, `status`, `exec`, `remove`, `prune`, `lock`, `unlock`, `move`, `repair` and `doctor`, all with JSON output.
- An MCP server (`git-graft mcp`) and an agent skill.
- Builds for macOS, Linux and Windows; requires Git 2.46 or newer.
- Installs with Homebrew, Scoop, Chocolatey, winget, Cargo, Linux packages and install scripts.

### Safety

- `remove` refuses to delete unmerged commits, branches, tags or stashes.
- Interrupted adds and merges resume or reverse safely from the files on disk.
- A damaged record or a moved primary checkout fails safely without affecting other grafts.

[Unreleased]: https://github.com/mprync/git-graft/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mprync/git-graft/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mprync/git-graft/releases/tag/v0.1.0
