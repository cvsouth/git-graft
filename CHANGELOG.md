# Changelog

All notable changes to git-graft are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-22

Upgrade if you use 0.1.0: after a restart, macOS can give the disk a new device number, and 0.1.0 then refuses every command in a project with `graft namespace ownership is unverified`. 0.2.0 reads 0.1.0's state and upgrades it on its first command, with nothing to run; after that, 0.1.0 can no longer read some of its caches, so don't downgrade.

Two changes in behaviour can affect scripts:

- A merge into the branch checked out in the primary now goes ahead around uncommitted edits to other paths instead of refusing.
- Several refusals that returned `operation_failed` now return their own codes: `destination_has_local_changes`, `local_changes` and `dependencies_not_shareable`. Check `code`, and `details`, rather than the message.

### Changed

- A merge or undo into the branch checked out in the primary now goes ahead around your uncommitted edits, as `git merge` does, and leaves them exactly as they were: staged changes stay staged and unstaged edits keep their bytes. It refuses only when an edit touches a path the merge changes, with `destination_has_local_changes` listing those paths in `details.paths`. A retry of a merge stopped before it wrote any file follows the same rule, so an unrelated edit no longer keeps it stuck.
- `remove` and `merge` refuse a graft with uncommitted, untracked or ignored files as `local_changes`, listing each path with its status (`added`, `deleted`, `modified`, `untracked` or `ignored`); on `remove` it notes that `remove --discard` archives them.
- A graft mirrors its primary's line endings. A primary file is reused when Git's own line-ending conversion of it, the one `git status` compares and `git add` stores, gives exactly the committed blob, verified by hashing the converted bytes or, for a file equal to its blob, by Git's index stat test. With `core.autocrlf=true`, an LF primary's text files are now cloned instead of written in full. Paths with a filter, `ident` or a working-tree encoding still come from Git, and so does any file `git add` would refuse under `core.safecrlf=true`.
- `doctor` reports `file_sharing.shared` from cloning support alone; line-ending settings no longer stop sharing.
- Creating a graft right after a commit, while the primary's newest objects are still loose, is faster. Graft reads each loose checkout blob's type and size from its object header instead of asking Git one object at a time, checks its snapshot's cloned objects without opening each one, and lists the object store with several workers. On a 3,000-file repository whose objects are all loose, a later `add` takes about 294 ms instead of 441 ms (`git worktree add`: 253 ms); at 30,000 files it takes 2.1 s instead of 3.6 s, ahead of `git worktree add` at 2.7 s.
- Simultaneous `add` calls build their grafts in parallel instead of queuing. An add holds the project lock only to reserve its name and to record the finished graft; the first build to need a missing shared cache fills it while the others wait to reuse it. `merge`, `remove`, `prune --apply` and the other commands that change project state wait for builds in progress, while `list`, `status`, lease renewal, `lock` and `unlock` do not. On 1,200 files, four simultaneous adds after a commit take a median of 1.0 s each instead of 2.0 s (`git worktree add`: 2.0 s), and eight take 2.2 s instead of 4.2 s (`git worktree add`: 3.2 s), measured on a heavily loaded machine.
- `deps share` refuses a project that cannot share, such as one with npm workspaces, overrides or linked or bundled packages, as `dependencies_not_shareable` instead of `operation_failed`. `details` names the `reason`, the offending `field` and `path` and a `fallback`: run `npm ci` in the graft, or use `deps add`. Workspaces stay unsupported because their links leave `node_modules`; the rules are in [what can be shared](docs/operations.md#what-can-be-shared).

### Fixed

- Commands no longer fail after a reboot changes the disk's device number. macOS renumbers APFS volumes when it restarts, and Linux can after a reboot or remount, so every command, `list` included, could stop with `graft namespace ownership is unverified`. Graft no longer compares the device number it stored. On macOS it identifies its private directories by inode and the volume's UUID, which survives a restart; on Linux and other Unix systems by inode, with the fixed path, project ID and ownership markers that every directory must also match. Where a filesystem reports no volume UUID, as some network and FUSE filesystems do, the inode decides. Birth times are not used, since setting a modification time into the past (`touch -t`, `rsync -a`, `cp -a`, `tar`) moves them. Windows still compares the volume serial number, which does not change. State that 0.1.0 wrote keeps working with nothing to do: the first command that finds a graft's directories unchanged rewrites their identities in the new form. A copied or replaced directory is still refused. Graft's caches verify across a reboot too, so they are reused instead of rebuilt.
- `remove` and `merge` no longer report `local_changes` for the shared `node_modules` that Graft itself attached when the graft has not changed it. The attach receipt now records the verified `contents` digest, and the install counts as local work again once any file in it is added, edited or deleted. The archive keeps every file either way, and other ignored files still count.
- `text=auto` with `core.autocrlf=true` no longer makes every file of a new graft come from Git.
- Files Git writes during a cloned checkout record their stat data, so the first `git status` in a graft reads no file contents.
- A file whose committed blob already has CRLF line endings, under `core.autocrlf` or `text=auto`, is no longer reported modified, which made `merge` refuse a clean graft on file systems without cloning. Graft rechecks a file whose timestamps changed only within the same second with Git's own comparison, as `git status` would.
- A graft checked out by Git records the change time of its top-level files after moving them into place, so its index matches the files exactly.
- A merge or undo into the branch checked out in the primary no longer overwrites an ignored file created there after Graft's last check. Git's `read-tree -u` replaces ignored files without asking, so each file the update creates is now written by `git checkout-index`, which creates it exclusively and refuses a path that is already taken. Graft then returns every other file to the previous commit and reports `destination_has_local_changes` naming the path; a retry reports `checkout_recovery_required` instead.
- A retry of an undo that stopped after Git turned a directory back into a file no longer refuses that file as untracked.
- On file systems that compare names exactly, such as Linux's, a retry or reversal of an interrupted merge or undo whose update turns a file into a directory, or back, now finishes. Git refused to remove the directory's old paths there, taking the file in their way for an untracked one, so the update stayed stopped. Graft now has Git finish the update from the tree the files hold rather than from the commit it moved away from.

## [0.1.0] - 2026-11-21

First public release.

### Added

- Grafts: `git graft add` creates a standalone Git repository next to your checkout with its own objects, branches, reflogs, stash and index. On APFS, Btrfs and XFS the files are copy-on-write clones, so a graft stores only what you change. Each reused file is verified to hold the bytes `git checkout` would write; anything unproven comes from Git.
- History modes: `--history local` (default), `isolated` and `tip`, or the `graft.history` setting.
- Shared dependencies: `git graft deps share` shares an installed `node_modules` with new grafts as writable, space-sharing copies. `deps add` adds packages, and other package managers can be registered.
- Merging: `git graft merge` imports a graft's commits into `main` or `--into <branch>`, reports conflicts before changing anything, and archives the graft. `--undo` appends a reversal commit.
- Archive and restore: `merge`, `remove` and `prune` archive a graft instead of deleting it; `git graft restore` reopens it.
- Leases: a secret per graft proves ownership; leases can be renewed and expire, and `prune` archives expired grafts.
- Also: `list`, `status`, `exec`, `lock`, `unlock`, `move`, `repair` and `doctor`. Every command prints JSON. `git graft --help` and the manual page explain the workflow, leases and error codes, and each command's `--help` has examples.
- MCP server (`git-graft mcp`, stdio) with nine tools: `graft_add`, `graft_status`, `graft_exec`, `graft_merge`, `graft_remove`, `graft_restore`, `graft_list`, `graft_renew` and `graft_add_dependency`. Execution and dependency tools require `--allow-exec`.
- An agent skill in `skills/git-graft/SKILL.md`.
- Platforms: macOS (Intel, Apple silicon), Linux (x86_64, aarch64; static musl) and Windows (x64, ARM64). Disk savings need APFS, Btrfs or XFS; on ext4 and NTFS grafts work but files are full copies. Requires Git 2.46 or newer.
- Installers: `install.sh` and `install.ps1` (checksum and provenance verified), Homebrew, Scoop, Chocolatey, winget, `.deb`, `.rpm`, `.apk` and Arch packages, and `cargo install --locked git-graft`. Releases publish `SHA256SUMS` and signed build provenance.

### Safety

- `remove` refuses unmerged commits, branches, tags and stashes (`unmerged_work`), and accepts commits a branch of the primary already contains, such as work merged with plain Git; `remove --discard` after a landed merge reports `unmerged_commits`, and a merge whose graft gained commits keeps it active (`"removed": false`).
- A damaged record (`record_damaged`) no longer blocks other grafts or prune, a moved primary fails closed (`primary_moved`), and archived leases name how to restore (`graft_archived`).
- Interrupted checkout updates resume or reverse from the files on disk, stale staging locks are reclaimed, and `exec` background jobs keep their graft busy, on Windows too.
- An interrupted `add` never leaves a graft Git can commit from: the graft holds its index lock from reservation until its checkout is published. Retrying the request archives the unfinished attempt whole, lists it in `archived_attempts` and builds the graft again; restoring that archive is refused with `unfinished_creation` and leaves it as it was.
- Merges refuse paths that the filesystem folds onto ignored files by Unicode case or normalization.
- Cache purges keep saved and registered layers, `deps layer remove` retires layers to `.git/graft/retained-layers/`, and restore reports `already_exists` and `archive_damaged` without touching the archive.

[Unreleased]: https://github.com/mprync/git-graft/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mprync/git-graft/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mprync/git-graft/releases/tag/v0.1.0
