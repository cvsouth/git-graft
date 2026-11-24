---
name: git-graft
description: Give an agent task its own graft of a Git repository through the CLI or MCP, then merge, recover or remove it. Covers lease handling, request retries, package-manager reuse, taking turns on a shared resource and undoing a merge.
---

# Use Graft for an agent task

A graft is an independent copy of the repository for one task. For routine work in a trusted clone, omit `history` from `graft_add`. This honors the project setting and defaults to local history. Choose `isolated` when the graft should exclude staged blobs, dangling objects and history unique to other branches. Choose `tip` when only the starting commit and its files are needed.

1. Discover the host's tools. Create the graft at a committed base with `graft_add`, using a fresh cryptographically random UUIDv4 as `request_id`. The request ID acts as the lease; keep it secret. Keep the request and returned lease for retries; share leases only with trusted local tools and omit them from shared logs. A reused name may identify another task. Use the returned `path` for edits and ordinary Git commands. A graft holds only tracked files. If the task needs untracked or ignored files from the checkout, such as `.env` or local configuration, call `graft_local_files` first and pass `include` to `graft_add` with `.gitignore`-style patterns for just those files. Prefer naming them: `["*"]` copies all of them, including build output and directories such as `target/` and `.venv/`, which can be large. Omitting `include` uses the project's `.worktreeinclude` file, if any, and copies nothing otherwise. Check `storage.included` in the result for what was copied and skipped.
2. Before installing dependencies, inspect `storage.dependencies` and `storage.registered_dependencies` in the `graft_add` result. Use an attached installation directly. Otherwise run the manager with `graft_exec` and an argv array (use its `manager` field for wrappers), or install packages with `graft_add_dependency`. Both need a host started with `--allow-exec`. Manager registration and dependency layers are CLI-only: ask the user to run `git graft deps manager add` when a suitable registration is missing. Check each command's exit status.
3. Inspect `graft_status` and the task's changes before merging. Commit with Git, or supply a commit message only when all nonignored changes belong to the task. Resolve reported conflicts and retry `graft_merge`. Ignored files may be user data; use `discard_ignored` only when authorized.
4. Keep the returned `recovery_lease` private. Archives can retain sensitive user files. Use `graft_restore` to reopen an archived graft and receive a new active lease. Use `graft_remove` to archive a graft you no longer need, and `graft_renew` to keep a long task's lease from expiring. Call `graft_merge` with `undo` only when asked to reverse the merge; it requires the destination still to point at the recorded merge commit.

Use cache-only registration for package downloads. For installed-file reuse, a user can register a trusted relocatable directory with every tracked install input and a key for external toolchain settings. Use the registration result and the next compatible `graft_add` attachment receipt to verify reuse. [Registration arguments and examples](../../docs/operations.md#register-a-package-manager). `deps share` needs a single-package npm project whose lockfile pins registry packages only; workspaces, overrides and linked or bundled packages are refused with `dependencies_not_shareable`. [What can be shared](../../docs/operations.md#what-can-be-shared)

## Take turns on something every graft shares

A graft has its own files, but a project can have one development server on a fixed port, one local database or one device. Before you use such a thing, take a turn on it. Graft does nothing to the resource itself: it only records whose turn it is, so the turn works when every agent asks.

1. Call `graft_turn_take` with your graft's `name` and `lease` and a `resource` name. Use the name the project's instructions give, so that every agent asks for the same one.
2. `granted: true` means the turn is yours until `expires_at`. During long use, call `graft_turn_take` again before then to renew it.
3. `granted: false` means another graft holds the turn or is ahead of you, and `position` is your place in line. Do not use the resource. Call again within 300 seconds to keep your place, or pass `wait` in seconds, shorter than your limit for one tool call, to keep asking in a single call.
4. Stop what you started and call `graft_turn_release` as soon as you finish, and also when you stop waiting. If you stop without releasing, the others wait until your turn expires.

`graft_turn_list` shows who holds and who waits for each resource. On the CLI these are `git graft turn take`, `turn release` and `turn list`. [Rules for turns](../../docs/operations.md#take-turns-on-a-shared-resource)

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
- `git_busy` or `workspace_busy`: another Git or Graft command, or a background job started with `graft_exec`, is running. Wait or stop your own background job, then retry.
- `retained_path` and `kept` (CLI `deps layer remove` and `gc --purge-cache`): layers were kept, not deleted. Leave them for the user.

[Full table of codes and fields](../../docs/operations.md#safety-error-codes-and-receipt-fields)
