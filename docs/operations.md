# Operating grafts

## Identity and lifecycle

`git graft add NAME --base REV` creates a graft: a standalone repository with its own Git objects, refs, index and working files. Routine CLI and MCP calls honor the primary repository's `graft.history` setting and default to local history. Set `graft.history=isolated` when unrelated objects must be excluded, or choose `tip` for a shallow graft containing only the starting commit and its files. The result gives its `path` and an active `lease`. A name can be reused, so keep the lease with the task and share it only with trusted local tools. Avoid including leases in shared logs or reports. CLI commands that change a graft accept `--lease` or `GIT_GRAFT_LEASE`; MCP tools take `lease`. Use `git graft list` and `git graft status NAME` to inspect grafts, and `git graft status NAME --env` to see a graft's runtime and cache paths. Run `git graft --help` (the manual page) or `git graft -h`, and `git graft COMMAND --help`, for the available options.

Ordinary Git commands operate on the graft's private repository. A branch or stash created there is local to it; `merge` transfers the selected committed work into the original repository. A nested `add` can use a parent graft's private commit as its base. Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Graft does not fetch missing history. Merging uses the destination’s existing boundaries and fails before changing it if ancestry cannot be resolved. Replacement refs, legacy Git grafts and repository-local checkout filters are declined.

Use `lock` and `unlock` to protect an active graft from moves, removal and pruning. `move` changes its path on the same filesystem. After a manual rename, `repair` verifies its recorded identity before accepting the new path. `remove NAME` archives one graft. `prune` previews the expired grafts that can be archived and `prune --apply` archives them.

## Copy untracked and ignored files

A graft starts with only the files of its commit. Untracked and ignored files in the checkout, such as `.env`, local configuration or build output, stay behind unless the request names them:

```sh
git graft add api --include .env --include 'config/*.local.json'
git graft add api --include '*'      # every untracked and ignored file
```

Over MCP, `graft_add` takes the same patterns as `include`, and `graft_local_files` (CLI `git graft local-files`) lists the checkout's untracked and ignored files and directories, so an agent can choose what its task needs. Patterns use `.gitignore` syntax and match only files Git does not track; a nested repository is skipped.

When a request names no patterns, a `.worktreeinclude` file at the root of the invoking checkout supplies them, in the same syntax, so a project can set a default once. Without one, nothing is copied. Patterns in a request replace the file's, and an empty MCP `include` list copies nothing.

Copies are clones on a filesystem that supports them, and full copies elsewhere. A copy never replaces a file of the checkout or an attached install, and is never written through a symlink; such paths are listed in the receipt's `skipped` with a reason (`exists`, `blocked`, `tracked`, `nested_repository`, `vanished`, `unreadable`, `symlink` on Windows, or `special`). The `add` receipt records them as `storage.included`: `source` (`request` or `.worktreeinclude`), `patterns`, `files`, `bytes`, `skipped_count` and up to 50 `skipped` entries.

A copied file that is untracked in the graft is added to the graft's `.git/info/exclude`, so `merge --commit` never commits it; commit one deliberately with `git add -f`. `status`, `merge` and `remove` do not count a copy as a change while it is the file Graft installed. Once the graft edits, replaces or deletes it, it is listed as `ignored` again. Copies stay in the graft's archive, like every other file.

## Run commands and install dependencies

```sh
git graft exec task --lease "<lease>" -- cargo test --locked
git graft deps add task --lease "<lease>" --manager npm
git graft deps add task --lease "<lease>" --manager pnpm zod
git graft status task --env
```

An empty package list requests a locked installation or fetch. `git graft deps add` supports npm, pnpm, Bun, Cargo and uv. Graft configures project-scoped caches for its managed commands. `--offline` requires the requested artifacts to be available locally. Package scripts run with the user's permissions. The MCP tools `graft_exec` and `graft_add_dependency` require the host's `--allow-exec` option.

