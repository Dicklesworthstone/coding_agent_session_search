/** Production query functions against real SQLite/FTS5, with a minimal oo1
 * statement adapter. This does not qualify browser layout or sqlite-wasm.
 * Run with Node 22+: node --experimental-vm-modules --test tests/pages/workspace-query.test.mjs
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { DatabaseSync } from 'node:sqlite';

const source = readFileSync(process.env.CASS_DATABASE_SOURCE ||
  new URL('../../src/pages_assets/database.js', import.meta.url), 'utf8');
const DAY = Date.parse('2026-10-15T00:00:00Z');
const ids = rows => Array.from(rows, row => row.conversation_id ?? row.id);

async function fixture(t) {
  const sql = new DatabaseSync(':memory:');
  t.after(() => sql.close());
  sql.exec(`
    CREATE TABLE conversations(id INTEGER PRIMARY KEY, agent TEXT, workspace TEXT COLLATE NOCASE,
      title TEXT, source_path TEXT, started_at INTEGER, ended_at INTEGER, message_count INTEGER);
    CREATE TABLE messages(id INTEGER PRIMARY KEY, conversation_id INTEGER, role TEXT, content TEXT);
    CREATE VIRTUAL TABLE messages_fts USING fts5(content, tokenize='porter');
    CREATE VIRTUAL TABLE messages_code_fts USING fts5(content, tokenize='unicode61');
  `);
  const insert = sql.prepare('INSERT INTO conversations VALUES (?, ?, ?, ?, ?, ?, NULL, 1)');
  const message = sql.prepare('INSERT INTO messages VALUES (?, ?, ?, ?)');
  const prose = sql.prepare('INSERT INTO messages_fts(rowid, content) VALUES (?, ?)');
  const code = sql.prepare('INSERT INTO messages_code_fts(rowid, content) VALUES (?, ?)');
  function seed(id, workspace, agent = 'codex', at = DAY, text = 'needle error_code phrase words') {
    insert.run(id, agent, workspace, `conversation ${id}`, `/source/${id}.jsonl`, at);
    message.run(id, id, 'user', text);
    prose.run(id, text); code.run(id, text);
  }
  seed(1, '/work/project', 'codex', DAY - 1);
  seed(2, '/work/project', 'claude', DAY);
  seed(3, '/work/project', 'codex', DAY + 86_400_000 - 1);
  seed(4, '/work/project-copy');
  seed(5, '/Work/project');
  seed(6, String.raw`C:\work\project`);
  seed(7, "/work/a' OR 1=1 --/%_?&#🔑");
  seed(8, null);
  seed(9, ' /work/project ');
  seed(10, '/work/project', 'codex', DAY + 86_400_000);
  seed(11, '/work/project', 'codex', null);

  const calls = [];
  let hydrated = 0, finalized = 0;
  const adapter = { prepare(query) {
    const prepared = sql.prepare(query);
    const call = { query, params: [] }; calls.push(call);
    let iterator, row;
    return {
      bind(params) { call.params = Array.from(params); },
      step() {
        iterator ??= prepared.iterate(...call.params);
        const next = iterator.next(); row = next.value;
        if (!next.done) hydrated++;
        return !next.done;
      },
      get(target) { return typeof target === 'number' ? Object.values(row)[target] : { ...row }; },
      finalize() { iterator?.return(); finalized++; },
    };
  } };
  const module = new vm.SourceTextModule(`${source}\nexport function installForTest(handle) { db = handle; }`, {
    context: vm.createContext({ console: { log() {}, warn() {}, error() {} } }),
  });
  await module.link(() => { throw Error('query tests must not import a runtime'); });
  await module.evaluate();
  const api = module.namespace; api.installForTest(adapter);
  return { api, sql, seed, calls, stats: () => ({ hydrated, finalized }) };
}

test('text search intersects workspace, agent and inclusive UTC date bounds in both FTS modes', async t => {
  const { api } = await fixture(t);
  for (const searchMode of ['auto', 'prose', 'code']) {
    assert.deepEqual(ids(api.searchConversations('needle', {
      workspace: '/work/project', agent: 'codex', since: '2026-10-15', until: '2026-10-15', searchMode,
    })), [3]);
  }
});

test('browsing uses the same intersection without requiring a text match', async t => {
  const { api, seed } = await fixture(t);
  seed(12, '/work/project', 'codex', DAY + 1, 'no search terms');
  const filters = { workspace: '/work/project', agent: 'codex', since: '2026-10-15', until: '2026-10-15' };
  assert.deepEqual(ids(api.getConversations(filters)), [3, 12]);
  assert.deepEqual(ids(api.searchConversations('needle', filters)), [3]);
});

test('workspace paths are literal, case-sensitive identities including Windows and special characters', async t => {
  const { api, calls } = await fixture(t);
  for (const [workspace, expected] of [
    ['/work/project-copy', [4]], ['/Work/project', [5]], [String.raw`C:\work\project`, [6]],
    ["/work/a' OR 1=1 --/%_?&#🔑", [7]], [' /work/project ', [9]], ['/work/project/', []], ['/missing', []],
  ]) {
    assert.deepEqual(ids(api.searchConversations('needle', { workspace })), expected);
    assert.deepEqual(ids(api.getConversations({ workspace })), expected);
    for (const call of calls.slice(-2)) {
      assert.ok(!call.query.includes(workspace), 'a path was interpolated into SQL');
      assert.ok(call.params.includes(workspace));
    }
  }
});

test('scoped pages do not filter an already limited cross-workspace shortlist', async t => {
  const { api, seed, stats } = await fixture(t);
  for (let id = 100; id < 220; id++) seed(id, '/other', 'codex', DAY);
  for (let id = 300; id < 423; id++) seed(id, '/target', 'codex', DAY);
  for (const text of [false, true]) {
    const all = [];
    const before = stats().hydrated;
    for (const offset of [0, 50, 100]) {
      const options = { workspace: '/target', limit: 50, offset };
      const rows = text ? api.searchConversations('needle', options) : api.getConversations(options);
      all.push(...ids(rows));
    }
    const expected = Array.from({ length: 123 }, (_, i) => 300 + i);
    assert.deepEqual(all, text ? expected : expected.reverse());
    assert.equal(new Set(all).size, 123);
    assert.equal(stats().hydrated - before, 123, 'query hydrated off-page rows');
  }
});

test('omitted selectors preserve existing unfiltered, agent and time browsing results', async t => {
  const { api } = await fixture(t);
  assert.deepEqual(ids(api.getConversations()), ids(api.getRecentConversations()));
  assert.deepEqual(ids(api.getConversations({ agent: 'claude' })), ids(api.getConversationsByAgent('claude')));
  assert.deepEqual(ids(api.getConversations({ since: DAY, until: DAY })),
    ids(api.getConversationsByTimeRange(DAY, DAY)));
  for (const workspace of [undefined, null, '']) {
    assert.deepEqual(ids(api.searchConversations('needle', { workspace })),
      ids(api.searchConversations('needle')));
  }
});

test('invalid workspace, time and pagination reject before preparing any SQL', async t => {
  const { api, calls } = await fixture(t);
  const invalid = [
    ...[false, 0, [], {}, '\ud800', 'x\0y', 'x'.repeat(4097)].map(workspace => ({ workspace })),
    { workspace: '/work/project', since: '2026-02-30' },
    { workspace: '/work/project', since: DAY + 1, until: DAY },
    { limit: -1 }, { limit: 1001 }, { limit: 1.5 }, { offset: -1 }, { offset: Number.MAX_SAFE_INTEGER + 1 },
  ];
  for (const options of invalid) {
    assert.throws(() => api.getConversations(options), { name: 'RangeError' });
    for (const query of ['', 'needle']) {
      assert.throws(() => api.searchConversations(query, options), { name: 'RangeError' });
    }
  }
  assert.equal(calls.length, 0);
});

test('Boolean phrases and prefixes keep their grammar within the selected workspace', async t => {
  const { api, seed } = await fixture(t);
  seed(20, '/syntax', 'codex', DAY, 'alpha beta together');
  seed(21, '/syntax', 'codex', DAY, 'gamma delta');
  seed(22, '/other', 'codex', DAY, 'gamma delta');
  assert.deepEqual(ids(api.searchConversations('"alpha beta" OR gam*', { workspace: '/syntax' })).sort(), [20, 21]);
  assert.deepEqual(ids(api.searchConversations('(alpha OR gamma) NOT delta', { workspace: '/syntax' })), [20]);
  assert.throws(() => api.searchConversations('NOT delta', { workspace: '/syntax' }), { name: 'SyntaxError' });
});

test('zero-sized and exhausted pages are empty and statements are finalized', async t => {
  const { api, calls, stats } = await fixture(t);
  for (const options of [{ limit: 0 }, { offset: 1000 }]) {
    assert.deepEqual(ids(api.getConversations({ workspace: '/work/project', ...options })), []);
    assert.deepEqual(ids(api.searchConversations('needle', { workspace: '/work/project', ...options })), []);
  }
  assert.equal(stats().hydrated, 0);
  assert.equal(stats().finalized, calls.length);
});

test('both inclusive bounds, including the Unix epoch, remain active under workspace filtering', async t => {
  const { api, seed } = await fixture(t);
  seed(30, '/epoch', 'codex', 0); seed(31, '/epoch', 'codex', 1); seed(32, '/other', 'codex', 0);
  const options = { workspace: '/epoch', since: 0, until: 0 };
  assert.deepEqual(ids(api.getConversations(options)), [30]);
  assert.deepEqual(ids(api.searchConversations('needle', options)), [30]);
});
