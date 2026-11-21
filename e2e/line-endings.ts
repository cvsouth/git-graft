/** A graft mirrors its primary's line endings wherever Git's own clean of a
 * primary file gives the committed blob, and leaves every other file to Git. */
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { objectResult, type Context, type Scenario } from './harness.ts';

type Case = {
  name: string;
  /** Global Git configuration lines for both the primary and its grafts. */
  config: string;
  attributes?: string;
  /** Committed blobs. */
  blobs: Record<string, string>;
  /** Let Git check every file out again under `config`, as a clone would. */
  checkout?: boolean;
  /** Primary working-tree bytes written after the checkout. */
  primary?: Record<string, string>;
  /** Tracked paths the graft must not reuse; Git writes them instead. */
  kept?: string[];
  /** Paths the primary's own status reports modified. Such a primary is
   * not merged into. */
  dirty?: string[];
};

const docs = Object.fromEntries(Array.from({ length: 10 }, (_, i) => [`docs/${i}.txt`, `doc ${i}\nline two\n`]));

const fixtures: Case[] = [
  {
    name: 'an LF primary under core.autocrlf=true',
    config: '[core]\n\tautocrlf = true\n',
    blobs: { ...docs, 'lf.txt': 'one\ntwo\n' },
  },
  {
    name: 'a CRLF primary under core.autocrlf=true',
    config: '[core]\n\tautocrlf = true\n',
    blobs: { ...docs, 'lf.txt': 'one\ntwo\n' },
    checkout: true,
  },
  {
    name: 'text=auto under core.autocrlf=true, with a binary file',
    config: '[core]\n\tautocrlf = true\n',
    attributes: '* text=auto\n',
    blobs: { ...docs, 'binary.dat': '\0binary\r\nbytes\n', 'crlf-blob.txt': 'kept\r\ncrlf\r\n' },
  },
  {
    name: 'eol=lf and eol=crlf attributes',
    config: '[core]\n\tautocrlf = false\n',
    attributes: '*.lf eol=lf\n*.crlf eol=crlf\n',
    blobs: { 'a.lf': 'one\ntwo\n', 'b.lf': 'one\ntwo\n', 'c.crlf': 'one\ntwo\n', 'd.crlf': 'one\ntwo\n' },
    checkout: true,
    // Git's clean of each gives its blob; Git would check them out the other way.
    primary: { 'b.lf': 'one\r\ntwo\r\n', 'd.crlf': 'one\ntwo\n' },
  },
  {
    name: 'mixed endings and a lone CR that Git cleans to their blobs',
    config: '[core]\n\tautocrlf = true\n',
    blobs: { 'mixed-blob.txt': 'one\r\ntwo\n', 'lone-cr.txt': 'one\rtwo\n', 'mixed.txt': 'one\ntwo\n' },
    primary: { 'mixed.txt': 'one\r\ntwo\n' },
  },
  {
    name: 'files whose bytes Git does not clean to their blobs',
    config: '[core]\n\tautocrlf = true\n',
    blobs: { 'lone-cr.txt': 'one\ntwo\n', 'binary.txt': 'one\ntwo\n', 'edited.txt': 'one\ntwo\n' },
    // A lone CR or NUL makes Git leave the file unconverted, so it differs.
    primary: { 'lone-cr.txt': 'one\r\ntwo\r', 'binary.txt': '\0one\r\ntwo\r\n', 'edited.txt': 'one\r\nTWO\r\n' },
    kept: ['lone-cr.txt', 'binary.txt', 'edited.txt'],
    dirty: ['binary.txt', 'edited.txt', 'lone-cr.txt'],
  },
  {
    name: 'a custom clean filter, even over identical bytes',
    config: '[core]\n\tautocrlf = false\n[filter "strip"]\n\tclean = tr -d \'\\\\r\'\n\tsmudge = cat\n',
    attributes: '*.f filter=strip\n',
    blobs: { 'hidden.f': 'one\ntwo\n', 'same.f': 'one\ntwo\n' },
    // The filter hides this edit from Git; Graft must not trust it.
    primary: { 'hidden.f': 'one\r\ntwo\r\n' },
    kept: ['hidden.f', 'same.f'],
  },
  {
    name: 'core.safecrlf=true refusing to add LF files Git checks out as CRLF',
    config: '[core]\n\tautocrlf = true\n\tsafecrlf = true\n',
    blobs: { 'lf.txt': 'one\ntwo\n', 'crlf.txt': 'one\ntwo\n' },
    primary: { 'crlf.txt': 'one\r\ntwo\r\n' },
    kept: ['.gitignore', 'tracked.txt', 'lf.txt'],
  },
];

