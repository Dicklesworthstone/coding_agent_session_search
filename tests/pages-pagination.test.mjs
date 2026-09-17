/**
 * Run: node --experimental-vm-modules --test tests/pages-pagination.test.mjs
 * Requires Node >=22.13 (node:sqlite). Runs the shipped query code against real
 * SQLite/FTS5 through a sqlite-wasm statement adapter. This is not browser E2E
 * coverage and does not exercise WASM initialization or encrypted loading.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { DatabaseSync } from 'node:sqlite';
import { test } from 'node:test';
import vm from 'node:vm';

const databaseSource = await readFile(new URL('../src/pages_assets/database.js', import.meta.url), 'utf8');

async function fixture(t, count = 137, globals = {}) {
    const sqlite = new DatabaseSync(':memory:');
    t.after(() => sqlite.close());
    sqlite.exec(`
        CREATE TABLE conversations (
            id INTEGER PRIMARY KEY, agent TEXT, workspace TEXT, title TEXT,
            source_path TEXT, started_at INTEGER, ended_at INTEGER,
            message_count INTEGER, metadata_json TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER,
            role TEXT, content TEXT, created_at INTEGER, updated_at INTEGER, model TEXT
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(content, tokenize='porter unicode61');
        CREATE VIRTUAL TABLE messages_code_fts USING fts5(content, tokenize='unicode61');
    `);
    const conv = sqlite.prepare('INSERT INTO conversations VALUES (?, ?, ?, ?, ?, ?, NULL, 1, NULL)');
    const message = sqlite.prepare("INSERT INTO messages VALUES (?, ?, 0, 'user', ?, NULL, NULL, NULL)");
    const prose = sqlite.prepare('INSERT INTO messages_fts(rowid, content) VALUES (?, ?)');
    const code = sqlite.prepare('INSERT INTO messages_code_fts(rowid, content) VALUES (?, ?)');
    for (let id = 1; id <= count; id += 1) {
        conv.run(id, id % 2 ? 'codex' : 'claude', id % 2 ? '/one' : '/two', `Session ${id}`, `/logs/${id}`, 1000);
        const content = 'pagination needle search_target';
        message.run(id, id, content);
        prose.run(id, content);
        code.run(id, content);
    }
    sqlite.exec('PRAGMA query_only=ON');
    const queries = [];
    let finalized = 0;
    const handle = {
        prepare(sql) {
            const statement = sqlite.prepare(sql);
            let params = [];
            let iterator;
            let row;
            const execution = { sql, params, rows: 0 };
            queries.push(execution);
            return {
                bind(values) { params = values; execution.params = Array.from(values); },
                step() {
                    iterator ??= statement.iterate(...params);
                    const next = iterator.next();
                    row = next.value;
                    if (!next.done) execution.rows += 1;
                    return !next.done;
                },
                get(column) { return typeof column === 'number' ? Object.values(row)[column] : { ...row }; },
                finalize() { iterator?.return(); finalized += 1; },
            };
        },
    };
    // Inject only the database handle into an otherwise unchanged source file.
    // Tests do not replace any production SQL, filtering or ranking logic.
    const context = vm.createContext({ console, URL, ...globals });
    const module = new vm.SourceTextModule(`${databaseSource}\nexport function attachTestDatabase(value) { db = value; }`, { context });
    await module.link(() => { throw new Error('Unexpected static import'); });
    await module.evaluate();
    module.namespace.attachTestDatabase(handle);
    return { api: module.namespace, module, context, sqlite, queries, finalized: () => finalized };
}

function ids(rows, key = 'id') {
    return Array.from(rows, row => row[key]);
}

for (const searchMode of ['prose', 'code', 'auto']) {
    test(`${searchMode} search reaches every match in stable non-overlapping pages`, async t => {
        const { api, finalized } = await fixture(t);
        const query = searchMode === 'prose' ? 'needle' : 'search_target';
        const all = [0, 50, 100].flatMap(offset => ids(api.searchConversations(query, { searchMode, limit: 50, offset }), 'message_id'));
        assert.deepEqual(all, Array.from({ length: 137 }, (_, i) => i + 1));
        assert.deepEqual(ids(api.searchConversations(query, { searchMode, offset: 50 }), 'message_id'), all.slice(50, 100));
        assert.equal(api.searchConversations(query, { searchMode, offset: 150 }).length, 0);
        assert.equal(finalized(), 5);
    });
}

test('recent browsing is deterministic even when all timestamps are tied', async t => {
    const { api } = await fixture(t);
    const all = [0, 50, 100].flatMap(offset => ids(api.getRecentConversations(50, offset)));
    assert.deepEqual(all, Array.from({ length: 137 }, (_, i) => 137 - i));
    assert.equal(api.getRecentConversations(50, 150).length, 0);
});

test('agent, workspace and optional time filters apply before page boundaries', async t => {
    const { api } = await fixture(t);
    const expected = Array.from({ length: 69 }, (_, i) => 137 - 2 * i);
    for (const load of [
        offset => api.getConversationsByAgent('codex', 50, 1000, 1000, offset),
        offset => api.getConversationsByWorkspace('/one', 50, offset),
    ]) {
        assert.deepEqual([...ids(load(0)), ...ids(load(50))], expected);
        assert.equal(load(100).length, 0);
    }
    assert.deepEqual(ids(api.getConversationsByTimeRange(1000, 1000, 50, 50)), Array.from({ length: 50 }, (_, i) => 87 - i));
    assert.equal(api.getConversationsByAgent('codex', 50, 1001, null, 0).length, 0);
    assert.equal(api.getConversationsByTimeRange(null, 999, 50, 0).length, 0);
    assert.equal(api.getConversationsByTimeRange(1000, null, 50, 100).length, 37);
});

test('combined full-text filters retain every selected result across pages', async t => {
    const { api } = await fixture(t);
    const options = { agent: 'codex', since: 1000, until: 1000, limit: 50 };
    const all = [0, 50].flatMap(offset => ids(api.searchConversations('needle', { ...options, offset }), 'message_id'));
    assert.deepEqual(all, Array.from({ length: 69 }, (_, i) => 2 * i + 1));
    assert.equal(api.searchConversations('needle', { ...options, since: 1001 }).length, 0);
});

test('unsafe pagination cannot disable LIMIT or silently change the page', async t => {
    const { api, finalized } = await fixture(t);
    const loaders = [
        (limit, offset) => api.searchConversations('needle', { limit, offset }),
        (limit, offset) => api.getRecentConversations(limit, offset),
        (limit, offset) => api.getConversationsByAgent('codex', limit, null, null, offset),
        (limit, offset) => api.getConversationsByWorkspace('/one', limit, offset),
        (limit, offset) => api.getConversationsByTimeRange(null, null, limit, offset),
    ];
    for (const load of loaders) {
        for (const invalid of [-1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, '50', null]) {
            assert.throws(() => load(invalid, 0), /limit/i);
            assert.throws(() => load(50, invalid), /offset/i);
        }
        assert.throws(() => load(1001, 0), /limit/i);
        assert.equal(load(0, 0).length, 0);
    }
    assert.equal(finalized(), loaders.length, 'rejected requests must not execute SQL');
});

test('database errors are distinguishable from an empty result set', async t => {
    const { api, sqlite } = await fixture(t);
    assert.equal(api.searchConversations('absentword').length, 0);
    sqlite.exec('PRAGMA query_only=OFF; DROP TABLE messages_code_fts; PRAGMA query_only=ON');
    assert.throws(() => api.searchConversations('search_target'), /messages_code_fts/);
});

test('punctuation is parameterized and the archive remains read-only', async t => {
    const { api, sqlite } = await fixture(t);
    assert.equal(api.searchConversations('" OR 1=1; --').length, 0);
    assert.equal(sqlite.prepare('SELECT COUNT(*) AS n FROM conversations').get().n, 137);
    assert.throws(() => api.execute('DELETE FROM conversations'), /read-only/);
    assert.throws(() => api.queryAll('DELETE FROM conversations'), /readonly/i);
});
