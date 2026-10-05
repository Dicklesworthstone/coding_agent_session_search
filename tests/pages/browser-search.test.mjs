// Run: node --test tests/pages/browser-search.test.mjs (Node 22.13+).
// Exercise the production SQL against real SQLite FTS5, not a canned query mock.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import { afterEach, beforeEach, test } from "node:test";

const source = await readFile(new URL("../../src/pages_assets/database.js", import.meta.url), "utf8");
// Expose only the existing handle-injection seam in this test's private module.
// Production exports and the browser's sqlite-wasm loader are unchanged.
const databaseUrl = `data:text/javascript;base64,${Buffer.from(
  source + "\nexport function setTestHandle(handle) { db = handle; }\n",
).toString("base64")}`;
const database = await import(databaseUrl);
let sqlite;
let prepares;
let finalizes;
const documents = [
  ...Array.from({ length: 8 }, (_, mask) => ({
    id: mask + 1,
    content: ["alpha", "beta", "gamma"].filter((_, bit) => mask & (1 << bit)).join(" ") || "zero",
  })),
  { id: 20, content: "disk full reboot" },
  { id: 21, content: "disk nearly full reboot" },
  { id: 22, content: "full disk" },
  { id: 23, content: 'say "hello world" politely' },
  { id: 24, content: "src/main.rs get_user_id user::value" },
  { id: 25, content: "src/other.rs get_user_id" },
  { id: 26, content: "authentication successful" },
  { id: 27, content: "author authorization" },
  { id: 28, content: "auth failure" },
  { id: 29, content: "authentication failed" },
  { id: 30, content: "alpha AND beta OR gamma NOT zero" },
];

beforeEach((context) => {
  // Expected UI validation failures should not dump embedded data-URL source.
  context.mock.method(console, "error", () => {});
  sqlite = new DatabaseSync(":memory:");
  sqlite.exec(`
    CREATE TABLE conversations (
      id INTEGER PRIMARY KEY, agent TEXT, workspace TEXT, title TEXT,
      source_path TEXT, started_at INTEGER, message_count INTEGER
    );
    CREATE TABLE messages (
      id INTEGER PRIMARY KEY, conversation_id INTEGER, role TEXT, content TEXT
    );
    CREATE VIRTUAL TABLE messages_fts USING fts5(content, tokenize = 'porter unicode61');
    CREATE VIRTUAL TABLE messages_code_fts USING fts5(content, tokenize = 'unicode61');
  `);
  for (const { id, content } of documents) {
    sqlite.prepare("INSERT INTO conversations VALUES (?, ?, ?, ?, ?, ?, ?)").run(
      id, id % 2 ? "claude" : "codex", "/workspace", `Conversation ${id}`, "/source", id * 1000, 1,
    );
    sqlite.prepare("INSERT INTO messages VALUES (?, ?, ?, ?)").run(id, id, "user", content);
    for (const table of ["messages_fts", "messages_code_fts"]) {
      sqlite.prepare(`INSERT INTO ${table}(rowid, content) VALUES (?, ?)`).run(id, content);
    }
  }
  prepares = [];
  finalizes = 0;
  database.setTestHandle({
    prepare(sql) {
      const record = { sql, params: [] };
      prepares.push(record);
      const statement = sqlite.prepare(sql);
      let rows;
      let current;
      return {
        bind(params) { record.params = params; },
        step() {
          rows ??= statement.iterate(...record.params);
          const next = rows.next();
          current = next.value;
          return !next.done;
        },
        get(key) {
          return typeof key === "number" ? Object.values(current)[key] : { ...current };
        },
        finalize() { rows?.return(); finalizes += 1; },
      };
    },
  });
});

afterEach(() => {
  database.setTestHandle(null);
  sqlite.close();
});

function ids(query, options = {}) {
  return database.searchConversations(query, { searchMode: "code", ...options })
    .map((row) => row.message_id).sort((a, b) => a - b);
}

function expected(predicate) {
  return documents.filter((doc) => predicate(new Set(doc.content.split(" "))))
    .map((doc) => doc.id).sort((a, b) => a - b);
}

test("ordinary multi-word search remains an intersection", () => {
  assert.deepEqual(ids("alpha beta"), expected((t) => t.has("alpha") && t.has("beta")));
});

test("quoted phrases require order and adjacency", () => {
  assert.deepEqual(ids('"disk full"'), [20]);
  assert.deepEqual(ids('"full disk"'), [22]);
  assert.deepEqual(ids('"disk full" reboot'), [20]);
});

