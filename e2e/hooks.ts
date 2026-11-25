import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { basename, join } from 'node:path';
import { objectResult, parseExact, type Scenario } from './harness.ts';
import { call, initialize, withClient } from './mcp.ts';
import { graft as start, waitFor } from './process-faults.ts';

type Context = Parameters<Scenario['run']>[0];
type Entry = Record<string, unknown>;

/** What the fixture hook does, chosen by its first argument:
 * `prepare` writes an ignored file into the graft, `clean` removes it, and
 * `fail` exits with status 3 after writing to both streams. Every run appends
 * its working directory and whole environment to the log named by the second
 * argument. `clean` also fails in a graft named `refused`. */
const fixture = `
const fs = require('node:fs');
const path = require('node:path');
const [action, log] = process.argv.slice(2);
fs.appendFileSync(log, JSON.stringify({ action, cwd: process.cwd(), env: process.env }) + '\\n');
if (action === 'fail' || (action === 'clean' && process.env.GIT_GRAFT_NAME === 'refused')) {
  process.stdout.write('partial output');
  process.stderr.write('cannot ' + process.env.GIT_GRAFT_HOOK);
  process.exit(3);
}
const made = path.join('ignored', 'prepared.txt');
if (action === 'prepare') {
  fs.mkdirSync('ignored', { recursive: true });
  fs.writeFileSync(made, process.env.GIT_GRAFT_NAME + ' on port ' + process.env.GIT_GRAFT_PORT + '\\n');
}
if (action === 'clean') fs.rmSync('ignored', { recursive: true, force: true });
process.stdout.write(action + ' ' + process.env.GIT_GRAFT_NAME);
`;

/** A hook that waits: the first run writes its graft's name to the file named
 * by its first argument, then waits for the file named by its second. Later
 * runs return at once. */
const waiting = `
const fs = require('node:fs');
const [started, release] = process.argv.slice(2);
if (!fs.existsSync(started)) {
  fs.writeFileSync(started, process.env.GIT_GRAFT_NAME);
  const end = Date.now() + 60000;
  const timer = setInterval(() => {
    if (fs.existsSync(release)) { clearInterval(timer); process.exit(0); }
    if (Date.now() > end) { clearInterval(timer); process.exit(2); }
  }, 25);
}
`;

const SHARED = '.graftconfig';
const LOCAL = '.graftconfig.local';

/** The hooks each test has written to the primary's `.graftconfig`, by name. */
const configured = new WeakMap<Context, Record<string, unknown>>();

/** Write the primary's `.graftconfig` with `hooks`, in the order given. */
function writeConfig(ctx: Context, file: string, hooks: Record<string, unknown>): void {
  writeFileSync(join(ctx.repo, file), JSON.stringify({ hooks }, null, 2));
}

/** Define the hook `name` in the primary's `.graftconfig`, replacing one of
 * that name, or remove it when `hook` is undefined. */
function define(ctx: Context, name: string, hook?: { event: string; argv: string[] }): void {
  const hooks = configured.get(ctx) ?? {};
  if (hook) hooks[name] = hook;
  else delete hooks[name];
  configured.set(ctx, hooks);
  writeConfig(ctx, SHARED, hooks);
}

/** Configure the waiting hook for `pre-archive`, and return its two files. */
function registerWaiting(ctx: Context): { started: string; release: string } {
  const script = join(ctx.root, 'waiting hook.cjs');
  writeFileSync(script, waiting);
  const files = { started: join(ctx.root, 'hook-started'), release: join(ctx.root, 'hook-release') };
  define(ctx, 'pre-archive', { event: 'pre-archive', argv: [process.execPath, script, files.started, files.release] });
  return files;
}

/** The fixture hook's program and arguments for `action`, logging to `log`. */
function fixtureArgv(ctx: Context, action: 'prepare' | 'clean' | 'fail', log: string): string[] {
  const script = join(ctx.root, 'hook fixture.cjs');
  if (!existsSync(script)) writeFileSync(script, fixture);
  return [process.execPath, script, action, log];
}

/** Configure the fixture hook for `event`, named after the event, and return
 * the path of its log. */
function register(ctx: Context, event: string, action: 'prepare' | 'clean' | 'fail'): string {
  const log = join(ctx.root, `${event}.log`);
  define(ctx, event, { event, argv: fixtureArgv(ctx, action, log) });
  return log;
}

/** Every run the fixture hook logged, oldest first. */
function runs(log: string): { action: string; cwd: string; env: Record<string, string> }[] {
  if (!existsSync(log)) return [];
  return readFileSync(log, 'utf8').trim().split('\n').map((line) => JSON.parse(line));
}

function entries(result: Record<string, unknown>): Entry[] {
  assert(Array.isArray(result.hooks), `expected a hooks array: ${JSON.stringify(result)}`);
  return result.hooks.map(objectResult);
}

function ran(event: string, stdout: string, name = event, file = SHARED): Entry {
  return { name, event, file, status: 'ok', exit_code: 0, stdout, stderr: '', stdout_truncated: false,
    stderr_truncated: false };
}

function failed(event: string, name = event, file = SHARED): Entry {
  return { name, event, file, status: 'failed', exit_code: 3, stdout: 'partial output', stderr: `cannot ${event}`,
    stdout_truncated: false, stderr_truncated: false };
}

