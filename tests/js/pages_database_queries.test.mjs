/**
 * Run the shipped viewer query code against real SQLite/FTS5, not a SQL mock.
 * Node 22+ supplies SQLite; the small adapter implements only sqlite-wasm's
 * statement iteration shape. It does not parse SQL or decide result rows.
 *
 * node --experimental-vm-modules --test tests/js/pages_database_queries.test.mjs
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const databaseFile = process.env.CASS_PAGES_TEST_DATABASE_JS ||
  fileURLToPath(new URL("../../src/pages_assets/database.js", import.meta.url));
const source = readFileSync(databaseFile, "utf8");

async function fixture(t) {
  const sqlite = new DatabaseSync(":memory:");
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
    CREATE INDEX messages_by_conversation ON messages(conversation_id, idx);
    CREATE VIRTUAL TABLE messages_fts USING fts5(content, tokenize='porter unicode61');
    CREATE VIRTUAL TABLE messages_code_fts USING fts5(content, tokenize='unicode61');
    INSERT INTO conversations VALUES
      (1, 'codex', 'project-one', 'one', 'one.jsonl', 100, 200, 0, '{}'),
      (2, 'claude_code', 'project-two', 'two', 'two.jsonl', 300, 400, 0, '{}');
  `);
  const statements = [];
  let finalized = 0;
  const adapter = {
    prepare(sql) {
      const statement = sqlite.prepare(sql);
      const record = { sql, params: [] };
      statements.push(record);
      let rows;
      let current;
      return {
        bind(params) { record.params = Array.from(params); },
        step() {
          rows ||= statement.iterate(...record.params);
          const next = rows.next();
          current = next.value;
          return !next.done;
        },
        get(column) {
          return typeof column === "number" ? Object.values(current)[column] : { ...current };
        },
        finalize() { rows?.return?.(); finalized += 1; },
      };
    },
    // closeDatabase() must invalidate sources but the test owns this handle.
    close() {},
  };
  // Append a test-only binding in the VM rather than changing production
  // exports or rewriting the function under test. All SQL is shipped code.
  const module = new vm.SourceTextModule(
    `${source}\nexport function __bindFixture(handle) { db = handle; }`,
    { context: vm.createContext({ console }) },
  );
  await module.link(() => { throw new Error("Unexpected static import"); });
  await module.evaluate();
  module.namespace.__bindFixture(adapter);
  const insert = sqlite.prepare("INSERT INTO messages VALUES (?, ?, ?, 'user', ?, NULL, NULL, NULL)");
  const fts = sqlite.prepare("INSERT INTO messages_fts(rowid, content) VALUES (?, ?)");
  const codeFts = sqlite.prepare("INSERT INTO messages_code_fts(rowid, content) VALUES (?, ?)");
  function add(id, text, conversation = 1, idx = id) {
    insert.run(id, conversation, idx, text);
    fts.run(id, text);
    codeFts.run(id, text);
  }
  function ids(query, options = {}) {
    return Array.from(module.namespace.searchConversations(query, options), (row) => row.message_id)
      .sort((a, b) => a - b);
  }
  t.after(() => assert.equal(finalized, statements.length, "all query statements must finalize"));
  return { api: module.namespace, sqlite, statements, add, ids };
}

for (const searchMode of ["code", "prose", "auto"]) {
  test(`${searchMode}: phrases require adjacent ordered words; prefixes expand`, async (t) => {
    const f = await fixture(t);
    f.add(1, "authentication failed during token refresh");
    f.add(2, "failed authentication; token stale and refresh later");
    f.add(3, "authentication retry eventually failed");
    assert.deepEqual(f.ids('"authentication failed"', { searchMode }), [1]);
    assert.deepEqual(f.ids('"token refresh"', { searchMode }), [1]);
    assert.deepEqual(f.ids("authent* fail*", { searchMode }), [1, 2, 3]);
    assert.deepEqual(f.ids('"token refr"*', { searchMode }), [1]);
    assert.deepEqual(f.ids('"authe*"', { searchMode }), []);
    assert.deepEqual(f.ids("authentication failed", { searchMode }), [1, 2, 3]);
  });
}

test("Boolean operators obey NOT > AND > OR, including implicit AND", async (t) => {
  const f = await fixture(t);
  for (let mask = 0; mask < 8; mask += 1) {
    const words = ["alpha", "beta", "gamma"].filter((_, bit) => mask & (1 << bit));
    f.add(mask + 1, words.join(" ") || "nothing");
  }
  const oracle = (predicate) => Array.from({ length: 8 }, (_, mask) => mask)
    .filter((mask) => predicate(Boolean(mask & 1), Boolean(mask & 2), Boolean(mask & 4)))
    .map((mask) => mask + 1);
  const cases = [
    ["alpha OR beta gamma", (a, b, c) => a || (b && c)],
    ["alpha OR beta AND gamma", (a, b, c) => a || (b && c)],
    ["(alpha OR beta) gamma", (a, b, c) => (a || b) && c],
    ["alpha AND (beta OR gamma)", (a, b, c) => a && (b || c)],
    ["alpha NOT beta", (a, b) => a && !b],
    ["alpha AND NOT beta", (a, b) => a && !b],
    ["alpha NOT beta OR gamma", (a, b, c) => (a && !b) || c],
    ["alpha OR beta NOT gamma", (a, b, c) => a || (b && !c)],
    ["alpha NOT (beta OR gamma)", (a, b, c) => a && !(b || c)],
    ["alpha NOT beta NOT gamma", (a, b, c) => a && !b && !c],
    ["alpha AND NOT beta NOT gamma", (a, b, c) => a && !b && !c],
    ["alpha NOT (beta NOT gamma)", (a, b, c) => a && !(b && !c)],
    ["(alpha OR beta) NOT (beta AND gamma)", (a, b, c) => (a || b) && !(b && c)],
    ["((alpha)) OR(beta AND gamma)", (a, b, c) => a || (b && c)],
  ];
  for (const [query, predicate] of cases) {
    assert.deepEqual(f.ids(query), oracle(predicate), query);
  }
  const a = new Set(f.ids("alpha"));
  assert(f.ids("alpha AND beta").every((id) => a.has(id)), "AND narrows");
  assert([...a].every((id) => f.ids("alpha OR beta").includes(id)), "OR widens");
});

test("phrases, prefixes and Boolean groups compose with filtering and pagination", async (t) => {
  const f = await fixture(t);
  f.add(1, "token refresh authentication", 1);
  f.add(2, "token refresh authentication", 2);
  f.add(3, "token stale refresh", 1);
  f.add(4, "retry refresh authentication", 1);
  const query = '("token refresh" OR retry) authent*';
  assert.deepEqual(f.ids(query), [1, 2, 4]);
  assert.deepEqual(f.ids(query, { agent: "codex" }), [1, 4]);
  assert.deepEqual(f.ids(query, { since: 200 }), [2]);
  assert.deepEqual(f.ids(query, { until: 200 }), [1, 4]);
  const all = Array.from(f.api.searchConversations(query), (row) => row.message_id);
  const pages = [0, 1, 2].flatMap((offset) =>
    Array.from(f.api.searchConversations(query, { offset, limit: 1 }), (row) => row.message_id));
  assert.deepEqual(pages, all, "ranked pagination has no duplicates or omissions");
  assert.equal(f.api.searchConversations(query, { limit: 0 }).length, 0);
  assert(f.api.searchConversations(query)[0].snippet.includes("<mark>"));
});

test("literal code, quoted operators and escaped quotes stay bound data", async (t) => {
  const f = await fixture(t);
  f.add(1, 'src/search.rs std::vector C:\\Users\\dev foo() alpha "beta" gamma');
  f.add(2, "AND OR NOT content:alpha");
  f.add(3, "alpha beta gamma");
  for (const query of ["src/search.rs", "std::vector", "C:\\Users\\dev", "foo()"]) {
    assert.deepEqual(f.ids(query, { searchMode: "code" }), [1], query);
  }
  assert.deepEqual(f.ids('"AND" "OR" "NOT"'), [2]);
  assert.deepEqual(f.ids("content:alpha"), [2], "no raw FTS column selector");
  assert.deepEqual(f.ids('"alpha ""beta"" gamma"'), [1, 3]);
  assert.deepEqual(f.ids('"x\"\" OR alpha\"\" y"'), []);
  const last = f.statements.at(-1);
  assert(last.sql.includes("MATCH ?"));
  assert(!last.sql.includes("alpha"));
  assert.equal(f.sqlite.prepare("SELECT COUNT(*) AS n FROM messages").get().n, 3);
});

test("malformed and over-budget queries fail before preparing SQL", async (t) => {
  const f = await fixture(t);
  const invalid = [
    '"unterminated', '""', '"   "', '"phrase"suffix',
    "alpha OR", "AND alpha", "alpha AND OR beta", "NOT alpha", "alpha OR NOT beta",
    "()", "(alpha", "alpha)", "alpha NOT", "*", "alpha**",
    "(".repeat(17) + "alpha" + ")".repeat(17),
    "alpha ".repeat(129), "x".repeat(4097), null, 123,
  ];
  for (const query of invalid) {
    assert.throws(() => f.ids(query), undefined, String(query));
  }
  assert.equal(f.statements.length, 0, "invalid queries must not hit SQLite");
  assert.deepEqual(f.ids(" \t\n "), []);
  assert.equal(f.statements.length, 0);
});

test("parser limits include valid boundary queries", async (t) => {
  const f = await fixture(t);
  f.add(1, "alpha");
  assert.deepEqual(f.ids("(".repeat(16) + "alpha" + ")".repeat(16)), [1]);
  assert.deepEqual(f.ids(Array(128).fill("alpha").join(" ")), [1]);
});

test("seeded Boolean trees agree with an independent truth-table oracle", async (t) => {
  const f = await fixture(t);
  for (let mask = 0; mask < 8; mask += 1) {
    f.add(mask + 1, ["alpha", "beta", "gamma"].filter((_, bit) => mask & (1 << bit)).join(" ") || "none");
  }
  let seed = 0x5eed;
  function next(max) {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed % max;
  }
  function expression(depth) {
    if (depth === 0 || next(4) === 0) {
      const bit = next(3);
      return { query: ["alpha", "beta", "gamma"][bit], accepts: (mask) => Boolean(mask & (1 << bit)) };
    }
    const left = expression(depth - 1);
    const right = expression(depth - 1);
    const operator = ["AND", "OR", "NOT"][next(3)];
    return {
      query: `(${left.query} ${operator} ${right.query})`,
      accepts: (mask) => operator === "AND" ? left.accepts(mask) && right.accepts(mask)
        : operator === "OR" ? left.accepts(mask) || right.accepts(mask)
          : left.accepts(mask) && !right.accepts(mask),
    };
  }
  for (let trial = 0; trial < 256; trial += 1) {
    const tree = expression(4);
    const expected = Array.from({ length: 8 }, (_, mask) => mask).filter(tree.accepts)
      .map((mask) => mask + 1);
    assert.deepEqual(f.ids(tree.query), expected, `seed=0x5eed trial=${trial} query=${tree.query}`);
  }
});