| Package manager | Reuse across grafts | Per-graft work |
| --- | --- | --- |
| npm | A compatible registered installation can attach automatically on a copy-on-write volume. The npm download cache is shared across managed commands. | Each graft has its own manifest, lockfile and writable installation. An addition can change transitive packages and metadata. |
| Cargo | `git graft exec` and `deps add` use a project-level `CARGO_HOME` for crate downloads and Git sources. This needs no registration. | Each graft has its own manifests, lockfile, Cargo target and build directories. A build can compile the same crate in two grafts. |
| NuGet | Register `dotnet` with `NUGET_PACKAGES` to give SDK-style projects one package store per Graft project. NuGet can reuse several cached versions side by side. | Restore generates each project's `obj/project.assets.json`. Builds normally write under `bin/` and `obj/`. Legacy `packages.config` projects can also copy packages into a project directory. |

Adding a package in one graft changes that graft’s declarations and resolved versions. Other grafts use a cached version when their own project files request it. Programs run directly from a terminal use the terminal’s environment. [Cargo cache](https://doc.rust-lang.org/cargo/guide/cargo-home.html) · [NuGet package storage](https://learn.microsoft.com/en-us/nuget/consume-packages/managing-the-global-packages-and-cache-folders) · [NuGet restore](https://learn.microsoft.com/en-us/nuget/consume-packages/package-restore)

## Register a package manager

You can teach Graft how a tool locates its cache once per project. Choose the executable name and supply the manager’s documented cache environment variable or command-line option. Graft creates an owned cache directory and passes its path on matching `git graft exec` calls. The first restore can populate this directory even if the same packages already exist in a machine-wide cache.

```sh
git graft deps manager add dotnet --cache-env NUGET_PACKAGES
git graft deps manager add mvn '--cache-arg=-Dmaven.repo.local={cache}'
git graft deps manager list
git graft exec task --lease "<lease>" -- dotnet restore
git graft exec task --lease "<lease>" --manager mvn -- ./mvnw verify
```

The default variable is `GRAFT_PACKAGE_CACHE`. A registration can also set one manager-specific variable and prepend ordered arguments. Each `--cache-arg` represents one argument and `{cache}` expands to the cache directory. Executable matching uses the basename. Use `--manager` to select the same registration for a wrapper. The package manager controls version resolution, concurrent cache writes and the work needed to complete a restore. Supply cache settings suitable for shared use. [Maven local repository property](https://maven.apache.org/components/ref/4.0.0-rc-7/configuration.html)

### Reuse an installed directory

With a trusted installed tree, you can register its location and every tracked file that determines the install. For example, a Ruby project that installs under `vendor/bundle` can register that tree from its primary checkout. Replace the example key with the actual runtime, package manager version, ABI and install settings.

```sh
git graft deps manager add bundle --install-dir vendor/bundle \
  --input Gemfile --input Gemfile.lock --key ruby-3.4-bundler-2.6-default
```

The installed tree must be relocatable, free of private data and independent of unlisted inputs. Use a committed ignore rule for the entire installation directory so each graft inherits it. To register it from an existing graft, also supply `--graft NAME --lease TOKEN`. Run registration again after a compatible install with new committed inputs to cache that version as well. Register a new key when external toolchain or install settings change.

During a full `add`, Graft checks the declared inputs, platform, key and destination ignore rules, then attaches verified files into a vacant path using native copy-on-write. Inspect each entry in `storage.registered_dependencies`. An `attached` status means the installed files are ready. With `inputs_mismatch` or `unavailable`, run the manager’s ordinary install in that graft. If a later ignore-rule edit exposes an attached package, `merge --commit` asks you to restore the rule or commit the intended changes explicitly with Git. Cache routing still works on a filesystem that cannot clone files. Sparse, empty and `--no-deps` checkouts skip attachment.

`git graft deps manager remove NAME` removes the settings and retains cached files and existing installs. When all grafts are archived, `git graft prune --purge-cache` previews eligible caches, registered installations and shared dependency layers that a later `deps share` replaced. Add `--apply` to remove verified entries. Layers saved with `deps layer save` or `deps share --snapshot`, and the layer `deps share` currently registers, are reported under `kept` and stay until `deps layer remove`. Changed or unrecognized generations remain available for inspection.

Manager registration and dependency layers are CLI-only. MCP hosts started with `--allow-exec` expose `graft_exec` and `graft_add_dependency` and attach registered installations on `graft_add`; without `--allow-exec` those tools are unavailable and installations are not attached.

## Reuse an npm installation

From a primary checkout with a trusted, relocatable install matching the committed lockfile, register it once:

```sh
git graft deps share --key node-and-npm-version-and-install-options
git graft add task --base main
```

The key attests to the toolchain, options and relevant environment. The install must contain no private data and be usable after source-only edits. Compatible full `add` calls on a clone-capable volume attach a private writable view; inspect `storage.dependencies.status`. An unavailable or mismatched layer is not installed automatically. Use `--no-deps` for a source-only graft. Sparse grafts and MCP hosts without `--allow-exec` do not attach the install.

### What can be shared

`deps share` registers a project only when its whole install is plain registry packages that the lockfile pins:

- npm lockfile version 2 or 3, with `package.json` and the lockfile's root entry agreeing on every dependency section.
- Every dependency is a registry version or an `npm:` alias, and every locked package has an HTTPS `resolved` URL and a SHA-256 or stronger `integrity` hash. Git, `file:`, `link:`, `workspace:` and tarball dependencies are refused.
- No `workspaces`, `overrides`, `bundleDependencies` or `bundledDependencies` in `package.json` or the lockfile root, and no `link` or `inBundle` package in the lockfile.
- `packageManager`, if present, names npm.

A project outside these rules is refused with `dependencies_not_shareable`. `details.reason` says why, `details.field` names the offending key (such as `workspaces` or `dependencies.NAME`), `details.path` names the file or lockfile entry that holds it when there is one, and `details.fallback` gives the way forward. Nothing is registered. Install in each graft instead: run `npm ci` there, or `git graft deps add NAME --lease TOKEN --manager npm`. Neither needs a shared layer, so neither clones the primary's files.

npm workspaces stay refused. npm installs each workspace member as a symlink in `node_modules` to the member's own folder, such as `node_modules/app -> ../packages/app`, and puts conflicting versions in that folder's own `node_modules`. A layer holds one `node_modules` and verifies that every link in it resolves inside it. These links leave it by design, and the members' nested installs lie outside it. Sharing them would mean verifying links against tracked source and layering several directories, which would loosen the check that keeps a shared layer from reaching outside its tree. Graft does not take that on for this case.

### Remove and merge after attaching

A shared install that Graft attached counts as the graft's own, not as local work, while it is unchanged. The attach receipt, `storage.dependencies`, records the `contents` digest of the files Graft verified and attached. `remove` and `merge` hash the graft's `node_modules` again. If it matches, the install is left out of the `local_changes` check, and the archive still holds every file. If any file was added, edited or deleted, or the receipt has no `contents` because the graft predates this check, `node_modules/` is reported as `ignored` as before. Other ignored files always count.

For an exact-source manual snapshot, use `deps share --snapshot --key KEY` (from the primary checkout) or `deps layer save NAME --key KEY` (from a graft), then `deps layer restore NAME --key KEY`. `deps layer remove NAME --key KEY` retires a saved layer: on every platform it moves the layer to `.git/graft/retained-layers/` and reports `retained_path`, so a patched install is never lost. Delete that directory yourself once you no longer need it; Graft never deletes it. Manual transfers require copy-on-write unless `--copy` is explicitly supplied. Review `--help` for leases and options. Treat installed layers as executable content shared within one trust domain.

## Merge and recover

`merge` integrates committed work into a local branch by fast-forward, a merge commit or `--squash`, subject to the destination's state. `--commit MESSAGE` stages nonignored changes in the graft first; use it only when all such changes belong in the task. If `--commit` detects a path with both staged and unstaged changes, it returns `partially_staged` without replacing the staged version. Commit the staged version or unstage it explicitly before retrying. This also applies to a staged deletion whose path has been recreated. Conflicts remain available for inspection. `--reconcile` supports a narrow offline npm lockfile conflict case; other conflicts need normal resolution. Ignored files block a normal merge; `--discard-ignored` retains them in the archived graft.

The receipt from `merge` has `"action":"merge"`, the `target` branch, its `previous` and new `commit`, and `graft_tip`; the receipt from `remove` has `"action":"remove"`. Both include `recovery_lease`. Retain it. The default merge commit message is `Merge graft NAME into TARGET`.

If the graft gains commits after its merge reaches the destination, for example from a hook or a second agent, `merge` does not archive it: the archive would present those commits as merged. The receipt then has `"removed": false`, the merged `graft_tip` and the newer `graft_head`. The graft stays active with the same lease and every file, and the next `merge` lands the newer work. `--undo` applies only to a merge that archived its graft.

If `merge --commit` is killed while it stages the graft, it leaves the graft's `.git/index.lock`. Retry the same merge: Graft releases a lock that carries its own staging marker, because it then holds the graft's workspace lock and Git can still read the index. A lock that Graft did not create is reported as `git_busy` and kept.

`restore NAME --lease TOKEN` reopens the archive and issues a fresh active lease. `merge NAME --undo --lease TOKEN` appends a reversal commit only while the destination is still at the recorded merge commit. `list` includes recoverable grafts. Archives retain repository and local file state, which can include credentials and other sensitive user data, and can occupy disk indefinitely. See [safety and recovery](safety.md).

For a disconnected `add`, retry with the same client-generated, cryptographically random UUIDv4 `request_id` and the same options. The request ID acts as the graft lease; keep it secret and share it only with trusted local tools.

### Interrupted add

If `add` stops before it finishes, for example because it was killed during the checkout, it reports `spawn_incomplete` or leaves a graft with `"status": "pending"`. Until the checkout's index is published, the unfinished graft holds `.git/index.lock`, which Graft writes when it reserves the graft, so Git cannot commit the files that are not checked out yet as deletions. The `spawn_incomplete` recovery message says whether the lock is still held. Do not delete that lock or work in a pending graft.

Retry the same request. Graft deletes nothing: it archives the whole unfinished attempt under a new lease, then builds the graft again at the same name and path. The reply lists each archived attempt in `archived_attempts`, with its `archive_path` and `lease`, and so do the graft's later `status`, merge and remove receipts, `restore` replies and `prune` entries. Copy anything you need from `archive_path`; its files are kept byte for byte, including untracked, ignored and edited files. A retry while the first attempt is still running waits for it and returns its graft. On Windows, a process that still has a file open in the attempt can block the archive move; the retry then reports `workspace_busy` and leaves the attempt in place, so retry again once that process exits.

An archived creation that never finished cannot be restored: `restore` with its lease, or with the lease of a pending graft you archived with `remove --discard`, fails with `unfinished_creation` and leaves the archive exactly as it was. `details.archive_path` names it; copy what you need from there, or run `add` again.

## Uncommitted edits in the primary checkout

A merge or undo into the branch checked out in the primary works like `git merge` with local edits. Your uncommitted edits stay exactly as they were, as long as none of them touches a path the merge changes. Edits are staged and unstaged changes and untracked files. Staged changes stay staged, unstaged edits keep their bytes, line endings included, and untracked files stay where they are. A retry of a merge that was stopped before it wrote any file follows the same rule.

If any edit touches a path the merge changes, Graft refuses with `destination_has_local_changes` and changes nothing. `details.paths` lists the overlapping paths, and `details.checkout`, `target`, `previous` and `commit` name the checkout and the update. Commit those edits or move them aside, then retry. Edits to other paths can stay.

These edits count as touching a changed path:

- an edit, deletion, staged version or type change (file, symlink or directory) of a file the merge changes, including a staged version that already equals the merged one;
- an untracked file where the merge adds a file, including one whose name differs only by case or Unicode normalization on a filesystem that treats both spellings as one name;
- either side of a rename, because a rename is a deletion and an addition;
- a file inside a path the merge turns into a file, or a file the merge turns into a directory;
- a submodule with new commits or edits, when the merge moves that submodule. A merge that leaves the submodule alone never touches it.

Just before Git writes any file, its dry run checks every file the merge replaces again, so an edit made while the merge prepares is refused the same way and leaves no recovery state behind. An ignored file the merge would overwrite or write into also stops it with `destination_has_local_changes`, naming the paths the merge would write; move that file aside, then retry. A file that appears later, even while Git writes, is never overwritten either: Git creates each new file exclusively and refuses a path that is already taken, and Graft then returns every other file to the previous commit and reports `destination_has_local_changes` naming that path. As with `git merge`, an edit to a tracked file the merge changes, made in the instant between Git's final check of that file and its write, can still be replaced. Unresolved conflicts, assume-unchanged or skip-worktree flags and sparse checkouts in the primary still block the merge.

## When two tasks edit the same file

Two grafts created from the same commit each get a writable checkout of that version. Saving or committing a file in one leaves the other graft and the primary at their own versions. When both tasks merge into the same branch, the first successful merge updates that branch. Graft uses Git's three-way merge for a second, diverged task. Edits to separate parts of a file can merge automatically.

If Git reports conflicting edits, Graft returns the paths and keeps the second graft. From that graft, fetch the current destination and merge it locally. This example uses a destination named `main`.

```sh
git fetch --no-tags /absolute/path/to/primary refs/heads/main
git merge FETCH_HEAD
git status
```

Resolve any conflicts, stage the resolutions, commit the merge and run the project's tests. From the primary, retry `git graft merge NAME --into main --lease TOKEN` with the same graft lease.

## Interrupted checkout update

A merge or undo into the branch checked out in the primary can stop while Git rewrites its files, for example when a checkout filter fails, the disk fills or the process is killed. It returns `checkout_recovery_required` with `target`, `previous`, `commit`, `graft_tip` and `recovery_path`, and keeps a journal there with the original index as `index.before`. Retry the same command with the same lease. Nothing needs to be cleaned up first.

On retry, Graft compares each file the update changes with its previous and merged versions and lets Git's two-tree update finish from there. A merge from another graft first reverses the stopped update in the same way, returning those files to the previous commit, and the stopped graft keeps its plan for its own retry. Neither direction deletes or overwrites untracked or ignored files, edits to other files, or changes staged since the interruption. While the native Git update of the stopped attempt is still running, the retry waits and reports it.

Recovery stops before writing any file, and names the paths in `paths`, when a changed file holds neither version or a changed path has a newly staged version. It also stops, naming them, when files appear where it writes; Git keeps them as they are. Keep a copy of those edits, return each file to either version or record and unstage the staged version, then retry. If the process tree was killed, Git can leave its own `HEAD.lock` or branch lock; Graft reports the lock path and does not remove it. Follow Git's advice once no Git command is running, then retry.

If the destination branch moved or is no longer checked out, the update can no longer finish. Graft returns `checkout_update_set_aside`, changes no further file and keeps the journal at the reported `recovery_path`. Files the stopped update had already written stay as they are and show in `git status`. Check `git status` in the primary, then retry the merge, which plans again against the current branch. The graft cannot be pruned while its checkout update is pending.

## Safety error codes and receipt fields

These codes and fields tell you that Graft kept something instead of deleting or overwriting it. Every error here leaves the graft, its archive and the primary's files as they were. Error codes appear as `code` in CLI JSON errors and MCP tool results.

| Code or field | Meaning | What to do |
| --- | --- | --- |
| `unmerged_work` | `remove` without `--discard` found work a merge never took: commits, a moved or detached HEAD, other branches, tags, a stash or other refs. `details.found` lists them. | Merge the graft, or archive it with `remove --discard` and keep the `recovery_lease`. |
| `unmerged_commits` (with `head`) | A `remove --discard` receipt with `"action":"remove"` after an earlier merge had landed. `commit` and `graft_tip` name that merge; `head` is the archived graft HEAD whose later commits were never merged. | Keep the `recovery_lease`. Restore the graft to merge the later commits. |
| `"removed": false` (with `graft_head`) | A merge receipt: the graft gained commits after `graft_tip` was merged, so it stays active with the same lease and files. | Merge it again to land `graft_head`. |
| `graft_archived` | The lease belongs to an archived graft. `details.archived_by` is `merge`, `remove` or `remove_or_prune`; `details.name_in_use` says whether another graft now holds the name. | Run `restore NAME --lease TOKEN`. If the name is in use, archive that graft first. |
| `record_damaged` | The record file of this graft cannot be read or conflicts with another record. `details.file` names it. Graft never rewrites, moves or deletes it. | Repair the file from a backup or move it aside yourself. Other grafts keep working. |
| `damaged_records` | A `list` and `prune` field naming each damaged record file and its error. Prune skips them. | As for `record_damaged`. |
| `primary_moved` | Every record names one other primary path, so the primary was moved or copied. `details.recorded_primary` is that path. Nothing is read or written. | Move the primary back to `recorded_primary`. Do not edit or move the record files. |
| `already_exists` | `restore` found another graft using the name, or a file or directory at the graft's path. | Merge or remove the other graft, or move the existing path aside after saving its data, then retry. |
| `archive_damaged` | The archive at `details.archive_path` is missing or was changed by hand. Restore leaves it exactly as found. | Copy what you need from it, or put back the missing Git files and retry. |
| `spawn_incomplete` | `add` stopped before the graft was built. `details.path` is the pending graft, which holds its index lock so Git cannot commit it. | Retry the same request. See [interrupted add](#interrupted-add). |
| `archived_attempts` | Named in `add`, `status`, merge, remove, `restore` and `prune` replies about a graft whose request was retried: archives of its unfinished attempts, each with `archive_path` and `lease`. | Copy what you need from `archive_path`. |
| `unfinished_creation` | `restore` was asked for an archive of a creation that never finished. `details.archive_path` names it; nothing was changed. | Copy what you need from the archive, or run `add` again. |
| `destination_has_local_changes` | Uncommitted edits in the primary checkout touch paths the merge or undo changes. `details.paths` lists them; nothing changed. | Commit those edits or move them aside, then retry. See [uncommitted edits in the primary checkout](#uncommitted-edits-in-the-primary-checkout). |
| `local_changes` | `merge` or `remove` found uncommitted, untracked or ignored files in the graft. `details.paths` lists up to 200 of them, each with `status` `added`, `deleted`, `modified`, `untracked` or `ignored`, and `details.count` gives the total. On `remove`, `details.discard_archives` is `true`: `remove --discard` archives every one of them with the graft. | Commit them and retry, or archive the graft with `remove --discard` and keep the `recovery_lease`. A shared install that Graft attached, and files `add` copied with `include`, are not listed while the graft has not changed them. |
| `dependencies_not_shareable` | `deps share` found a project whose install is not plain, lockfile-pinned registry packages, such as npm workspaces. `details.reason` says why, `details.field` and `details.path` name the offending key and file when known, and `details.fallback` gives the alternative. Nothing was registered. | Install in each graft: run `npm ci` there, or `git graft deps add NAME --lease TOKEN --manager npm`. See [what can be shared](#what-can-be-shared). |
| `checkout_recovery_required` | A checkout update of the primary was interrupted. `details.paths` lists files that need attention. | Retry the same merge or undo with the same lease. See [interrupted checkout update](#interrupted-checkout-update). |
| `checkout_update_set_aside` | An interrupted checkout update can no longer finish because its branch moved or is no longer checked out. The journal stays at `details.recovery_path`. | Check `git status` in the primary, then retry the merge. |
| `git_busy` | A Git lock or an unreadable index in the graft or primary is held by a command Graft did not start, or cannot prove has exited. | Wait for the Git command to finish. Remove a lock only once no Git command is running. |
| `workspace_busy` | Another Graft command, or a process started by `exec` (including background jobs), holds the graft. | Wait, or stop the background job, then retry. |
| `kept` | A `gc --purge-cache` field listing saved layers (`saved_layer`) and the registered shared layer (`registered_shared_layer`) it did not delete. | Retire them with `deps layer remove` when no longer needed. |
| `retained`, `retained_path` | `deps layer remove` moved the layer to `.git/graft/retained-layers/` instead of deleting it. | Delete `retained_path` yourself once you no longer need the install. |
| prune `reason` `error`, `archive_occupied`, `busy` | Prune could not check or archive this graft (`error` carries the message), found its archive path occupied, or found it busy. It went on with the others. | Fix the named problem, then rerun `prune`. |
