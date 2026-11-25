/**
 * Turns on a shared resource: grafts take it in the order they asked, a turn
 * runs out or follows its graft into the archive, a waiting take returns when
 * the holder gives the turn up, and MCP serves the same three operations.
 */
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { execute, objectResult, type Context, type ObjectResult, type Scenario } from './harness.ts';
import { call, discover, initialize, invalidCall, withClient } from './mcp.ts';
import { Gate, age, background, graft, running, waitFor } from './process-faults.ts';

type Graft = { name: string; lease: string; id: string };

function add(ctx: Context, name: string): Graft {
  const { lease, result } = ctx.spawn(name);
  assert(typeof result.id === 'string');
  return { name, lease, id: result.id };
}
function take(ctx: Context, graft: Graft, resource: string, ...options: string[]): ObjectResult {
  return ctx.cli(['turn', 'take', graft.name, resource, '--lease', graft.lease, ...options]);
}
function release(ctx: Context, graft: Graft, resource: string): ObjectResult {
  return ctx.cli(['turn', 'release', graft.name, resource, '--lease', graft.lease]);
}
/** Each listed resource as [resource, holder, queue]. */
function turns(ctx: Context): unknown[][] {
  const listed = ctx.cli(['turn', 'list']).turns;
  assert(Array.isArray(listed));
  return listed.map(objectResult).map(turn => [turn.resource, turn.holder, turn.queue]);
}
function stateFile(ctx: Context, resource: string): string {
  return join(ctx.repo, '.git', 'graft', 'turns', `turn-${resource}.json`);
}
function heartbeat(ctx: Context, graft: Graft): unknown {
  return ctx.cli(['status', graft.name]).last_heartbeat;
}
function number(value: unknown): number {
  assert(typeof value === 'number');
  return value;
}