test("prefix searches keep the star outside the quoted term", () => {
  assert.deepEqual(ids("auth*"), [26, 27, 28, 29]);
  assert.deepEqual(ids('"authentication succ"*'), [26]);
  assert.deepEqual(ids('"auth*"'), [28]);
});

test("OR, AND and binary NOT have explicit precedence", () => {
  assert.deepEqual(ids("alpha OR beta AND gamma"), expected((t) =>
    t.has("alpha") || (t.has("beta") && t.has("gamma"))));
  assert.deepEqual(ids("alpha NOT beta OR gamma"), expected((t) =>
    (t.has("alpha") && !t.has("beta")) || t.has("gamma")));
  assert.deepEqual(ids("alpha OR beta NOT gamma"), expected((t) =>
    t.has("alpha") || (t.has("beta") && !t.has("gamma"))));
});

test("parentheses and nested exclusions are not discarded", () => {
  assert.deepEqual(ids("(alpha OR beta) NOT gamma"), expected((t) =>
    (t.has("alpha") || t.has("beta")) && !t.has("gamma")));
  assert.deepEqual(ids("alpha NOT (beta OR gamma)"), expected((t) =>
    t.has("alpha") && !(t.has("beta") || t.has("gamma"))));
  assert.deepEqual(ids("alpha NOT (beta NOT gamma)"), expected((t) =>
    t.has("alpha") && !(t.has("beta") && !t.has("gamma"))));
});

test("implicit and explicit AND have the same grouping", () => {
  assert.deepEqual(ids("alpha (beta OR gamma)"), ids("alpha AND (beta OR gamma)"));
  assert.deepEqual(ids("alpha beta OR gamma"), ids("alpha AND beta OR gamma"));
  assert.deepEqual(ids("alpha NOT beta gamma"), ids("(alpha NOT beta) AND gamma"));
});

test("operators can still be searched literally and quotes can be doubled", () => {
  assert.deepEqual(ids('"AND"'), [30]);
  assert.deepEqual(ids("and"), [30]);
  assert.deepEqual(ids('"say ""hello world"" politely"'), [23]);
});

test("code punctuation stays a bound, quoted literal", () => {
  assert.deepEqual(ids("src/main.rs get_user_id"), [24]);
  ids("content:alpha");
  assert.match(prepares.at(-1).params[0], /"content:alpha"/);
  assert.equal(prepares.at(-1).sql.includes("content:alpha"), false);
  ids("alpha';DROP");
  assert.equal(sqlite.prepare("SELECT COUNT(*) AS n FROM messages").get().n, documents.length);
});

test("invalid syntax fails before touching SQLite rather than weakening the query", () => {
  for (const query of [
    '"disk full', "alpha OR", "AND alpha", "NOT alpha", "alpha OR NOT beta",
    "alpha AND OR beta", "()", "(alpha", "alpha)", '""', "*", "auth**",
  ]) {
    const count = prepares.length;
    assert.throws(() => ids(query), SyntaxError, query);
    assert.equal(prepares.length, count, query);
  }
});

test("query size and nesting budgets fail predictably", () => {
  assert.throws(() => ids("a".repeat(4097)), RangeError);
  assert.throws(() => ids("a ".repeat(129)), RangeError);
  assert.throws(() => ids("(".repeat(17) + "alpha" + ")".repeat(17)), RangeError);
  assert.equal(prepares.length, 0);
});

test("empty input returns no results and non-string inputs are rejected", () => {
  assert.deepEqual(ids(" \n\t "), []);
  for (const value of [null, undefined, {}, 42, ["alpha"]]) {
    assert.throws(() => ids(value), TypeError);
  }
});

test("boolean search preserves filters, stable pagination and statement cleanup", () => {
  const options = { agent: "codex", since: 2000, until: 8000 };
  const whole = database.searchConversations("alpha OR beta", { ...options, limit: 100 });
  const pages = [];
  for (let offset = 0; offset < whole.length; offset += 2) {
    pages.push(...database.searchConversations("alpha OR beta", { ...options, limit: 2, offset }));
  }
  assert.deepEqual(pages, whole);
  assert.equal(whole.length > 0, true);
  assert.equal(whole.every((r) => r.agent === "codex" && r.started_at >= 2000 && r.started_at <= 8000), true);
  assert.equal(finalizes, prepares.length);
});

