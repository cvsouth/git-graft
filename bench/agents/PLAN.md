# Agent swarm benchmark

A real end-to-end run: a team of coding agents builds a full-stack web app together, every agent working in its own graft. The run measures how git-graft behaves under that load and proves no agent's work is lost.

## What the run answers

| Question | Measured by |
| --- | --- |
| How many grafts were made, merged, removed and restored? | Every `graft_*` call, from the MCP event log |
| How much disk did they take, compared with `git worktree`? | APFS file data under the run directory, sampled every 30 s, plus a computed worktree baseline |
| Did any graft lose a change? | A change ledger checked against the final repository and archives |
| How did merging go? | Conflicts, retries, set-aside updates and time per merge |
| Did agents clean up after themselves? | Active, archived and expired grafts at the end, and disk returned by `prune` |
| How fast was it? | Time to create each graft, time until its first `git status`, time to merge |

## The project the agents build

**Tasklane**, a small team task board, as a monorepo:

```
tasklane/
  apps/web/        React + Vite + TypeScript frontend
  apps/api/        Node + Fastify + TypeScript API, SQLite through Drizzle
  packages/shared/ Shared types and validation (zod)
  e2e/             Playwright tests
```

Features: sign-up and sign-in, boards, columns and cards, drag and drop, comments, live updates over WebSockets, search, and an activity feed. The work is big enough to need 12 agents running at once. It also forces real overlap: several agents edit `packages/shared` and the API routes, so merge conflicts happen naturally.

Dependencies are installed once in the primary and shared with every graft through `git graft deps share`. No agent runs a cold `npm ci`.

## Agents

Every agent talks to git-graft only through its MCP server (`git-graft --repo <run>/tasklane mcp --allow-exec`) and works only inside its own graft.

| # | Agent | Job | Grafts |
| --- | --- | --- | --- |
| 1 | Coordinator | Splits the work into tickets, assigns them, merges finished grafts in dependency order and resolves conflicts | 1 per conflict it resolves |
| 2 | Architect | Scaffolds the monorepo, tooling and CI scripts, then registers the shared install | 1 |
| 3 | Shared types | Domain types and zod schemas in `packages/shared` | 1 per ticket |
| 4 | API auth | Sign-up, sign-in, sessions | 1 per ticket |
| 5 | API boards | Boards, columns, cards, ordering | 1 per ticket |
| 6 | API realtime | WebSocket updates and the activity feed | 1 per ticket |
| 7 | Web shell | Routing, layout, design tokens, API client | 1 per ticket |
| 8 | Web board | Board view, drag and drop, card editor | 1 per ticket |
| 9 | Web auth and settings | Auth screens, profile, settings | 1 per ticket |
| 10 | API tests | Unit and integration tests for the API | 1 per ticket |
| 11 | Web tests | Playwright end-to-end tests | 1 per ticket |
| 12 | Docs | README, API reference, architecture notes | 1 per ticket |
| 13 | Chaos | Interrupts others on a schedule (see "Stress phases"); never writes product code | short-lived grafts |
| 14 | Auditor | At the end: verifies the ledger, cleanup and disk, then writes the report | read-only |

Each ticket is one graft: `graft_add` → work and commit → `graft_merge` into `main`. An agent that is asked to change its work after review gets a fresh graft from the new `main`. Target: 60–100 grafts over the run, with at least 10 active at once.

## Phases

1. **Setup** (Architect, about 10 minutes). Scaffold, install dependencies, `deps share`, commit. The coordinator writes the tickets to `run/tickets.json`.
2. **Build waves** (about 2 hours). Three waves of tickets with dependencies between them: shared types before the API, the API before web screens, tests last. All agents work at once whenever their tickets allow.
3. **Stress phases**, run by the Chaos agent between waves:
   - **Burst:** 50 `graft_add` calls in 60 seconds from several agents, then all 50 removed.
   - **Merge storm:** eight grafts with non-conflicting changes merge into `main` at the same moment, then four with overlapping changes.
   - **Crash:** an agent process is killed with SIGKILL in the middle of `graft_merge`, `graft_add` and `graft_remove`; the run then checks that a retry recovers.
   - **Interference:** an agent tries to merge, remove or exec in another agent's graft with a wrong lease (must be refused), and edits a primary file while a merge into the checked-out `main` is running (the edit must survive).
   - **Restore:** an agent removes a graft with unmerged work by mistake, using `--discard`, then restores it and merges it.
