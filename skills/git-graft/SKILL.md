---
name: git-graft
description: Give an agent task its own graft of a Git repository through the CLI or MCP, then merge, push, recover or remove it. Covers lease handling, request retries, package-manager reuse, pushing for a pull request and undoing a merge.
---

# Use Graft for an agent task

A graft is an independent copy of the repository for one task. For routine work in a trusted clone, omit `history` from `graft_add`. This honors the project setting and defaults to local history. Choose `isolated` when the graft should exclude staged blobs, dangling objects and history unique to other branches. Choose `tip` when only the starting commit and its files are needed.

1. Discover the host's tools. Create the graft at a committed base with `graft_add`, using a fresh cryptographically random UUIDv4 as `request_id`. The request ID acts as the lease; keep it secret. Keep the request and returned lease for retries; share leases only with trusted local tools and omit them from shared logs. A reused name may identify another task. Use the returned `path` for edits and ordinary Git commands. A graft holds only tracked files. If the task needs untracked or ignored files from the checkout, such as `.env` or local configuration, call `graft_local_files` first and pass `include` to `graft_add` with `.gitignore`-style patterns for just those files. Prefer naming them: `["*"]` copies all of them, including build output and directories such as `target/` and `.venv/`, which can be large. Omitting `include` uses the project's `.worktreeinclude` file, if any, and copies nothing otherwise. Check `storage.included` in the result for what was copied and skipped.
2. Before installing dependencies, inspect `storage.dependencies` and `storage.registered_dependencies` in the `graft_add` result. Use an attached installation directly. Otherwise run the manager with `graft_exec` and an argv array (use its `manager` field for wrappers), or install packages with `graft_add_dependency`. Both need a host started with `--allow-exec`. Manager registration and dependency layers are CLI-only: ask the user to run `git graft deps manager add` when a suitable registration is missing. Check each command's exit status.
3. Inspect `graft_status` and the task's changes before merging. Commit with Git, or supply a commit message only when all nonignored changes belong to the task. Resolve reported conflicts and retry `graft_merge`. Ignored files may be user data; use `discard_ignored` only when authorized.
4. Keep the returned `recovery_lease` private. Archives can retain sensitive user files. Use `graft_restore` to reopen an archived graft and receive a new active lease. Use `graft_remove` to archive a graft you no longer need, and `graft_renew` to keep a long task's lease from expiring. Call `graft_merge` with `undo` only when asked to reverse the merge; it requires the destination still to point at the recorded merge commit.