export const cases: Scenario[] = [
  {
    name: 'turn list leaves fresh and foreign project state untouched',
    run(ctx) {
      const state = join(ctx.repo, '.git', 'graft');
      assert.deepEqual(ctx.cli(['turn', 'list']), { turns: [] });
      assert(!existsSync(state));
      mkdirSync(state);
      const foreign = join(state, 'foreign-note');
      writeFileSync(foreign, 'retain foreign state');
      ctx.fail(['turn', 'list']);
      assert.equal(readFileSync(foreign, 'utf8'), 'retain foreign state');
      assert.deepEqual(readdirSync(state), ['foreign-note']);
    },
  },
  {
    name: 'grafts take a turn in the order they asked and hand it over on release',
    run(ctx) {
      const [a, b, c] = ['task-a', 'task-b', 'task-c'].map(name => add(ctx, name));
      // Nothing is stored until a turn is taken.
      assert.deepEqual(ctx.cli(['turn', 'list']), { turns: [] });
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: false, next: null });
      assert(!existsSync(join(ctx.repo, '.git', 'graft', 'turns')));

      const first = take(ctx, a, 'web');
      const since = number(first.since);
      assert.deepEqual(first, { resource: 'web', granted: true, holder: 'task-a', since, expires_at: since + 1800, queue: [] });
      const held = { resource: 'web', holder: 'task-a', since, expires_at: since + 1800 };
      assert.deepEqual(take(ctx, b, 'web'), { ...held, granted: false, position: 1, queue: ['task-b'] });
      assert.deepEqual(take(ctx, c, 'web'), { ...held, granted: false, position: 2, queue: ['task-b', 'task-c'] });
      // Asking again keeps the place, and another resource has its own turn.
      assert.equal(take(ctx, b, 'web').position, 1);
      const database = take(ctx, b, 'database');
      assert.equal(database.granted, true);
      const listing = ctx.cli(['turn', 'list']);
      assert.deepEqual(listing, { turns: [
        { resource: 'database', holder: 'task-b', since: database.since, expires_at: database.expires_at, queue: [] },
        { ...held, queue: ['task-b', 'task-c'] },
      ] });
      // Names only: no lease anywhere, and no graft ID in what a command prints.
      for (const { lease, id } of [a, b, c]) {
        assert(!JSON.stringify(listing).includes(lease) && !JSON.stringify(listing).includes(id));
        assert(!readFileSync(stateFile(ctx, 'web'), 'utf8').includes(lease));
      }

      // A take or a release renews the lease heartbeat like other commands.
      age(ctx);
      assert.equal(take(ctx, a, 'web').granted, true);
      assert(number(heartbeat(ctx, a)) > 0);
      assert.equal(heartbeat(ctx, c), 0);
      assert.deepEqual(release(ctx, c, 'cache'), { resource: 'cache', released: false, next: null });
      assert(number(heartbeat(ctx, c)) > 0);
      assert.equal(heartbeat(ctx, b), 0);

      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
      // The turn is free, but task-b asked first and gets it when it next asks.
      assert.deepEqual(take(ctx, c, 'web'),
        { resource: 'web', granted: false, holder: null, since: null, expires_at: null, position: 2, queue: ['task-b', 'task-c'] });
      const handed = take(ctx, b, 'web');
      assert.deepEqual([handed.granted, handed.holder, handed.queue], [true, 'task-b', ['task-c']]);
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: false, next: 'task-c' });
      assert.deepEqual(release(ctx, b, 'web'), { resource: 'web', released: true, next: 'task-c' });
      assert.equal(take(ctx, c, 'web').granted, true);
      // A waiter can leave the queue without ever holding the turn.
      assert.equal(take(ctx, a, 'web').position, 1);
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: null });
      assert.deepEqual(release(ctx, c, 'web'), { resource: 'web', released: true, next: null });
      assert.deepEqual(turns(ctx), [['database', 'task-b', []]]);
    },
  },
  {
    name: 'a wrong lease, an invalid resource or an out-of-range option takes no turn and changes nothing',
    run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(name => add(ctx, name));
      assert.equal(take(ctx, a, 'web').granted, true);
      assert.equal(take(ctx, b, 'web').position, 1);
      const stored = readFileSync(stateFile(ctx, 'web'), 'utf8');
      const listed = ctx.cli(['turn', 'list']);
      const beats = [heartbeat(ctx, a), heartbeat(ctx, b)];

      for (const [action, name, lease] of [
        ['take', 'task-a', randomUUID()], ['take', 'task-a', b.lease],
        ['release', 'task-a', b.lease], ['release', 'task-b', randomUUID()],
      ]) {
        const error = ctx.fail(['turn', action, name, 'web', '--lease', lease]);
        assert.equal(error.code, 'lease_mismatch', action);
        assert(!JSON.stringify(error).includes(a.lease) && !JSON.stringify(error).includes(b.lease));
      }
      assert.equal(ctx.fail(['turn', 'take', 'missing', 'web', '--lease', a.lease]).code, 'not_found');
      for (const resource of ['Web', 'web_server', 'web/api', 'web server', 'a'.repeat(65)]) {
        for (const action of ['take', 'release']) {
          assert.equal(ctx.fail(['turn', action, 'task-a', resource, '--lease', a.lease]).code, 'invalid_resource', resource);
        }
      }
      // The command line refuses these before anything runs.
      for (const options of [['--wait', '3601'], ['--ttl', '0'], ['--ttl', '86401'], ['--wait', '-1']]) {
        const out = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'turn', 'take', 'task-a', 'web', '--lease', a.lease, ...options]);
        assert.equal(out.status, 2, options.join(' '));
      }
      assert.equal(ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'turn', 'take', 'task-a', 'web']).status, 2);
      assert.equal(readFileSync(stateFile(ctx, 'web'), 'utf8'), stored);
      assert.deepEqual(ctx.cli(['turn', 'list']), listed);
      assert.deepEqual([heartbeat(ctx, a), heartbeat(ctx, b)], beats);

      // The lease can come from the environment, as for other commands.
      const out = execute(ctx.bin, ['--repo', ctx.repo, '--json', 'turn', 'take', 'task-a', 'web'], ctx.repo,
        { ...ctx.env, GIT_GRAFT_LEASE: a.lease });
      assert.equal(out.status, 0, out.stderr);
      assert.equal(objectResult(JSON.parse(out.stdout)).granted, true);
    },
  },
  {
    name: 'a turn runs out after its ttl unless it is taken again',
    async run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(name => add(ctx, name));
      const short = take(ctx, a, 'web', '--ttl', '1');
      assert.equal(short.granted, true);
      assert.equal(short.expires_at, number(short.since) + 1);
      await delay(2_100);
      assert.deepEqual(turns(ctx), []);
      const next = take(ctx, b, 'web', '--ttl', '600');
      assert.deepEqual([next.granted, next.holder, next.expires_at], [true, 'task-b', number(next.since) + 600]);
      // The earlier holder lost the turn and queues like anyone else.
      assert.deepEqual([take(ctx, a, 'web').granted, take(ctx, a, 'web').position], [false, 1]);

      // Taking it again renews it from now and keeps its ttl; a new ttl replaces it.
      await delay(1_100);
      const renewed = take(ctx, b, 'web');
      assert.deepEqual([renewed.granted, renewed.since], [true, next.since]);
      assert(number(renewed.expires_at) > number(next.expires_at));
      const shorter = take(ctx, b, 'web', '--ttl', '5');
      assert.equal(shorter.since, next.since);
      assert(number(shorter.expires_at) < number(renewed.expires_at));
    },
  },
  {
    name: 'an archived holder loses its turn and a restored one keeps a turn nobody took',
    run(ctx) {
      const [a, b, c] = ['task-a', 'task-b', 'task-c'].map(name => add(ctx, name));
      for (const resource of ['web', 'database']) assert.equal(take(ctx, a, resource).granted, true);
      assert.equal(take(ctx, b, 'web').position, 1);
      assert.equal(take(ctx, c, 'cache').granted, true);
      assert.equal(take(ctx, b, 'cache').position, 1);

      // Remove changes no turn state; the holder is forgotten when next looked at.
      const stored = readFileSync(stateFile(ctx, 'web'), 'utf8');
      ctx.cli(['remove', 'task-a', '--lease', a.lease]);
      assert.equal(readFileSync(stateFile(ctx, 'web'), 'utf8'), stored);
      assert.deepEqual(turns(ctx), [['cache', 'task-c', ['task-b']], ['web', null, ['task-b']]]);
      assert.equal(take(ctx, b, 'web').granted, true);
      assert.equal(ctx.fail(['turn', 'take', 'task-a', 'web', '--lease', a.lease]).code, 'graft_archived');

      // Restored within its ttl, the graft still holds the turn nobody took.
      const reopened = ctx.cli(['restore', 'task-a', '--lease', a.lease]);
      assert(typeof reopened.lease === 'string' && reopened.lease !== a.lease);
      const restored = { ...a, lease: reopened.lease };
      assert.deepEqual(turns(ctx), [['cache', 'task-c', ['task-b']], ['database', 'task-a', []], ['web', 'task-b', []]]);
      assert.equal(take(ctx, b, 'database').position, 1);
      assert.equal(take(ctx, restored, 'database').granted, true);
      assert.equal(take(ctx, restored, 'web').position, 1);
      assert.equal(ctx.fail(['turn', 'take', 'task-a', 'web', '--lease', a.lease]).code, 'lease_mismatch');

      // A new graft that reuses an archived holder's name holds nothing.
      ctx.cli(['remove', 'task-c', '--lease', c.lease]);
      const reused = add(ctx, 'task-c');
      assert.notEqual(reused.id, c.id);
      const behind = take(ctx, reused, 'cache');
      assert.deepEqual([behind.granted, behind.holder, behind.queue], [false, null, ['task-b', 'task-c']]);
    },
  },
  {
    name: 'take --wait returns soon after the holder gives up the turn and holds no lock meanwhile',
    async run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(name => add(ctx, name));
      assert.equal(take(ctx, a, 'web').granted, true);
      let started = performance.now();
      const brief = take(ctx, b, 'web', '--wait', '1');
      assert.deepEqual([brief.granted, brief.position], [false, 1]);
      assert(performance.now() - started >= 900, 'a wait that was not granted returned early');

      let settled = false;
      const waiting = graft(ctx, ['turn', 'take', 'task-b', 'web', '--lease', b.lease, '--wait', '20'])
        .finally(() => { settled = true; });
      await delay(1_500);
      assert(!settled, 'the waiting take returned while the turn was held');
      // Other commands, the holder's renewal included, are not kept waiting.
      started = performance.now();
      ctx.cli(['list']);
      assert.equal(take(ctx, a, 'web').granted, true);
      assert.deepEqual(turns(ctx), [['web', 'task-a', ['task-b']]]);
      assert(performance.now() - started < 5_000, 'a waiting take held up other commands');

      started = performance.now();
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
      const outcome = await waiting;
      assert(outcome.ok, JSON.stringify(outcome.json));
      assert.deepEqual([outcome.json.granted, outcome.json.holder, outcome.json.queue], [true, 'task-b', []]);
      assert(performance.now() - started < 10_000, 'the waiting take returned long after the release');
    },
  },
  {
    name: 'a graft takes and gives up turns while a process it started with exec runs',
    async run(ctx) {
      const [a, b] = ['agent', 'task-b'].map(name => add(ctx, name));
      const started = join(ctx.root, 'background-started');
      const stop = join(ctx.root, 'background-release');
      // The usual order: take the turn, then start the shared thing.
      assert.equal(take(ctx, a, 'web').granted, true);
      const exec = ctx.cli(['exec', 'agent', '--lease', a.lease, '--', ...background(ctx, started, stop)]);
      assert.equal(exec.success, true, JSON.stringify(exec));
      let pid = 0;
      try {
        await waitFor(() => existsSync(started) && readFileSync(started, 'utf8').length > 0, 'the background process');
        pid = Number(readFileSync(started, 'utf8'));
        assert(running(pid), 'the background process exited early');
        assert.equal(ctx.fail(['lock', 'agent', '--lease', a.lease]).code, 'workspace_busy');
        assert.equal(take(ctx, a, 'web').granted, true);
        assert.equal(take(ctx, b, 'web').position, 1);
        assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
        assert.equal(take(ctx, a, 'web').position, 2);
      } finally {
        writeFileSync(stop, 'release\n');
        if (pid) await waitFor(() => !running(pid), 'the background process to exit');
      }
    },
  },
  {
    name: 'turn commands do not wait for a graft that is being built',
    async run(ctx) {
      if (process.platform === 'win32') ctx.unavailable('the fault hooks are POSIX shell scripts');
      const [a, b] = ['task-a', 'task-b'].map(name => add(ctx, name));
      // A smudge filter holds the next add part way through its checkout.
      const gate = new Gate(ctx, 'build');
      const filter = join(ctx.root, 'hold-filter.sh');
      writeFileSync(filter, ['#!/bin/sh', gate.shell(), 'exec cat', ''].join('\n'), { mode: 0o755 });
      writeFileSync(ctx.env.GIT_CONFIG_GLOBAL!, `[filter "hold"]\n\tsmudge = '${filter}'\n\tclean = cat\n`, { flag: 'a' });
      writeFileSync(join(ctx.repo, '.gitattributes'), '*.dat filter=hold\n');
      writeFileSync(join(ctx.repo, 'payload.dat'), 'filtered payload\n');
      ctx.git(['add', '-A']);
      ctx.git(['commit', '-qm', 'A filtered file']);
      gate.arm();
      let built = false;
      const held = graft(ctx, ['add', 'held']).finally(() => { built = true; });
      try {
        await waitFor(() => existsSync(gate.reached) || built, 'the held checkout');
        assert(existsSync(gate.reached), 'the add finished before its checkout');
        // Run beside the add, so its exit is noticed if a command waited for it.
        const turn = async (...args: string[]) => {
          const outcome = await graft(ctx, ['turn', ...args]);
          assert(!built, `turn ${args[0]} waited for the build`);
          assert(outcome.ok, JSON.stringify(outcome.json));
          return outcome.json;
        };
        assert.equal((await turn('take', 'task-a', 'web', '--lease', a.lease)).granted, true);
        assert.equal((await turn('take', 'task-b', 'web', '--lease', b.lease)).position, 1);
        const listed = (await turn('list')).turns;
        assert(Array.isArray(listed) && listed.length === 1);
        assert.deepEqual(objectResult(listed[0]).queue, ['task-b']);
        assert.deepEqual(await turn('release', 'task-a', 'web', '--lease', a.lease),
          { resource: 'web', released: true, next: 'task-b' });
      } finally {
        gate.open();
        assert((await held).ok);
      }
    },
  },
  {
    name: 'MCP serves the turn tools without --allow-exec',
    run: async ctx => withClient(ctx, false, async client => {
      await initialize(client);
      const catalog = await discover(client, false);
      ctx.measure('advertised_tools', [...catalog.keys()].sort());
      const schema = objectResult(catalog.get('graft_turn_take')?.inputSchema);
      assert(Array.isArray(schema.required));
      assert.deepEqual([...schema.required].sort(), ['lease', 'name', 'resource']);
      assert.deepEqual(Object.keys(objectResult(schema.properties)).sort(), ['lease', 'name', 'resource', 'ttl', 'wait']);
      assert.equal(objectResult(catalog.get('graft_turn_list')?.annotations).readOnlyHint, true);
      for (const name of ['graft_turn_take', 'graft_turn_release']) {
        const annotations = objectResult(catalog.get(name)?.annotations);
        assert.deepEqual([annotations.readOnlyHint, annotations.destructiveHint], [false, false], name);
      }

      assert.deepEqual(await call(client, 'graft_turn_list'), { turns: [] });
      assert(!existsSync(join(ctx.repo, '.git', 'graft')));
      const [a, b] = ['task-a', 'task-b'].map(name => add(ctx, name));
      const owned = (graft: Graft) => ({ name: graft.name, lease: graft.lease, resource: 'web' });
      const first = await call(client, 'graft_turn_take', { ...owned(a), ttl: 600 });
      const since = number(first.since);
      assert.deepEqual(first, { resource: 'web', granted: true, holder: 'task-a', since, expires_at: since + 600, queue: [] });
      const queued = await call(client, 'graft_turn_take', owned(b));
      assert.deepEqual([queued.granted, queued.position, queued.queue], [false, 1, ['task-b']]);
      assert.deepEqual(take(ctx, b, 'web'), queued);
      const listed = await call(client, 'graft_turn_list');
      assert.deepEqual(listed, ctx.cli(['turn', 'list']));
      assert.deepEqual(listed, { turns: [{ resource: 'web', holder: 'task-a', since, expires_at: since + 600, queue: ['task-b'] }] });
      ctx.coverTool('graft_turn_list');

      await invalidCall(client, 'graft_turn_take', { name: 'task-a', resource: 'web' });
      await invalidCall(client, 'graft_turn_take', { name: 'task-a', lease: a.lease });
      await invalidCall(client, 'graft_turn_take', { ...owned(a), wait: 3601 });
      await invalidCall(client, 'graft_turn_take', { ...owned(a), ttl: 0 });
      await invalidCall(client, 'graft_turn_take', { ...owned(a), ttl: 86401 });
      await invalidCall(client, 'graft_turn_take', { ...owned(a), force: true });
      await invalidCall(client, 'graft_turn_release', { name: 'task-a', lease: a.lease });
      await invalidCall(client, 'graft_turn_list', { resource: 'web' });
      for (const tool of ['graft_turn_take', 'graft_turn_release']) {
        const refused = await call(client, tool, { ...owned(a), lease: randomUUID() }, true);
        assert.equal(refused.code, 'lease_mismatch', tool);
        assert(!JSON.stringify(refused).includes(a.lease));
        assert.equal((await call(client, tool, { ...owned(a), resource: 'Web' }, true)).code, 'invalid_resource', tool);
      }
      assert.deepEqual(await call(client, 'graft_turn_list'), listed);

      // A wait that is not granted returns when its time is up.
      let started = performance.now();
      const waited = await call(client, 'graft_turn_take', { ...owned(b), wait: 2 });
      assert.deepEqual([waited.granted, waited.position], [false, 1]);
      assert(performance.now() - started >= 1_900, 'a wait that was not granted returned early');

      // A wait the client cancels stops asking, so it takes no turn that
      // nobody would use. The server sends no reply to a cancelled request.
      const id = 7_000;
      client.send(JSON.stringify({ jsonrpc: '2.0', id, method: 'tools/call',
        params: { name: 'graft_turn_take', arguments: { ...owned(b), wait: 20 } } }));
      await delay(1_500);
      client.send(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/cancelled', params: { requestId: id, reason: 'test' } }));
      await delay(1_200);
      assert.deepEqual(await call(client, 'graft_turn_release', owned(a)), { resource: 'web', released: true, next: 'task-b' });
      await delay(2_200);
      assert.deepEqual(turns(ctx), [['web', null, ['task-b']]], 'a cancelled wait kept asking');

      // A wait still in progress is granted soon after the holder releases.
      assert.equal((await call(client, 'graft_turn_take', owned(a))).position, 2);
      const handed = await call(client, 'graft_turn_take', owned(b));
      assert.deepEqual([handed.granted, handed.holder, handed.queue], [true, 'task-b', ['task-a']]);
      const waiting = call(client, 'graft_turn_take', { ...owned(a), wait: 20 });
      await delay(300);
      started = performance.now();
      assert.deepEqual(await call(client, 'graft_turn_release', owned(b)), { resource: 'web', released: true, next: 'task-a' });
      const granted = await waiting;
      assert.deepEqual([granted.granted, granted.holder, granted.queue], [true, 'task-a', []]);
      assert(performance.now() - started < 10_000, 'the waiting call returned long after the release');
      ctx.coverTool('graft_turn_take');

      assert.deepEqual(await call(client, 'graft_turn_release', owned(b)), { resource: 'web', released: false, next: null });
      assert.deepEqual(await call(client, 'graft_turn_release', owned(a)), { resource: 'web', released: true, next: null });
      assert.deepEqual(await call(client, 'graft_turn_list'), { turns: [] });
      ctx.coverTool('graft_turn_release');
    }),
  },
];