test("grouped truth-table oracle covers all three boolean operators", () => {
  const ops = ["AND", "OR", "NOT"];
  const apply = (op, a, b) => op === "AND" ? a && b : op === "OR" ? a || b : a && !b;
  for (const left of ops) {
    for (const right of ops) {
      assert.deepEqual(ids(`(alpha ${left} beta) ${right} gamma`), expected((t) =>
        apply(right, apply(left, t.has("alpha"), t.has("beta")), t.has("gamma"))));
      assert.deepEqual(ids(`alpha ${left} (beta ${right} gamma)`), expected((t) =>
        apply(left, t.has("alpha"), apply(right, t.has("beta"), t.has("gamma")))));
    }
  }
});

test("invalid timestamp values never become absent filters", () => {
  for (const value of ["nonsense", " ", -1, 0.5, NaN, Infinity, true, false, [], {}, "0x10", "1e3", "1.5", "9007199254740992"]) {
    for (const bound of ["since", "until"]) {
      assert.throws(() => ids("alpha", { [bound]: value }), RangeError);
    }
    assert.throws(() => database.getConversationsByAgent("claude", 50, value), RangeError);
    assert.throws(() => database.getConversationsByTimeRange(null, value), RangeError);
  }
  assert.equal(prepares.length, 0);
});

test("reversed ranges are rejected by every query entry point", () => {
  assert.throws(() => ids("alpha", { since: 8000, until: 2000 }), /since.*until/);
  assert.throws(() => database.getConversationsByAgent("claude", 50, 8000, 2000), /since.*until/);
  assert.throws(() => database.getConversationsByTimeRange(8000, 2000), /since.*until/);
  assert.throws(() => ids("", { since: "broken" }), RangeError);
  assert.equal(prepares.length, 0);
});

test("omitted bounds and zero remain distinct; endpoints are inclusive", () => {
  assert.deepEqual(database.normalizeSearchTimeRange(), { since: null, until: null });
  assert.deepEqual(database.normalizeSearchTimeRange("0", " 2000 "), { since: 0, until: 2000 });
  assert.deepEqual(database.getConversationsByTimeRange("2000", "2000").map((r) => r.id), [2]);
  assert.deepEqual(database.getConversationsByTimeRange(null, 0), []);
  assert.deepEqual(database.getConversationsByTimeRange(0, "").map((r) => r.id).sort((a, b) => a - b), documents.map((d) => d.id));
});

test("date-only ranges include the entire UTC end date", () => {
  const start = Date.UTC(2026, 9, 4);
  const end = start + 86400000 - 1;
  assert.deepEqual(database.normalizeSearchTimeRange("2026-10-04", "2026-10-04"), { since: start, until: end });
  for (const [id, timestamp] of [[2, start - 1], [3, start], [4, end], [5, end + 1]]) {
    sqlite.prepare("UPDATE conversations SET started_at = ? WHERE id = ?").run(timestamp, id);
  }
  assert.deepEqual(database.getConversationsByTimeRange("2026-10-04", "2026-10-04").map((r) => r.id).sort(), [3, 4]);
});

test("calendar rollover and ambiguous timestamp text are rejected", () => {
  for (const value of ["2025-02-29", "2026-02-30", "2026-13-01", "2026-04-31", "1969-12-31", "10/04/2026", "2026-10-04T00:00:00"]) {
    assert.throws(() => database.normalizeSearchTimeRange(value), RangeError, value);
  }
  assert.equal(database.normalizeSearchTimeRange("2024-02-29").since, Date.UTC(2024, 1, 29));
});

test("deterministically generated nested queries match a Boolean oracle in both indexes", () => {
  let seed = 0x5eed;
  const next = () => (seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0);
  const names = ["alpha", "beta", "gamma"];
  const ops = ["AND", "OR", "NOT"];
  function make(depth) {
    if (!depth || next() % 4 === 0) {
      const term = names[next() % names.length];
      return { query: term, matches: (t) => t.has(term) };
    }
    const left = make(depth - 1);
    const right = make(depth - 1);
    const op = ops[next() % ops.length];
    return {
      query: `(${left.query} ${op} ${right.query})`,
      matches: (t) => op === "AND" ? left.matches(t) && right.matches(t)
        : op === "OR" ? left.matches(t) || right.matches(t) : left.matches(t) && !right.matches(t),
    };
  }
  for (let index = 0; index < 128; index += 1) {
    const expression = make(4);
    const matches = expected(expression.matches);
    assert.deepEqual(ids(expression.query, { searchMode: "code" }), matches, expression.query);
    assert.deepEqual(ids(expression.query, { searchMode: "prose" }), matches, expression.query);
  }
});

