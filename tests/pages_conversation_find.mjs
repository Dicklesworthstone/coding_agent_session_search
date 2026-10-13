/** Production transcript search against real SQLite, not a SQL/result mock.
 * Run: node --experimental-vm-modules --test tests/pages_conversation_find.mjs
 * The adapter implements only sqlite-wasm's statement interface. The production
 * module, SQL, order/cursor logic, cancellation and body caches run unchanged.
 * This is not browser E2E or sqlite-wasm runtime qualification.
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";
import vm from "node:vm";

const sourceText = await readFile(new URL("../src/pages_assets/database.js", import.meta.url), "utf8");

async function fixture(records = []) {
  const native = new DatabaseSync(":memory:");
  native.exec(`CREATE TABLE messages (
    id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, idx INTEGER, content TEXT,
    role TEXT DEFAULT 'user', created_at INTEGER, updated_at INTEGER, model TEXT);
    CREATE INDEX message_order ON messages(conversation_id, idx);`);
  const insert = native.prepare("INSERT INTO messages(id, conversation_id, idx, content) VALUES(?,?,?,?)");
  native.exec("BEGIN");
  for (const row of records) insert.run(row.id, row.conversation ?? 1, row.idx, row.content);
  native.exec("COMMIT");
  const queries = [];
  let openStatements = 0;
  let closed = false;
  const handle = {
    prepare(sql) {
      const statement = native.prepare(sql);
      const entry = { sql, params: [], rows: 0, fields: new Set() };
      queries.push(entry);
      openStatements += 1;
      let iterator;
      let row;
      let finalized = false;
      return {
        bind(params) { entry.params = params; },
        step() {
          iterator ??= statement.iterate(...entry.params);
          const result = iterator.next();
          row = result.value;
          if (!result.done) {
            entry.rows += 1;
            for (const key of Object.keys(row)) entry.fields.add(key);
          }
          return !result.done;
        },
        get(key) { return typeof key === "number" ? Object.values(row)[key] : { ...row }; },
        finalize() {
          assert.equal(finalized, false, "statement finalized twice");
          finalized = true;
          iterator?.return();
          openStatements -= 1;
        },
      };
    },
    close() { if (!closed) { native.close(); closed = true; } },
  };
  const context = vm.createContext({ console: { log() {}, warn() {}, error() {} }, setTimeout });
  // Fixture installation is appended to the evaluated test copy only. No
  // production hook or implementation text replacement is needed.
  const module = new vm.SourceTextModule(sourceText + `
    export function installFixture(handle) { db = handle; isInitialized = true; lifecycleGeneration++; }
  `, { context });
  await module.link(() => { throw new Error("unexpected static import"); });
  await module.evaluate();
  const api = module.namespace;
  api.installFixture(handle);
  const source = new api.ConversationMessageSource(1);
  return { native, api, source, queries, handle,
    get openStatements() { return openStatements; },
    close() { assert.equal(openStatements, 0); handle.close(); },
  };
}

function sortedRows(rows) {
  return rows.filter((row) => (row.conversation ?? 1) === 1).sort((a, b) => {
    if (a.idx === null && b.idx !== null) return -1;
    if (a.idx !== null && b.idx === null) return 1;
    return (a.idx ?? 0) - (b.idx ?? 0) || a.id - b.id;
  });
}

function oracle(rows, text, anchor, direction) {
  const ordered = sortedRows(rows);
  const anchorIndex = ordered.findIndex((row) => row.id === anchor);
  const start = anchor === null ? (direction === 1 ? 0 : ordered.length - 1) : anchorIndex + direction;
  for (let n = 0; n < ordered.length; n += 1) {
    const raw = start + n * direction;
    const index = ((raw % ordered.length) + ordered.length) % ordered.length;
    if (ordered[index].content?.includes(text)) {
      return { messageId: ordered[index].id, index, wrapped: raw < 0 || raw >= ordered.length,
        checked: n + 1, total: ordered.length };
    }
  }
  return { messageId: null, index: -1,
    wrapped: ordered.length > 0 && anchor !== null &&
      (direction === 1 ? start + ordered.length - 1 >= ordered.length : start - ordered.length + 1 < 0),
    checked: ordered.length, total: ordered.length };
}
const plain = (value) => JSON.parse(JSON.stringify(value));

const tricky = [
  { id: 90, idx: null, content: "start needle" },
  { id: 4, idx: null, content: null },
  { id: 80, idx: -7, content: "needle at negative order" },
  { id: 5, idx: 8, content: "gap" },
  { id: 24, idx: 8, content: "needle duplicate order" },
  { id: 1, idx: 999, content: "last needle" },
  { id: 2, idx: 0, content: "foreign needle", conversation: 2 },
];

test("find searches both directions with NULL/gapped/tied order keys and wraps exactly once", async () => {
  const f = await fixture(tricky);
  try {
    for (const anchor of [null, ...sortedRows(tricky).map((r) => r.id)]) {
      for (const direction of [1, -1]) {
        for (const text of ["needle", "gap", "absent", "negative", "start", "last"]) {
          assert.deepEqual(plain(await f.source.findText(text, { afterMessageId: anchor, direction })),
            oracle(tricky, text, anchor, direction));
          assert.equal(f.openStatements, 0);
        }
      }
    }
  } finally { f.close(); }
});

test("literal text preserves punctuation, whitespace, Unicode and case without SQL/FTS syntax", async () => {
  const literals = ["100%_", "' OR 1=1 --", "foo()", "AND NOT", 'a"b', "δ🙂東京", "\n", "\0after", "  "];
  const rows = literals.map((content, i) => ({ id: i + 1, idx: i, content: `before${content}after` }));
  const f = await fixture(rows);
  try {
    for (const text of literals) assert.deepEqual(plain(await f.source.findText(text)), oracle(rows, text, null, 1));
    assert.equal((await f.source.findText("and not")).messageId, null, "case must not be silently folded");
    assert.equal(f.native.prepare("SELECT COUNT(*) AS n FROM messages").get().n, literals.length);
    const scans = f.queries.filter((q) => q.sql.includes("AS matched"));
    assert(scans.every((q) => !q.sql.includes("OR 1=1 --") && q.params.length >= 3));
  } finally { f.close(); }
});

test("100000-message search reaches offscreen text, yields, and never hydrates bodies or an offset page", async () => {
  const n = 100_000;
  const f = await fixture(Array.from({ length: n }, (_, i) => ({
    id: i + 1, idx: i * 2, content: i === n - 1 ? "deep-target" : "ordinary message",
  })));
  try {
    const progress = [];
    let timerRan = false;
    setTimeout(() => { timerRan = true; }, 0);
    const result = await f.source.findText("deep-target", { onProgress: (p) => progress.push(p.checked) });
    assert.equal(result.messageId, n);
    assert.equal(result.index, n - 1);
    assert.equal(result.checked, n);
    assert(timerRan, "scan must yield to event-loop tasks");
    assert(progress.length > 700);
    assert(progress.every((value, i) => value === (i + 1) * 128));
    assert.deepEqual(plain(f.source.getCacheStats()), { idPages: 0, messages: 0, textUnits: 0 });
    const scans = f.queries.filter((q) => q.sql.includes("AS matched"));
    assert(scans.every((q) => q.rows <= 128 && !q.fields.has("content") && !q.sql.includes("OFFSET")));
    const seek = scans.find((q) => q.sql.includes("(idx, id) >"));
    const plan = f.native.prepare(`EXPLAIN QUERY PLAN ${seek.sql}`).all(...seek.params).map((r) => r.detail).join("\n");
    assert.match(plan, /USING INDEX message_order/);
    assert.match(plan, /idx>/);
    assert.doesNotMatch(plan, /TEMP B-TREE/);
  } finally { f.close(); }
});

test("reverse keyset pages cross the NULL partition and use the index range", async () => {
  const rows = Array.from({ length: 530 }, (_, i) => ({
    id: 1000 - i, idx: i < 270 ? null : Math.floor(i / 3), content: i === 269 ? "target" : "ordinary",
  }));
  const f = await fixture(rows);
  try {
    const result = await f.source.findText("target", { direction: -1 });
    assert.deepEqual(plain(result), oracle(rows, "target", null, -1));
    const seek = f.queries.find((q) => q.sql.includes("(idx, id) <"));
    const plan = f.native.prepare(`EXPLAIN QUERY PLAN ${seek.sql}`).all(...seek.params).map((r) => r.detail).join("\n");
    assert.match(plan, /idx</);
    assert.doesNotMatch(plan, /TEMP B-TREE/);
  } finally { f.close(); }
});

test("randomized corpus agrees with independent literal/cyclic traversal in either direction", async () => {
  let seed = 7919;
  const random = () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 2 ** 32);
  const rows = Array.from({ length: 403 }, (_, i) => ({ id: (i + 1) * 3,
    idx: random() < 0.2 ? null : Math.floor(random() * 30) - 10,
    content: ["red", "green", "blue", "none", " "][Math.floor(random() * 5)],
    conversation: random() < 0.1 ? 2 : 1,
  }));
  const f = await fixture(rows);
  try {
    const ordered = sortedRows(rows);
    for (let n = 0; n < 60; n++) {
      const anchor = n % 7 === 0 ? null : ordered[Math.floor(random() * ordered.length)].id;
      const direction = random() < 0.5 ? 1 : -1;
      const text = ["red", "green", "blue", "missing", " "][n % 5];
      assert.deepEqual(plain(await f.source.findText(text, { afterMessageId: anchor, direction })),
        oracle(rows, text, anchor, direction));
    }
  } finally { f.close(); }
});

test("abort before or between pages rejects instead of reporting no matches", async () => {
  const f = await fixture(Array.from({ length: 400 }, (_, i) => ({ id: i + 1, idx: i, content: "x" })));
  try {
    const cancelled = new AbortController(); cancelled.abort();
    const before = f.queries.length;
    await assert.rejects(f.source.findText("none", { signal: cancelled.signal }), { name: "AbortError" });
    assert.equal(f.queries.length, before);
    const controller = new AbortController();
    const search = f.source.findText("none", { signal: controller.signal });
    setTimeout(() => controller.abort(), 0);
    await assert.rejects(search, { name: "AbortError" });
    assert(f.queries.filter((q) => q.sql.includes("AS matched")).length <= 2);
    assert.equal(f.openStatements, 0);
    assert.equal((await f.source.findText("x")).messageId, 1);
  } finally { f.close(); }
});

test("archive lock/replacement invalidates pending search before querying another generation", async () => {
  const f = await fixture(Array.from({ length: 300 }, (_, i) => ({ id: i + 1, idx: i, content: "x" })));
  try {
    const search = f.source.findText("none");
    const count = f.queries.length;
    f.api.closeDatabase();
    await assert.rejects(search, /no longer active/);
    assert.equal(f.queries.length, count);
    const replacement = { prepare() { throw new Error("must not query replacement"); } };
    f.api.installFixture(replacement);
    await assert.rejects(f.source.findText("x"), /no longer active/);
  } finally { f.close(); }
});

test("source disposal and progress errors reject without leaving statements active", async () => {
  const f = await fixture(Array.from({ length: 300 }, (_, i) => ({ id: i + 1, idx: i, content: "x" })));
  try {
    await assert.rejects(f.source.findText("none", { onProgress() { throw new Error("callback stopped"); } }), /callback stopped/);
    assert.equal(f.openStatements, 0);
    await assert.rejects(f.source.findText("none", { onProgress() { f.source.dispose(); } }), /no longer active/);
    assert.equal(f.openStatements, 0);
  } finally { f.close(); }
});

test("bad inputs and foreign/missing anchors are errors, never broadened searches", async () => {
  const f = await fixture(tricky);
  try {
    for (const text of ["", null, 12, "x".repeat(4097), "\ud800"]) {
      const before = f.queries.length;
      await assert.rejects(f.source.findText(text), /Find text/);
      assert.equal(f.queries.length, before);
    }
    for (const direction of [0, "1", NaN, 2]) await assert.rejects(f.source.findText("needle", { direction }), /direction/);
    for (const afterMessageId of [0, -1, 1.5, "1", Number.MAX_SAFE_INTEGER + 1]) {
      await assert.rejects(f.source.findText("needle", { afterMessageId }), /anchor/);
    }
    await assert.rejects(f.source.findText("needle", { afterMessageId: 2 }), /not in this conversation/);
    await assert.rejects(f.source.findText("needle", { afterMessageId: 10000 }), /not in this conversation/);
    await assert.rejects(f.source.findText("needle", { onProgress: 1 }), /callback/);
  } finally { f.close(); }
});

test("empty and single-message transcripts have finite, accurate wrap behavior", async () => {
  for (const rows of [[], [{ id: 9, idx: null, content: "sole" }]]) {
    const f = await fixture(rows);
    try {
      for (const direction of [1, -1]) {
        assert.deepEqual(plain(await f.source.findText("sole", { direction })), oracle(rows, "sole", null, direction));
        if (rows.length) {
          assert.equal((await f.source.findText("sole", { afterMessageId: 9, direction })).wrapped, true);
        }
      }
    } finally { f.close(); }
  }
});

test("malformed identities/order keys and truncated source fail explicitly", async () => {
  for (const row of [
    { id: 0, idx: 0, content: "x" }, { id: -1, idx: 0, content: "x" },
    { id: 1, idx: 0.5, content: "x" }, { id: 1, idx: "not-an-integer", content: "x" },
  ]) {
    const f = await fixture([row]);
    try { await assert.rejects(f.source.findText("x"), /Invalid message/); }
    finally { f.close(); }
  }
  const f = await fixture([{ id: 1, idx: 0, content: "x" }]);
  try {
    f.native.exec("DELETE FROM messages"); // simulates a broken supposedly immutable owner
    await assert.rejects(f.source.findText("x"), /changed while finding/);
  } finally { f.close(); }
});

test("a large individual message returns a match flag without populating the text cache", async () => {
  const f = await fixture([{ id: 1, idx: 0, content: "x".repeat(3 * 1024 * 1024) + "large-target" }]);
  try {
    assert.equal((await f.source.findText("large-target")).messageId, 1);
    assert.equal(f.source.getCacheStats().textUnits, 0);
    assert(f.queries.every((q) => !q.fields.has("content")));
  } finally { f.close(); }
});