function commit(ctx: Context, cwd: string, message: string): void {
  ctx.git(['add', '-A'], cwd);
  ctx.git(['commit', '-qm', message], cwd);
}

/** Every ref, the index and every file status of a checkout. */
function work(ctx: Context, path: string): string {
  return [
    ctx.git(['for-each-ref', '--format=%(refname) %(objectname)'], path),
    ctx.git(['ls-files', '--stage'], path),
    ctx.git(['status', '--porcelain=v1', '--ignored=matching'], path),
  ].join('\n--\n');
}

/** Expire a graft's lease so prune considers it, as 24 idle hours would. */
function expire(ctx: Context, id: unknown): void {
  assert(typeof id === 'string');
  const file = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${id}.json`);
  // Records hold 64-bit file IDs, which plain JSON.parse would round on NTFS.
  writeFileSync(file, JSON.stringify({ ...objectResult(parseExact(readFileSync(file, 'utf8'))), heartbeat: 0 }));
}

function retained(result: Record<string, unknown>): Map<unknown, Entry> {
  assert(Array.isArray(result.retained));
  return new Map(result.retained.map(objectResult).map((item): [unknown, Entry] => [item.name, item]));
}

export const cases: Scenario[] = [
  {
    name: 'hook list shows the configured hooks in run order, each with its file, and creates no Graft state',
    run(ctx) {
      const state = join(ctx.repo, '.git', 'graft');
      assert.deepEqual(ctx.cli(['hook', 'list']), { hooks: [] });
      writeConfig(ctx, SHARED, {
        setup: { event: 'post-add', argv: ['/usr/local/bin/setup', 'two words'] },
        teardown: { event: 'pre-archive', argv: ['teardown'] },
        assets: { event: 'post-add', argv: ['assets'] },
        reopen: { event: 'post-restore', argv: ['reopen'] },
      });
      writeConfig(ctx, LOCAL, {
        notify: { event: 'post-add', argv: ['notify'] },
        assets: null,
        setup: { event: 'post-add', argv: ['/usr/local/bin/setup', '--small'] },
        'not-shared': null,
      });
      // A replaced hook keeps its place; a new local one runs after the shared ones.
      assert.deepEqual(ctx.cli(['hook', 'list']), { hooks: [
        { name: 'setup', event: 'post-add', argv: ['/usr/local/bin/setup', '--small'], file: LOCAL },
        { name: 'teardown', event: 'pre-archive', argv: ['teardown'], file: SHARED },
        { name: 'reopen', event: 'post-restore', argv: ['reopen'], file: SHARED },
        { name: 'notify', event: 'post-add', argv: ['notify'], file: LOCAL },
      ] });
      assert(!existsSync(state), 'listing hooks created Graft state');
      // Hooks are defined only in the files; there is no command to register one.
      for (const args of [['hook', 'set', 'post-add', '--', 'program'], ['hook', 'remove', 'post-add']]) {
        assert.notEqual(ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', ...args]).status, 0, args.join(' '));
      }
      rmSync(join(ctx.repo, LOCAL));
      rmSync(join(ctx.repo, SHARED));
    },
  },
  {
    name: 'a malformed configuration refuses list, add and restore with the file named, and changes nothing',
    run(ctx) {
      const kept = ctx.spawn('kept');
      const archived = ctx.cli(['remove', 'kept', '--lease', kept.lease]);
      const before = ctx.cli(['list']);
      for (const [file, body, cause] of [
        [SHARED, '{"hooks": {', /EOF/],
        [SHARED, '[]', /JSON object/],
        [SHARED, '{"includes": [".env"]}', /unknown field/],
        [LOCAL, '{"hooks": {"a": {"event": "post-merge", "argv": ["x"]}}}', /unknown variant/],
        [LOCAL, '{"hooks": {"a": {"event": "post-add", "argv": []}}}', /program/],
        [LOCAL, '{"hooks": {"a": {"event": "post-add", "argv": ["x"], "shell": true}}}', /unknown field/],
        [SHARED, '{"hooks": {"a": null}}', /belongs in \.graftconfig\.local/],
        ...['./scripts/setup', 'scripts/setup', '../setup'].map((program) =>
          [LOCAL, JSON.stringify({ hooks: { a: { event: 'post-add', argv: [program] } } }), /is a relative path/]),
      ] as [string, string, RegExp][]) {
        rmSync(join(ctx.repo, SHARED), { force: true });
        rmSync(join(ctx.repo, LOCAL), { force: true });
        writeFileSync(join(ctx.repo, file), body);
        for (const args of [['hook', 'list'], ['add', 'refused'], ['restore', 'kept', '--lease', String(archived.recovery_lease)]]) {
          const error = ctx.fail(args);
          assert.equal(error.code, 'invalid_config', `${args[0]}: ${body}`);
          assert.equal(basename(String(objectResult(error.details).file)), file, body);
          assert(String(error.message).includes(String(objectResult(error.details).file)), body);
          assert.match(String(error.message), cause, body);
        }
      }
      assert.deepEqual(ctx.cli(['list']), before);
      rmSync(join(ctx.repo, SHARED), { force: true });
      rmSync(join(ctx.repo, LOCAL), { force: true });
    },
  },
  {
    name: 'results carry no hooks field while no hook is registered',
    run(ctx) {
      const added = ctx.spawn('plain');
      assert(!('hooks' in added.result));
      writeFileSync(join(added.path, 'tracked.txt'), 'changed\n');
      commit(ctx, added.path, 'change');
      const merged = ctx.cli(['merge', 'plain', '--lease', added.lease]);
      assert(!('hooks' in merged));
      const restored = ctx.cli(['restore', 'plain', '--lease', added.lease]);
      assert(!('hooks' in restored));
      const removed = ctx.cli(['remove', 'plain', '--lease', String(restored.lease)]);
      assert.deepEqual(removed, { action: 'remove', name: 'plain', removed: true, recovery_lease: restored.lease });
      const idle = ctx.spawn('idle');
      expire(ctx, idle.result.id);
      const pruned = retained(ctx.cli(['prune', '--apply'])).get('idle');
      assert.equal(pruned?.reason, 'archived');
      assert(pruned && !('hooks' in pruned));
      // A hook for another event changes nothing either.
      define(ctx, 'never', { event: 'post-restore', argv: ['never-started'] });
      assert(!('hooks' in ctx.spawn('other-event').result));
    },
  },
  {
    name: 'a post-add hook prepares the new graft and sees its environment without the lease',
    run(ctx) {
      mkdirSync(join(ctx.repo, 'docs'));
      writeFileSync(join(ctx.repo, 'docs', 'guide.txt'), 'guide\n');
      commit(ctx, ctx.repo, 'docs');
      const log = register(ctx, 'post-add', 'prepare');
      const lease = randomUUID();
      // The caller's own lease variable must not reach the hook either.
      ctx.env.GIT_GRAFT_LEASE = 'lease-of-the-caller';
      const added = ctx.cli(['add', 'prepared', '--request-id', lease]);
      assert.equal(added.lease, lease);
      assert.deepEqual(entries(added), [ran('post-add', 'prepare prepared')]);
      const path = String(added.path);
      assert.equal(readFileSync(join(path, 'ignored', 'prepared.txt'), 'utf8'), `prepared on port ${added.port}\n`);
      const [run, ...more] = runs(log);
      assert.equal(more.length, 0);
      assert.equal(realpathSync(run.cwd), realpathSync(path));
      const described = Object.fromEntries(Object.entries(run.env).filter(([key]) => key.startsWith('GIT_GRAFT_')));
      assert.deepEqual(described, {
        GIT_GRAFT_HOOK: 'post-add',
        GIT_GRAFT_HOOK_NAME: 'post-add',
        GIT_GRAFT_NAME: 'prepared',
        GIT_GRAFT_PATH: path,
        GIT_GRAFT_PRIMARY: run.env.GIT_GRAFT_PRIMARY,
        GIT_GRAFT_BRANCH: 'graft/prepared',
        GIT_GRAFT_BASE: ctx.git(['rev-parse', 'HEAD']),
        GIT_GRAFT_PORT: String(added.port),
      });
      assert.equal(realpathSync(run.env.GIT_GRAFT_PRIMARY), realpathSync(ctx.repo));
      assert(!JSON.stringify(run.env).includes(lease), 'the hook saw the lease');
      assert(!JSON.stringify(run.env).includes('lease-of-the-caller'), 'the hook saw the caller lease variable');

      // The graft is active once the hook has run, and its lease works.
      assert.equal(ctx.cli(['status', 'prepared']).phase, 'ready');
      ctx.cli(['heartbeat', 'prepared', '--lease', lease]);
      // Retrying the request returns the same graft and does not run the hook again.
      const retried = ctx.cli(['add', 'prepared', '--request-id', lease]);
      assert(!('hooks' in retried));
      assert.equal(retried.path, added.path);
      assert.equal(runs(log).length, 1);

      // Detached, unpopulated and locked grafts run the hook like any other.
      const detached = ctx.cli(['add', 'detached', '--detach', '--lock', '--reason', 'kept']);
      assert.deepEqual(entries(detached), [ran('post-add', 'prepare detached')]);
      const empty = ctx.cli(['add', 'empty', '--orphan', 'fresh']);
      assert.deepEqual(entries(empty), [ran('post-add', 'prepare empty')]);
      const bare = ctx.cli(['add', 'unpopulated', '--no-checkout']);
      assert.deepEqual(entries(bare), [ran('post-add', 'prepare unpopulated')]);
      const sparse = ctx.cli(['add', 'sparse', '--sparse', 'docs']);
      assert.deepEqual(entries(sparse), [ran('post-add', 'prepare sparse')]);
      const later = runs(log).slice(1);
      assert.deepEqual(later.map((item) => item.env.GIT_GRAFT_BRANCH), ['', 'fresh', 'graft/unpopulated', 'graft/sparse']);
      assert.equal(later[1].env.GIT_GRAFT_BASE, '');
      assert.equal(realpathSync(later[2].cwd), realpathSync(String(bare.path)));
      delete ctx.env.GIT_GRAFT_LEASE;
    },
  },
  {
    name: 'a failing post-add hook is reported and the graft stays active and usable',
    run(ctx) {
      const log = register(ctx, 'post-add', 'fail');
      const added = ctx.spawn('unprepared');
      assert.deepEqual(entries(added.result), [failed('post-add')]);
      assert.equal(runs(log).length, 1);
      assert.equal(ctx.cli(['status', 'unprepared']).phase, 'ready');
      assert.equal(readFileSync(join(added.path, 'tracked.txt'), 'utf8'), 'initial\n');
      const executed = ctx.cli(['exec', 'unprepared', '--lease', added.lease, '--', process.execPath, '-e',
        'process.stdout.write("usable")']);
      assert.equal(executed.stdout, 'usable');
      writeFileSync(join(added.path, 'tracked.txt'), 'worked\n');
      commit(ctx, added.path, 'work');
      assert.equal(ctx.cli(['merge', 'unprepared', '--lease', added.lease]).removed, true);

      // A hook that cannot be started is a failed hook too, not a failed add.
      const missing = join(ctx.root, 'no such program');
      define(ctx, 'post-add', { event: 'post-add', argv: [missing] });
      const second = ctx.spawn('unstarted');
      const [entry] = entries(second.result);
      assert.equal(entry.status, 'failed');
      assert.equal(entry.exit_code, null);
      assert.match(String(entry.reason), /could not be started/);
      assert.equal(ctx.cli(['status', 'unstarted']).phase, 'ready');
      ctx.cli(['remove', 'unstarted', '--lease', second.lease]);
    },
  },
  {
    name: 'a pre-archive hook removes what post-add made so remove and merge proceed',
    run(ctx) {
      register(ctx, 'post-add', 'prepare');
      const kept = ctx.spawn('kept');
      // Without a pre-archive hook, the file the post-add hook made stops a plain remove.
      const refused = ctx.fail(['remove', 'kept', '--lease', kept.lease]);
      assert.equal(refused.code, 'local_changes');
      assert(existsSync(join(kept.path, 'ignored', 'prepared.txt')));

      const log = register(ctx, 'pre-archive', 'clean');
      const removed = ctx.cli(['remove', 'kept', '--lease', kept.lease]);
      assert.deepEqual(removed, { action: 'remove', name: 'kept', removed: true, recovery_lease: kept.lease,
        hooks: [ran('pre-archive', 'clean kept')] });
      assert(!existsSync(kept.path));
      assert.equal(runs(log)[0].env.GIT_GRAFT_HOOK, 'pre-archive');
      assert.equal(runs(log)[0].env.GIT_GRAFT_PATH, kept.path);
      // Repeating the remove replays its receipt without running the hook again.
      const { hooks: _, ...receipt } = removed;
      assert.deepEqual(ctx.cli(['remove', 'kept', '--lease', kept.lease]), receipt);
      assert.equal(runs(log).length, 1);

      const merged = ctx.spawn('merged');
      writeFileSync(join(merged.path, 'feature.txt'), 'feature\n');
      commit(ctx, merged.path, 'feature');
      const landed = ctx.cli(['merge', 'merged', '--lease', merged.lease]);
      assert.equal(landed.removed, true);
      assert.deepEqual(entries(landed), [ran('pre-archive', 'clean merged')]);
      assert.equal(readFileSync(join(ctx.repo, 'feature.txt'), 'utf8'), 'feature\n');
      assert(!('hooks' in ctx.cli(['merge', 'merged', '--lease', merged.lease])));
      assert.equal(runs(log).length, 2);

      // A plain remove refuses unmerged commits, a change to a tracked file
      // or a pending operation before the hook runs, so the setup stays.
      const unmerged = ctx.spawn('unmerged');
      writeFileSync(join(unmerged.path, 'later.txt'), 'later\n');
      commit(ctx, unmerged.path, 'later');
      const unmergedWork = work(ctx, unmerged.path);
      assert.equal(ctx.fail(['remove', 'unmerged', '--lease', unmerged.lease]).code, 'unmerged_work');
      writeFileSync(join(unmerged.path, 'tracked.txt'), 'edited\n');
      assert.equal(ctx.fail(['remove', 'unmerged', '--lease', unmerged.lease]).code, 'local_changes');
      ctx.git(['checkout', '--', 'tracked.txt'], unmerged.path);
      assert.equal(work(ctx, unmerged.path), unmergedWork);
      assert(existsSync(join(unmerged.path, 'ignored', 'prepared.txt')));
      const edited = ctx.spawn('edited');
      writeFileSync(join(edited.path, 'tracked.txt'), 'edited\n');
      ctx.git(['add', 'tracked.txt'], edited.path);
      const refusal = ctx.fail(['remove', 'edited', '--lease', edited.lease]);
      assert.equal(refusal.code, 'local_changes');
      // The refusal names every local change, the setup's ignored file too.
      assert.deepEqual(objectResult(refusal.details).paths,
        [{ path: 'tracked.txt', status: 'modified' }, { path: 'ignored/', status: 'ignored' }]);
      assert(existsSync(join(edited.path, 'ignored', 'prepared.txt')));
      assert.equal(runs(log).length, 2);
      // --discard archives the work, so the hook runs for it.
      const discarded = ctx.cli(['remove', 'unmerged', '--lease', unmerged.lease, '--discard']);
      assert.deepEqual(entries(discarded), [ran('pre-archive', 'clean unmerged')]);
      assert.equal(runs(log).length, 3);
      // A command that refuses after the hook ran runs it again on the retry.
      ctx.git(['reset', '-q', '--hard'], edited.path);
      writeFileSync(join(edited.path, 'notes.txt'), 'notes\n');
      assert.equal(ctx.fail(['remove', 'edited', '--lease', edited.lease]).code, 'local_changes');
      assert(!existsSync(join(edited.path, 'ignored')));
      assert.equal(runs(log).length, 4);
      rmSync(join(edited.path, 'notes.txt'));
      assert.deepEqual(entries(ctx.cli(['remove', 'edited', '--lease', edited.lease])), [ran('pre-archive', 'clean edited')]);
      assert.equal(runs(log).length, 5);

      // A locked graft is refused before its hook runs.
      const locked = ctx.spawn('locked', ['--lock']);
      assert.equal(ctx.fail(['remove', 'locked', '--lease', locked.lease, '--discard']).code, 'workspace_locked');
      assert.equal(ctx.fail(['merge', 'locked', '--lease', locked.lease]).code, 'workspace_locked');
      assert.equal(runs(log).length, 5);
      assert(existsSync(join(locked.path, 'ignored', 'prepared.txt')));
    },
  },
  {
    name: 'a failing pre-archive hook refuses remove and merge and changes nothing',
    run(ctx) {
      const graft = ctx.spawn('guarded');
      writeFileSync(join(graft.path, 'feature.txt'), 'feature\n');
      commit(ctx, graft.path, 'feature');
      writeFileSync(join(graft.path, 'notes.txt'), 'untracked notes\n');
      const log = register(ctx, 'pre-archive', 'fail');
      const before = { graft: work(ctx, graft.path), primary: work(ctx, ctx.repo), head: ctx.git(['rev-parse', 'HEAD']),
        list: ctx.cli(['list']) };
      const unchanged = () => {
        assert.equal(work(ctx, graft.path), before.graft);
        assert.equal(work(ctx, ctx.repo), before.primary);
        assert.equal(ctx.git(['rev-parse', 'HEAD']), before.head);
        assert.equal(readFileSync(join(graft.path, 'notes.txt'), 'utf8'), 'untracked notes\n');
        assert(!existsSync(join(ctx.repo, 'feature.txt')));
        const status = ctx.cli(['status', 'guarded']);
        assert.equal(status.phase, 'ready');
        assert.equal(status.path, graft.path);
      };
      // A plain remove refuses the unmerged commit before the hook runs.
      assert.equal(ctx.fail(['remove', 'guarded', '--lease', graft.lease]).code, 'unmerged_work');
      assert.equal(runs(log).length, 0);
      unchanged();
      const commands = [
        ['remove', 'guarded', '--lease', graft.lease, '--discard'],
        ['merge', 'guarded', '--lease', graft.lease],
        ['merge', 'guarded', '--lease', graft.lease, '--commit', 'Add notes', '--squash'],
      ];
      for (const [index, args] of commands.entries()) {
        const error = ctx.fail(args);
        assert.equal(error.code, 'hook_failed', args.join(' '));
        assert.match(String(error.message), /exited with code 3.*not archived/);
        const details = objectResult(error.details);
        assert.equal(details.name, 'guarded');
        assert.deepEqual(entries(details), [failed('pre-archive')]);
        assert(!JSON.stringify(error).includes(graft.lease), 'the refusal disclosed the lease');
        assert.equal(runs(log).length, index + 1);
        unchanged();
      }
      assert.deepEqual(ctx.cli(['list']).workspaces, before.list.workspaces);

      // A hook that cannot be started refuses in the same way.
      define(ctx, 'pre-archive', { event: 'pre-archive', argv: [join(ctx.root, 'no such program')] });
      const unstarted = ctx.fail(['remove', 'guarded', '--lease', graft.lease, '--discard']);
      assert.equal(unstarted.code, 'hook_failed');
      assert.match(String(unstarted.message), /could not be started/);
      unchanged();

      // With the hook turned off in the local file the same commands work again.
      writeConfig(ctx, LOCAL, { 'pre-archive': null });
      const merged = ctx.cli(['merge', 'guarded', '--lease', graft.lease, '--commit', 'Add notes']);
      assert.equal(merged.removed, true);
      assert(!('hooks' in merged));
    },
  },
  {
    name: 'prune --apply keeps a graft whose pre-archive hook fails and archives the others',
    run(ctx) {
      register(ctx, 'post-add', 'prepare');
      const prepared = ctx.spawn('prepared');
      const second = ctx.spawn('second');
      const refused = ctx.spawn('refused');
      const worked = ctx.spawn('worked');
      const mixed = ctx.spawn('mixed');
      const edited = ctx.spawn('edited');
      define(ctx, 'post-add');
      const clean = ctx.spawn('clean');
      const live = ctx.spawn('live');
      rmSync(join(worked.path, 'ignored'), { recursive: true });
      writeFileSync(join(worked.path, 'feature.txt'), 'unmerged\n');
      commit(ctx, worked.path, 'unmerged work');
      // Commits with an uncommitted edit, and an edit alone: both report
      // dirty, and neither is the hook's to clear.
      writeFileSync(join(mixed.path, 'feature.txt'), 'unmerged\n');
      commit(ctx, mixed.path, 'unmerged work');
      for (const graft of [mixed, edited]) writeFileSync(join(graft.path, 'tracked.txt'), 'edited\n');
      const log = register(ctx, 'pre-archive', 'clean');
      for (const graft of [prepared, second, refused, worked, clean, mixed, edited]) expire(ctx, graft.result.id);
      const keptWork = work(ctx, refused.path);
      const editedWork = [mixed, edited].map((graft) => work(ctx, graft.path));

      // A preview runs no hook, so it still reports what the hook would remove.
      const preview = retained(ctx.cli(['prune']));
      assert.deepEqual(Object.fromEntries([...preview].map(([name, item]) => [name, item.reason])),
        { prepared: 'dirty', second: 'dirty', refused: 'dirty', worked: 'changed', clean: 'eligible', mixed: 'dirty',
          edited: 'dirty' });
      assert.equal(runs(log).length, 0);
      assert([...preview.values()].every((item) => !('hooks' in item)));

      const result = ctx.cli(['prune', '--apply']);
      const applied = retained(result);
      assert.deepEqual(Object.fromEntries([...applied].map(([name, item]) => [name, item.reason])),
        { prepared: 'archived', second: 'archived', refused: 'hook_failed', worked: 'changed', clean: 'archived',
          mixed: 'dirty', edited: 'dirty' });
      assert.deepEqual(entries(applied.get('prepared')!), [ran('pre-archive', 'clean prepared')]);
      assert.deepEqual(entries(applied.get('clean')!), [ran('pre-archive', 'clean clean')]);
      assert.deepEqual(entries(applied.get('refused')!), [failed('pre-archive')]);
      // A graft kept for its unmerged commits or a tracked change is left
      // alone, hook unrun, with the file its setup made still there.
      for (const name of ['worked', 'mixed', 'edited']) assert(!('hooks' in applied.get(name)!), name);
      assert.deepEqual([mixed, edited].map((graft) => work(ctx, graft.path)), editedWork);
      for (const graft of [mixed, edited]) assert(existsSync(join(graft.path, 'ignored', 'prepared.txt')));
      assert.deepEqual(runs(log).map((item) => item.env.GIT_GRAFT_NAME).sort(), ['clean', 'prepared', 'refused', 'second']);
      for (const graft of [prepared, second, refused, worked, clean, mixed, edited]) {
        assert(!JSON.stringify(result).includes(graft.lease), 'prune disclosed a lease');
      }
      for (const graft of [prepared, second, clean]) assert(!existsSync(graft.path));
      assert.equal(work(ctx, refused.path), keptWork);
      assert.equal(readFileSync(join(refused.path, 'ignored', 'prepared.txt'), 'utf8'), `refused on port ${refused.result.port}\n`);
      assert.equal(ctx.cli(['status', 'refused']).phase, 'ready');
      assert.equal(ctx.cli(['status', 'worked']).phase, 'ready');
      ctx.cli(['heartbeat', 'live', '--lease', live.lease]);
      ctx.cli(['restore', 'prepared', '--lease', prepared.lease]);
    },
  },
  {
    name: 'a running hook keeps its own graft busy and leaves the rest of the project free',
    async run(ctx) {
      const slow = ctx.spawn('slow');
      const other = ctx.spawn('other');
      const { started, release } = registerWaiting(ctx);
      const removing = start(ctx, ['remove', 'slow', '--lease', slow.lease]);
      try {
        await waitFor(() => existsSync(started), 'the pre-archive hook to start');
        // Other grafts are added, used and listed while the hook runs.
        const added = ctx.spawn('during');
        ctx.cli(['heartbeat', 'other', '--lease', other.lease]);
        ctx.cli(['exec', 'during', '--lease', added.lease, '--', process.execPath, '-e', '']);
        assert.equal(ctx.cli(['status', 'slow']).phase, 'ready');
        assert(Array.isArray(ctx.cli(['list']).workspaces));
        ctx.cli(['hook', 'list']);
        // The graft itself is held for the hook.
        for (const args of [['exec', 'slow', '--lease', slow.lease, '--', process.execPath, '-e', ''],
          ['heartbeat', 'slow', '--lease', slow.lease], ['lock', 'slow', '--lease', slow.lease],
          ['remove', 'slow', '--lease', slow.lease, '--discard']]) {
          assert.equal(ctx.fail(args).code, 'workspace_busy', args[0]);
        }
        assert(existsSync(slow.path));
      } finally {
        writeFileSync(release, 'release');
      }
      const removed = await removing;
      assert.equal(removed.ok, true, removed.stderr);
      assert.equal(entries(removed.json)[0].status, 'ok');
      assert(!existsSync(slow.path));
    },
  },
  {
    name: 'prune judges the remaining grafts again after a hook, so one renewed meanwhile is kept',
    async run(ctx) {
      const grafts = new Map([ctx.spawn('first'), ctx.spawn('second')]
        .map((item) => [String(item.result.name), item] as const));
      const { started, release } = registerWaiting(ctx);
      for (const item of grafts.values()) expire(ctx, item.result.id);
      const pruning = start(ctx, ['prune', '--apply']);
      let renewed = '';
      try {
        await waitFor(() => existsSync(started), 'the pre-archive hook to start');
        // Prune is waiting for the hook of one graft. The owner of the other
        // renews its lease meanwhile, so it is no longer expired.
        const hooked = readFileSync(started, 'utf8');
        renewed = hooked === 'first' ? 'second' : 'first';
        ctx.cli(['heartbeat', renewed, '--lease', grafts.get(renewed)!.lease]);
      } finally {
        writeFileSync(release, 'release');
      }
      const pruned = await pruning;
      assert.equal(pruned.ok, true, pruned.stderr);
      const reasons = [...retained(pruned.json)].map(([name, item]) => [name, item.reason]);
      // The hook that waited was the first to run, so prune reached the
      // renewed graft after it, found the lease renewed and left it unlisted.
      assert.deepEqual(reasons, [[renewed === 'first' ? 'second' : 'first', 'archived']]);
      assert(existsSync(grafts.get(renewed)!.path), 'prune archived a graft renewed while a hook ran');
      assert.equal(ctx.cli(['status', renewed]).phase, 'ready');
      ctx.cli(['heartbeat', renewed, '--lease', grafts.get(renewed)!.lease]);
    },
  },
  {
    name: 'a post-restore hook runs once after restore reopens a graft',
    run(ctx) {
      const graft = ctx.spawn('reopened');
      const removed = ctx.cli(['remove', 'reopened', '--lease', graft.lease]);
      const log = register(ctx, 'post-restore', 'prepare');
      const restored = ctx.cli(['restore', 'reopened', '--lease', String(removed.recovery_lease)]);
      assert.deepEqual(entries(restored), [ran('post-restore', 'prepare reopened')]);
      assert.notEqual(restored.lease, graft.lease);
      const [run] = runs(log);
      assert.equal(realpathSync(run.cwd), realpathSync(graft.path));
      assert.equal(run.env.GIT_GRAFT_HOOK, 'post-restore');
      assert.equal(run.env.GIT_GRAFT_PATH, graft.path);
      assert.equal(run.env.GIT_GRAFT_PORT, String(restored.port));
      assert(!JSON.stringify(run.env).includes(String(restored.lease)), 'the hook saw the new lease');
      assert(!JSON.stringify(run.env).includes(graft.lease), 'the hook saw the recovery lease');
      assert.equal(readFileSync(join(graft.path, 'ignored', 'prepared.txt'), 'utf8'), `reopened on port ${restored.port}\n`);
      // Repeating the restore returns the same graft without running the hook again.
      const { hooks: _, ...reply } = restored;
      assert.deepEqual(ctx.cli(['restore', 'reopened', '--lease', String(removed.recovery_lease)]), reply);
      assert.equal(runs(log).length, 1);
      ctx.cli(['heartbeat', 'reopened', '--lease', String(restored.lease)]);

      // A failing post-restore hook is reported, and the graft is restored all the same.
      const second = ctx.spawn('second');
      const archived = ctx.cli(['remove', 'second', '--lease', second.lease]);
      register(ctx, 'post-restore', 'fail');
      const again = ctx.cli(['restore', 'second', '--lease', String(archived.recovery_lease)]);
      assert.deepEqual(entries(again), [failed('post-restore')]);
      assert.equal(ctx.cli(['status', 'second']).phase, 'ready');
      ctx.cli(['heartbeat', 'second', '--lease', String(again.lease)]);
    },
  },
  {
    name: 'hooks for one event run in file order, and once one fails the rest are skipped',
    run(ctx) {
      const log = join(ctx.root, 'order.log');
      writeConfig(ctx, SHARED, {
        first: { event: 'pre-archive', argv: fixtureArgv(ctx, 'clean', log) },
        second: { event: 'pre-archive', argv: fixtureArgv(ctx, 'fail', log) },
        third: { event: 'pre-archive', argv: fixtureArgv(ctx, 'clean', log) },
      });
      const graft = ctx.spawn('ordered');
      const error = ctx.fail(['remove', 'ordered', '--lease', graft.lease]);
      assert.equal(error.code, 'hook_failed');
      assert.match(String(error.message), /pre-archive hook "second" exited with code 3.*not archived/);
      const reported = entries(objectResult(error.details));
      assert.deepEqual(reported.slice(0, 2), [ran('pre-archive', 'clean ordered', 'first'), failed('pre-archive', 'second')]);
      const { reason, ...third } = reported[2];
      assert.deepEqual(third, { name: 'third', event: 'pre-archive', file: SHARED, status: 'skipped', exit_code: null,
        stdout: '', stderr: '', stdout_truncated: false, stderr_truncated: false });
      assert.match(String(reason), /"second" failed first/);
      assert.equal(ctx.cli(['status', 'ordered']).phase, 'ready');

      // The local file turns the failing hook off, and the remove goes on.
      writeConfig(ctx, LOCAL, { second: null });
      const removed = ctx.cli(['remove', 'ordered', '--lease', graft.lease]);
      assert.deepEqual(entries(removed),
        [ran('pre-archive', 'clean ordered', 'first'), ran('pre-archive', 'clean ordered', 'third')]);
      assert.deepEqual(runs(log).map((item) => item.env.GIT_GRAFT_HOOK_NAME), ['first', 'second', 'first', 'third']);
    },
  },
  {
    name: 'a committed .graftconfig runs its hooks for every graft, and only the primary copy counts',
    run(ctx) {
      const log = join(ctx.root, 'committed.log');
      const setup = { event: 'post-add', argv: fixtureArgv(ctx, 'clean', log) };
      writeFileSync(join(ctx.repo, SHARED), JSON.stringify({ hooks: { setup } }));
      commit(ctx, ctx.repo, 'Configure Graft');
      const child = ctx.spawn('child');
      assert.deepEqual(entries(child.result), [ran('post-add', 'clean child', 'setup')]);
      assert(existsSync(join(child.path, SHARED)), 'the graft has its own copy of the committed file');

      // The graft commits a failing pre-archive hook to its copy. Graft reads
      // the primary's, so a graft added from inside it and its own merge run
      // only the primary's hooks.
      const guard = { event: 'pre-archive', argv: fixtureArgv(ctx, 'fail', log) };
      writeFileSync(join(child.path, SHARED), JSON.stringify({ hooks: { setup, guard } }));
      commit(ctx, child.path, 'Guard archives');
      assert.deepEqual(ctx.cli(['hook', 'list'], child.path).hooks, [{ name: 'setup', ...setup, file: SHARED }]);
      const grandchild = ctx.spawn('grandchild', [], child.path);
      assert.deepEqual(entries(grandchild.result), [ran('post-add', 'clean grandchild', 'setup')]);
      ctx.cli(['remove', 'grandchild', '--lease', grandchild.lease]);
      const merged = ctx.cli(['merge', 'child', '--lease', child.lease]);
      assert.equal(merged.removed, true);
      assert(!('hooks' in merged));
      assert.deepEqual(runs(log).map((item) => item.action), ['clean', 'clean']);

      // Once merged into the primary, the new hook runs for later grafts.
      const later = ctx.spawn('later');
      const refused = ctx.fail(['remove', 'later', '--lease', later.lease]);
      assert.equal(refused.code, 'hook_failed');
      assert.deepEqual(entries(objectResult(refused.details)), [failed('pre-archive', 'guard')]);
    },
  },
  {
    name: 'an MCP host runs hooks only with --allow-exec and otherwise reports them skipped',
    async run(ctx) {
      const logs = { add: register(ctx, 'post-add', 'prepare'), archive: register(ctx, 'pre-archive', 'clean'),
        restore: register(ctx, 'post-restore', 'prepare') };
      const count = () => [logs.add, logs.archive, logs.restore].map((log) => runs(log).length);
      const skipped = (event: string) => {
        const entry: Entry = { name: event, event, file: SHARED, status: 'skipped', exit_code: null, stdout: '',
          stderr: '', stdout_truncated: false, stderr_truncated: false };
        return (found: Entry[]) => {
          assert.equal(found.length, 1);
          const { reason, ...rest } = found[0];
          assert.deepEqual(rest, entry);
          assert.match(String(reason), /--allow-exec/);
        };
      };
      await withClient(ctx, false, async (client) => {
        const info = await initialize(client);
        assert.match(String(info.instructions), /Hooks.*not run.*skipped/);
        const added = await call(client, 'graft_add', { name: 'quiet' });
        skipped('post-add')(entries(added));
        const path = String(added.path);
        assert(!existsSync(join(path, 'ignored')));
        const owned = { name: 'quiet', lease: String(added.lease) };
        const removed = await call(client, 'graft_remove', owned);
        skipped('pre-archive')(entries(removed));
        assert.equal(removed.removed, true);
        const restored = await call(client, 'graft_restore', owned);
        skipped('post-restore')(entries(restored));
        assert(!existsSync(join(path, 'ignored')));
        writeFileSync(join(path, 'feature.txt'), 'feature\n');
        commit(ctx, path, 'feature');
        const merged = await call(client, 'graft_merge', { name: 'quiet', lease: String(restored.lease) });
        skipped('pre-archive')(entries(merged));
        assert.equal(merged.removed, true);
        assert.deepEqual(count(), [0, 0, 0]);
        for (const tool of ['graft_add', 'graft_remove', 'graft_restore', 'graft_merge']) ctx.coverTool(tool);
      });
      await withClient(ctx, true, async (client) => {
        const info = await initialize(client);
        assert.match(String(info.instructions), /Hooks.*run when a graft is added/);
        const added = await call(client, 'graft_add', { name: 'allowed' });
        assert.deepEqual(entries(added), [ran('post-add', 'prepare allowed')]);
        const path = String(added.path);
        assert(existsSync(join(path, 'ignored', 'prepared.txt')));
        const owned = { name: 'allowed', lease: String(added.lease) };
        const removed = await call(client, 'graft_remove', owned);
        assert.deepEqual(entries(removed), [ran('pre-archive', 'clean allowed')]);
        const restored = await call(client, 'graft_restore', owned);
        assert.deepEqual(entries(restored), [ran('post-restore', 'prepare allowed')]);
        assert(existsSync(join(path, 'ignored', 'prepared.txt')));
        writeFileSync(join(path, 'allowed.txt'), 'allowed\n');
        commit(ctx, path, 'allowed');
        const merged = await call(client, 'graft_merge', { name: 'allowed', lease: String(restored.lease) });
        assert.deepEqual(entries(merged), [ran('pre-archive', 'clean allowed')]);
        assert.equal(merged.removed, true);
        assert.deepEqual(count(), [1, 2, 1]);

        // A failing pre-archive hook refuses the tool call and keeps the graft.
        const guarded = await call(client, 'graft_add', { name: 'refused' });
        const refusal = await call(client, 'graft_remove', { name: 'refused', lease: String(guarded.lease) }, true);
        assert.equal(refusal.code, 'hook_failed');
        assert(existsSync(String(guarded.path)));
        assert.equal((await call(client, 'graft_status', { name: 'refused' })).phase, 'ready');
      });
    },
  },
];