4. **Cleanup** (each agent): remove or merge every graft it made, then run `prune` (preview, then `--apply`) and report what is left.
5. **Audit** (Auditor): checks, stats and the report.

## Instrumentation

All files go under `run/` next to the project; nothing outside the run directory is touched.

- **MCP event log** (`run/events.jsonl`): a thin stdio proxy between each agent and `git-graft mcp` records every request and response. Fields: time, agent, tool, graft name, duration, result code, storage receipt, and the lease's SHA-256 (never the lease itself).
- **Disk samples** (`run/disk.jsonl`): every 30 seconds, physical APFS file data under the run directory (`e2e/apfs-file-data.py`) and `df` free space.
- **Worktree baseline**: for every graft created, the bytes `git worktree add` plus `npm ci` would have written, from the same repository at the same commit. This is measured once per wave on a scratch copy, then multiplied by the number of grafts.
- **Change ledger** (`run/ledger.jsonl`): after every commit, the agent records the commit ID, the paths it changed and the SHA-256 of each changed file. Before each `graft_merge` or `graft_remove`, it also records any uncommitted or untracked files it means to keep.
- **Merge log**: each merge's outcome (fast-forward, merged, conflict, retry, set aside), the conflicting paths and how the conflict was resolved.

## Data-loss checks (Auditor)

A run passes only if every check holds.

1. Every ledger commit is reachable from `main`, or from an archived graft that `git graft list` shows as recoverable.
2. For every ledger file hash, the same bytes exist at that path in the commit the ledger names.
3. Every uncommitted or untracked file an agent recorded before a remove still exists in that graft's archive, byte for byte.
4. The primary checkout's files recorded at the start, plus every edit the Interference phase made, are intact.
5. Every crash in the Crash phase was recovered by a retry, with no manual repair.
6. No refused operation (wrong lease, unmerged work) changed any file.

## Final report

The Auditor writes `run/REPORT.md` and renders its charts with the existing Excalidraw pipeline (`scripts/charts/`):

- **Totals:** agents, tickets, grafts created, merged, removed, restored, and peak active at once.
- **Disk:** peak and final disk used by grafts against the worktree baseline (chart), and disk returned by cleanup.
- **Speed:** median and p95 time for `graft_add`, the first `git status` and `graft_merge`.
- **Merging:** merges, conflicts, retries, set-aside updates and median time to resolve a conflict.
- **Safety:** each data-loss check with pass or fail and its evidence, and every error code seen and why.
- **Cleanup:** grafts still active at the end and why, archives kept, and `prune` results.
- **Issues found:** anything surprising, with the event log lines that show it, written as git-graft issues ready to file.

## Pass criteria

| Measure | Target |
| --- | --- |
| Lost changes | 0 |
| Crashes needing manual repair | 0 |
| Disk used by grafts, against the worktree baseline | at most 5% |
| Median `graft_add` time | no slower than `git worktree add` on the same repository |
| Grafts left active after cleanup | 0, except any the run deliberately locked |
| Refused operations that changed files | 0 |

## Running it

Prerequisites: macOS on APFS (the disk figures need a cloning filesystem), Git 2.46 or newer, Node 24, the release build of git-graft on `PATH`, and an agent runner that can start 14 agents with an MCP server each. Forge's subagents work, and so does any harness that runs Claude Code, Codex or Cursor agents.

```sh
bench/agents/run.sh --dir ~/graft-swarm-run --agents 12   # creates run/ and tasklane/, starts the proxy, samplers and agents
bench/agents/audit.sh --dir ~/graft-swarm-run             # rerun the audit on a finished run
```

Both scripts are still to be written from this plan, as the first step of building the benchmark.

**Cost:** each build agent needs roughly 0.5–2 million tokens over the run, depending on the model. Use a cheaper model for the build agents and a stronger one for the Coordinator and the Auditor. Set a token budget per agent in the runner, and stop the run when the budget is spent rather than buying more usage.

## Safety

- The run lives entirely in its own directory and never touches other repositories.
- The Chaos agent kills only the agent processes it started.
- Leases appear only as hashes in logs and the report.
