// Run: node --test tests/pages/search-route-state.test.mjs (Node 22.13+).
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
const uiSource = await readFile(new URL("../../src/pages_assets/search.js", import.meta.url), "utf8");
const ui = await import(`data:text/javascript;base64,${Buffer.from(
  uiSource
    .replace('from "./database.js"', `from "${databaseUrl}"`)
    .replace('import { parseRouteIdSegment } from "./router.js";',
      'const parseRouteIdSegment = () => { throw new Error("Unexpected route-ID parsing"); };')
    .replace('import { VirtualList } from "./virtual-list.js";',
      'class VirtualList { constructor() { throw new Error("Unexpected virtual-list rendering"); } }')
    + "\nexport function setTestElements(value) { Object.assign(elements, value); }"
    + "\nexport { updateTimeFilter as setTestTimeFilter };",
).toString("base64")}`);

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

function fakeElement() {
  const classes = new Set();
  let html = "";
  let text = "";
  return {
    value: "", style: {}, dataset: {}, scrollTop: 0,
    get innerHTML() { return html; },
    set innerHTML(value) { html = value; },
    get textContent() { return text; },
    set textContent(value) {
      text = String(value);
      html = text.replace(/&/g, "&amp;").replace(/</g, "&lt;")
        .replace(/>/g, "&gt;").replace(/"/g, "&quot;");
    },
    classList: {
      add: (name) => classes.add(name), remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
      toggle: (name, enabled) => enabled ? classes.add(name) : classes.delete(name),
    },
    setAttribute() {},
  };
}

function prepareUi() {
  globalThis.document = { getElementById: () => null, createElement: () => fakeElement() };
  const elements = Object.fromEntries([
    "searchInput", "resultsList", "resultCount", "loadingIndicator", "noResults", "resultsContainer",
  ].map((name) => [name, fakeElement()]));
  ui.setTestElements(elements);
  ui.clearSearch({ reloadRecent: false });
  return elements;
}

test("invalid routes display an error and perform no unrestricted query", async () => {
  const elements = prepareUi();
  for (const filters of [
    { since: "bad" }, { until: true }, { since: 8000, until: 2000 },
    { time: "fortnight" }, { time: "week", since: "bad" },
    { time: "week", timePreset: "month" }, { time: "custom" },
  ]) {
    await ui.setSearchRoute({ q: "alpha", ...filters });
    assert.match(elements.resultsList.innerHTML, /role="alert"/);
    assert.equal(ui.getSearchState().resultCount, 0);
    assert.equal(typeof ui.getSearchState().filterError, "string");
  }
  assert.equal(prepares.length, 0);
});

test("editing the query cannot silently discard an invalid route filter", async () => {
  const elements = prepareUi();
  await ui.setSearchRoute({ q: "alpha", since: "bad" }, { runSearch: false });
  await ui.setSearchQuery("beta");
  assert.match(elements.resultsList.innerHTML, /since/);
  assert.equal(prepares.length, 0);
});

test("invalid route cancels pending valid results before they can paint", async () => {
  const elements = prepareUi();
  const pending = ui.setSearchQuery("alpha");
  await ui.setSearchRoute({ q: "beta", since: "bad" });
  await pending;
  assert.match(elements.resultsList.innerHTML, /role="alert"/);
  assert.equal(ui.getSearchState().isSearching, false);
  assert.equal(ui.getSearchState().resultCount, 0);
  assert.equal(prepares.length, 0);
});

test("a corrected route clears the guard and applies the requested range", async () => {
  prepareUi();
  await ui.setSearchRoute({ q: "alpha", since: "bad" });
  await ui.setSearchRoute({ q: "alpha", since: "2000", until: "4000" });
  assert.equal(ui.getSearchState().filterError, null);
  assert.equal(ui.getSearchState().resultCount, 2);
  assert.deepEqual(prepares.at(-1).params.slice(1, 3), [2000, 4000]);
});

test("explicitly choosing a new time filter or clearing search unblocks it", async () => {
  prepareUi();
  await ui.setSearchRoute({ since: "bad" }, { runSearch: false });
  ui.setTestTimeFilter("");
  await ui.setSearchQuery("alpha");
  assert.equal(ui.getSearchState().filterError, null);
  assert.equal(ui.getSearchState().resultCount > 0, true);
  await ui.setSearchRoute({ since: "bad" }, { runSearch: false });
  ui.clearSearch({ reloadRecent: false });
  assert.equal(ui.getSearchState().filterError, null);
});

test("selecting the custom placeholder does not discard custom bounds", async () => {
  prepareUi();
  await ui.setSearchRoute({ since: "2000", until: "4000" }, { runSearch: false });
  ui.setTestTimeFilter("custom");
  assert.equal(ui.getSearchState().filters.since, 2000);
  assert.equal(ui.getSearchState().filters.until, 4000);
});

test("parser errors are actionable in the UI, not a misleading empty result", async () => {
  const elements = prepareUi();
  await ui.setSearchQuery("alpha OR");
  assert.match(elements.resultsList.innerHTML, /Expected a search term/);
  assert.equal(prepares.length, 0);
});

test("a saved relative-filter snapshot keeps its explicit bounds", async () => {
  prepareUi();
  await ui.setSearchRoute({ time: "week", since: "2000", until: "4000" }, { runSearch: false });
  const filters = ui.getSearchState().filters;
  assert.deepEqual(filters, { agent: null, since: 2000, until: 4000, timePreset: "custom" });
  await ui.setSearchRoute(filters, { runSearch: false });
  assert.deepEqual(ui.getSearchState().filters, filters);
});

test("an unknown time selection cannot clear the invalid-filter guard", async () => {
  prepareUi();
  await ui.setSearchRoute({ since: "bad" }, { runSearch: false });
  assert.throws(() => ui.setTestTimeFilter("fortnight"), RangeError);
  assert.equal(typeof ui.getSearchState().filterError, "string");
  assert.equal(prepares.length, 0);
});

test("correcting a rejected time filter preserves the independent agent filter", async () => {
  prepareUi();
  await ui.setSearchRoute({ q: "alpha", agent: "codex", since: "bad" });
  assert.equal(ui.getSearchState().filters.agent, "codex");
  ui.setTestTimeFilter("");
  await ui.setSearchQuery("alpha");
  assert.equal(ui.getSearchState().resultCount > 0, true);
  assert.equal(prepares.at(-1).params[1], "codex");
});