function write(root: string, files: Record<string, string>) {
  for (const [name, bytes] of Object.entries(files)) {
    mkdirSync(dirname(join(root, name)), { recursive: true });
    writeFileSync(join(root, name), bytes);
  }
}

function check(ctx: Context, item: Case) {
  // Commit the blobs exactly as given, then switch to the case's settings.
  write(ctx.repo, item.blobs);
  if (item.attributes) writeFileSync(join(ctx.repo, '.gitattributes'), item.attributes);
  ctx.git(['add', '.']);
  ctx.git(['commit', '-qm', 'line endings']);
  writeFileSync(ctx.env.GIT_CONFIG_GLOBAL!, item.config);
  if (item.checkout) ctx.git(['checkout-index', '--all', '--force', '-u']);
  write(ctx.repo, item.primary ?? {});
  // Git calls a stat-tracked file of a new size modified without reading it,
  // so `git add` refreshes the rewritten files that still clean to their blobs.
  const rewritten = Object.keys(item.primary ?? {});
  if (rewritten.length && !item.dirty) ctx.git(['add', '--', ...rewritten]);
  const kept = new Set(item.kept ?? []);
  const status = ctx.git(['status', '--porcelain']).split('\n').filter(Boolean);
  assert.deepEqual(status.map(line => line.slice(3)).sort(), item.dirty ?? [], `${item.name}: primary status`);
  // Git's own checkout of the same commit under the same settings.
  const reference = join(ctx.root, 'reference');
  ctx.git(['checkout-index', '--all', '--force', `--prefix=${reference}/`]);

  const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone === true;
  const ws = ctx.spawn('mirror');
  const tracked = ctx.git(['ls-files']).split('\n');
  for (const name of tracked) {
    const graft = readFileSync(join(ws.path, name));
    const expected = native && !kept.has(name) ? join(ctx.repo, name) : join(reference, name);
    assert.deepEqual(graft, readFileSync(expected), `${item.name}: ${name}`);
  }
  const storage = objectResult(ws.result.storage);
  if (native && ['darwin', 'linux'].includes(process.platform)) {
    assert.equal(storage.cloned_files, tracked.length - kept.size, `${item.name}: ${JSON.stringify(storage)}`);
    assert.equal(storage.private_files, kept.size, item.name);
  }
  // Recorded stat data lets the first status trust every entry without
  // rewriting the index.
  const index = readFileSync(join(ws.path, '.git', 'index'));
  assert.equal(ctx.git(['status', '--porcelain'], ws.path), '', item.name);
  assert.deepEqual(readFileSync(join(ws.path, '.git', 'index')), index, `${item.name}: status rewrote the index`);
  assert.equal(ctx.git(['diff', '--stat'], ws.path), '', item.name);

  // An archived graft restores, and a merged one lands and can be undone.
  const archived = ctx.spawn('archived');
  // CRLF content is accepted under every case's settings, safecrlf included.
  writeFileSync(join(archived.path, 'archived.txt'), 'archived\r\n');
  ctx.git(['add', 'archived.txt'], archived.path);
  ctx.git(['commit', '-qm', 'archived work'], archived.path);
  ctx.cli(['remove', 'archived', '--lease', archived.lease, '--discard']);
  const restored = ctx.cli(['restore', 'archived', '--lease', archived.lease]);
  assert.equal(ctx.git(['status', '--porcelain'], String(restored.path)), '', `${item.name}: restored status`);
  if (item.dirty) return;
  const before = ctx.git(['rev-parse', 'HEAD']);
  writeFileSync(join(ws.path, 'feature.txt'), 'feature\r\n');
  ctx.git(['add', 'feature.txt'], ws.path);
  ctx.git(['commit', '-qm', 'feature'], ws.path);
  const merged = ctx.cli(['merge', 'mirror', '--lease', ws.lease]);
  assert.equal(merged.removed, true, item.name);
  assert.equal(readFileSync(join(ctx.repo, 'feature.txt'), 'utf8').replace(/\r\n/g, '\n'), 'feature\n');
  assert.equal(ctx.git(['status', '--porcelain']), '', `${item.name}: primary after merge`);
  ctx.cli(['merge', 'mirror', '--undo', '--lease', ws.lease]);
  assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false, `${item.name}: undo`);
  assert.equal(ctx.git(['diff', before, 'HEAD', '--stat']), '', `${item.name}: undo restores the tree`);
  assert.equal(ctx.git(['status', '--porcelain']), '', `${item.name}: primary after undo`);
}

export const cases: Scenario[] = fixtures.map(item => ({
  name: `a graft mirrors line endings: ${item.name}`,
  run(ctx: Context) { check(ctx, item); },
}));