Use cache-only registration for package downloads. For installed-file reuse, a user can register a trusted relocatable directory with every tracked install input and a key for external toolchain settings. Use the registration result and the next compatible `graft_add` attachment receipt to verify reuse. [Registration arguments and examples](../../docs/operations.md#register-a-package-manager). `deps share` needs a single-package npm project whose lockfile pins registry packages only; workspaces, overrides and linked or bundled packages are refused with `dependencies_not_shareable`. [What can be shared](../../docs/operations.md#what-can-be-shared)

## Publish a graft for a pull request

`graft_push` sends a graft's commits to a branch on a remote of the primary repository. The host offers it only when started with `--allow-push`; if it is not listed, publishing is not permitted and there is nothing to work around. Use it only when the task asks for the work to be published, for example to push a branch or open a pull request. Otherwise merge with `graft_merge`, or leave the graft for the user.

1. Commit the task's changes in the graft. Only commits are pushed.
2. Call `graft_push` with the graft's `name` and `lease`. The remote branch takes the name of the graft's branch; pass `to` to choose another, which a detached `HEAD` requires. Pass `remote` only when the task names a remote other than `origin`.
3. Check the result: `branch` and `commit` are what the remote now holds, and `uncommitted_changes` counts files that were left out.
4. Open the pull request from `branch` with the tools the task provides. Graft does not open it.
5. The graft stays active. For review changes, commit and call `graft_push` again. Do not also call `graft_merge` for work that goes through a pull request unless asked.

Nothing is forced, and you must not look for another way to overwrite a remote branch. Pushed commits do not count as merged, so `graft_remove` still reports `unmerged_work`. When the user says the work is finished, ask before removing with `discard`, and keep the `recovery_lease`. [Push to a remote](../../docs/operations.md#push-to-a-remote)

## When Graft keeps work instead of finishing

Graft never deletes or overwrites work to finish a command. These results mean it kept something; read `code` and `details`, then act as below. Never delete or edit Graft's record, archive or journal files to get past one.

- `unmerged_work`: removing without `discard` found unmerged commits, branches, tags or a stash (`details.found`). Merge the graft, or ask the user before removing it with `discard`.
- `"removed": false` on a merge receipt: the graft gained commits (`graft_head`) after `graft_tip` merged. It stays active with the same lease; merge it again.
- `unmerged_commits: true` on a remove receipt: the archive holds commits made after an earlier merge (`head`). Keep the `recovery_lease` and tell the user.
- `graft_archived`: your lease belongs to an archived graft (`details.archived_by`). Restore it with `graft_restore` and use the new lease. If `details.name_in_use`, ask the user first.
- `record_damaged` or a `damaged_records` list: a record file is unreadable. Other grafts still work. Ask the user to repair it; do not touch the file.
- `primary_moved`: the primary was moved or copied. Ask the user to move it back to `details.recorded_primary`.
- `already_exists` or `archive_damaged` on restore: the name or path is taken, or the archive was changed by hand. Report it to the user; restore changed nothing.
- `destination_has_local_changes`: the user's uncommitted edits in the primary touch paths the merge or undo changes (`details.paths`). Nothing changed. Ask the user to commit or move those edits, then retry. Their edits to other paths never block a merge.
- `local_changes`: the graft has uncommitted, untracked or ignored files (`details.paths`, each with its `status`). Commit the task's changes and retry, or ask the user before removing with `discard`, which archives them. The shared `node_modules` Graft attached at `graft_add`, and files `include` copied, are not counted until you change them; if one is listed, you changed it.
- `dependencies_not_shareable` (CLI `deps share`): the project's install is not plain, lockfile-pinned registry packages, for example npm workspaces, overrides or linked packages. `details.reason`, `field`, `path` and `fallback` say why and what to do. Nothing was registered. Install in each graft instead with `npm ci`, or `deps add` with `--manager npm`; do not edit the manifest to get past it.
- `checkout_recovery_required`: retry the same merge or undo with the same lease. If it names `paths`, ask the user how to keep those edits.
- `checkout_update_set_aside`: the destination branch moved after an interrupted update. Ask the user to check `git status` in the primary, then retry the merge.
- `push_rejected`: the remote refused the update (`details.reason`), usually because the remote branch has commits the graft lacks. Nothing was pushed. Tell the user, who can fetch the remote in the primary so that you can merge the branch into the graft and push again. Push under another name with `to` only if the task allows a new branch.
- `push_failed`: the push could not be completed, for example for network or authentication reasons (`details.stderr`). Report it to the user. Do not retry in a loop or try another remote.
- `remote_not_found`: the primary has no remote of that name; `details.remotes` lists the ones it has. Use one only if the task names it, otherwise ask the user.
- `detached_head` or `nothing_to_push`: pass `to` to name the remote branch, or commit first.
- `default_branch`: the graft's branch has the name of the remote's default branch. Pass `to` with another branch name. Pass `to` with the default branch only if the task says to update it.
- `git_busy` or `workspace_busy`: another Git or Graft command, or a background job started with `graft_exec`, is running. Wait or stop your own background job, then retry.
- `retained_path` and `kept` (CLI `deps layer remove` and `gc --purge-cache`): layers were kept, not deleted. Leave them for the user.

[Full table of codes and fields](../../docs/operations.md#safety-error-codes-and-receipt-fields)
