/** Run: node --experimental-vm-modules --test tests/pages/conversation-copy.test.mjs
 * Runs production conversation.js + share.js against real SQLite queries.
 * Database/DOM/clipboard boundaries are controlled adapters, not browser E2E.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";
import vm from "node:vm";

const conversationSource = readFileSync(process.env.CASS_CONVERSATION_SOURCE ||
  new URL("../../src/pages_assets/conversation.js", import.meta.url), "utf8");
const shareSource = readFileSync(new URL("../../src/pages_assets/share.js", import.meta.url), "utf8");
const MAX = 8 * 1024 * 1024;

async function fixture(t, { count = 301, native, onYield, failQuery, promiseClipboard = true, rejectItem = false } = {}) {
  const sql = new DatabaseSync(":memory:");
  sql.exec(`CREATE TABLE messages(id INTEGER PRIMARY KEY, conversation_id INTEGER,
    idx INTEGER, role TEXT, content TEXT);
    CREATE INDEX message_order ON messages(conversation_id, idx, id)`);
  const insert = sql.prepare("INSERT INTO messages VALUES (?, ?, ?, ?, ?)");
  const roles = ["user", "assistant", "system", "tool", "agent", "future-role"];
  for (let n = 0; n < count; n++) {
    insert.run(n * 3 + 1, 1, n < 150 ? null : Math.floor(n / 3), roles[n % roles.length], `body-${n} 🔑\0end`);
  }
  insert.run(count * 3 + 10, 2, 0, "user", "other conversation must never be copied");
  sql.exec("PRAGMA query_only=ON");
  const calls = [], copied = [], errors = [], timers = new Set();
  let rowsHydrated = 0, maxHydratedRows = 0, metadataRows = 0, sourceTraversals = 0, yields = 0, itemRequests = 0;
  let disposed = false;
  const eventTarget = () => {
    const handlers = new Map();
    return {
      addEventListener(type, handler) {
        if (!handlers.has(type)) handlers.set(type, new Set());
        handlers.get(type).add(handler);
      },
      removeEventListener(type, handler) { handlers.get(type)?.delete(handler); },
      dispatch(type) { for (const handler of [...(handlers.get(type) || [])]) handler({ type }); },
    };
  };
  const window = { ...eventTarget(), location: { href: "https://example.invalid/archive/" } };
  const button = { ...eventTarget(), dataset: {}, innerHTML: "Copy", textContent: "", disabled: false, isConnected: true };
  let fallbackCalls = 0;
  const document = { ...eventTarget(),
    getElementById(id) { return id === "copy-btn" ? button : null; },
    body: { appendChild(node) { node.parentNode = this; }, removeChild(node) { node.parentNode = null; } },
    createElement() { return { value: "", style: {}, setAttribute() {}, focus() {}, select() {} }; },
    execCommand() { fallbackCalls++; return true; },
  };
  const queryAll = (query, params = []) => {
    const call = { query, params: Array.from(params), rows: 0 }; calls.push(call);
    if (failQuery?.(query, calls)) throw new Error("injected read failure");
    const rows = sql.prepare(query).all(...params).map(row => ({ ...row }));
    call.rows = rows.length;
    if (query.includes("copy_content")) {
      rowsHydrated += rows.filter(row => typeof row.copy_content === "string").length;
      maxHydratedRows = Math.max(maxHydratedRows, rows.length);
    } else if (query.includes("content_bytes")) metadataRows += rows.length;
    return rows;
  };
  const source = {
    get length() { if (disposed) throw new Error("source disposed"); return count; },
    dispose() { disposed = true; }, clearCache() {},
    forEach(callback) {
      sourceTraversals++;
      for (const row of sql.prepare("SELECT * FROM messages WHERE conversation_id=1 ORDER BY idx,id").all()) {
        rowsHydrated++; callback(row);
      }
    },
  };
  const container = { innerHTML: "" };
  const tick = (callback, delay = 0) => {
    const timer = setTimeout(() => {
      timers.delete(timer);
      if (delay === 0) { yields++; onYield?.(f); }
      callback();
    }, delay);
    timer.unref?.(); timers.add(timer); return timer;
  };
  // Preparation tasks must keep Node alive while their promises are pending.
  const keepAlive = setInterval(() => {}, 1000);
  window.setTimeout = tick;
  const context = vm.createContext({ window, document, setTimeout: tick, clearTimeout,
    TextEncoder, URL, URLSearchParams, Blob,
    ...(promiseClipboard ? { ClipboardItem: class { constructor(data) { this.data = data; } } } : {}),
    navigator: { clipboard: {
      ...(promiseClipboard ? { write(items) {
        itemRequests++;
        assert.equal(yields, 0, "native write must begin inside the initial gesture");
        assert.equal(calls.length, 0, "native request precedes transcript SQL");
        if (rejectItem) return Promise.reject(new Error("item permission denied"));
        return Promise.resolve(items[0].data["text/plain"]).then(async blob => {
          copied.push(await blob.text());
          return native ? native(f) : undefined;
        });
      } } : {}),
      writeText(text) { copied.push(text); return native ? native(f) : Promise.resolve(); },
    } },
    console: { debug() {}, warn() {}, error(...values) { errors.push(values); } },
  });
  const synthetic = values => new vm.SyntheticModule(Object.keys(values), function () {
    for (const [key, value] of Object.entries(values)) this.setExport(key, value);
  }, { context });
  const router = synthetic({ buildConversationPath: () => "/c/1", buildSearchPath: () => "/search",
    parseConversationRouteParts: () => null, splitRouteQuery: value => value.split("?") });
  const share = new vm.SourceTextModule(shareSource, { context });
  await share.link(name => { assert.equal(name, "./router.js"); return router; });
  const deps = {
    "./database.js": synthetic({ ConversationMessageSource: class {}, checkMemoryPressure: () => false,
      getConversation: () => null, getMemoryUsage: () => null, queryAll }),
    "./attachments.js": synthetic({ createAttachmentElement() {}, getMessageAttachments: () => [],
      initAttachments: async () => ({}), reset() {} }),
    "./share.js": share,
    "./virtual-list.js": synthetic({ VariableHeightVirtualList: class {} }),
  };
  const injected = `\nexport { copyConversation as copyForTest, formatConversationAsText as formatForTest };
    export function activateForTest(conversation, messages, container) {
      activeConversationLoadId++;
      currentConversation = conversation; currentMessages = messages;
      initConversationViewer(container, () => clearViewer()); setupEventListeners();
    }`;
  const module = new vm.SourceTextModule(conversationSource + injected, { context });
  await module.link(name => { assert.ok(deps[name], name); return deps[name]; });
  await module.evaluate();
  const conv = { id: 1, title: "Test transcript", agent: "codex", workspace: "/work/project", started_at: null };
  module.namespace.activateForTest(conv, source, container);
  const f = { api: module.namespace, source, conv, calls, copied, errors, sql, button,
    lock: () => window.dispatch("cass:lock"),
    navigation: () => module.namespace.clearViewer(),
    stats: () => ({ rowsHydrated, metadataRows, maxHydratedRows, sourceTraversals, yields, fallbackCalls, itemRequests }),
    edit(operation) { sql.exec("PRAGMA query_only=OFF"); try { operation(sql); } finally { sql.exec("PRAGMA query_only=ON"); } },
  };
  t.after(() => { for (const timer of timers) clearTimeout(timer); clearInterval(keepAlive); sql.close(); });
  return f;
}

function expectedText(f) {
  const names = { user: "User", assistant: "Assistant", agent: "Assistant", system: "System", tool: "Tool" };
  return `# Test transcript\nAgent: codex\nDate: \nWorkspace: /work/project\n\n---\n` +
    f.sql.prepare("SELECT role,content FROM messages WHERE conversation_id=1 ORDER BY idx,id").all()
      .map(row => `\n## ${names[row.role] || "Unknown"}:\n\n${row.content ?? ""}\n\n---\n`).join("");
}

test("copy preserves every message, role and NULL/tied order key across bounded pages", async t => {
  const f = await fixture(t);
  await f.api.copyForTest();
  assert.deepEqual(f.copied, [expectedText(f)]);
  assert.ok(!f.copied[0].includes("other conversation"));
  const stats = f.stats();
  assert.equal(stats.metadataRows, 301); assert.equal(stats.rowsHydrated, 301);
  assert.ok(stats.maxHydratedRows <= 32); assert.equal(stats.sourceTraversals, 0);
  assert.ok(stats.yields > 10); assert.equal(f.button.disabled, false);
  assert.equal(f.button.innerHTML, "Copied!");
  assert.ok(f.calls.every(call => !/OFFSET/i.test(call.query)));
  assert.ok(f.calls.filter(call => call.query.includes("content_bytes")).every(call => call.rows <= 128));
  assert.throws(() => f.sql.exec("DELETE FROM messages"), /readonly/);
});

for (const variant of ["single giant body", "aggregate", "UTF-8 rather than code units"]) {
  test(`oversized ${variant} is refused before any body hydration or clipboard write`, async t => {
    const f = await fixture(t, { count: variant === "aggregate" ? 20 : 1 });
    f.edit(sql => sql.prepare("UPDATE messages SET content=? WHERE conversation_id=1").run(
      variant === "aggregate" ? "x".repeat(500_000) : variant === "single giant body" ?
        "x".repeat(MAX + 1) : "é".repeat(MAX / 2),
    ));
    await f.api.copyForTest();
    assert.equal(f.stats().rowsHydrated, 0); assert.equal(f.copied.length, 0);
    assert.equal(f.button.innerHTML, "Copy limit: 8 MiB"); assert.equal(f.button.disabled, false);
  });
}

test("oversized metadata and excessive message counts fail before SQL", async t => {
  const f = await fixture(t, { count: 1 });
  await assert.rejects(f.api.formatForTest({ ...f.conv, title: "x".repeat(MAX + 1) }, f.source), /8 MiB/);
  await assert.rejects(f.api.formatForTest(f.conv, { length: 100_001 }), /100k messages/);
  assert.equal(f.calls.length, 0); assert.equal(f.copied.length, 0);
});

test("invalid conversation identities and counts never reach SQL", async t => {
  const f = await fixture(t, { count: 1 });
  for (const value of [0, -1, "1 OR 1=1", Number.MAX_SAFE_INTEGER + 1, null]) {
    await assert.rejects(f.api.formatForTest({ ...f.conv, id: value }, f.source), /Invalid/);
  }
  for (const length of [-1, 1.5, NaN, Number.MAX_SAFE_INTEGER + 1]) {
    await assert.rejects(f.api.formatForTest(f.conv, { length }), /Invalid/);
  }
  assert.equal(f.calls.length, 0);
});

test("NULL bodies are empty text, while non-text SQLite values fail without copying", async t => {
  const f = await fixture(t, { count: 1 });
  f.edit(sql => sql.exec("UPDATE messages SET content=NULL WHERE conversation_id=1"));
  assert.equal(await f.api.formatForTest(f.conv, f.source), expectedText(f));
  f.edit(sql => sql.exec("UPDATE messages SET content=x'6162' WHERE conversation_id=1"));
  await assert.rejects(f.api.formatForTest(f.conv, f.source), /Invalid message content/);
  assert.equal(f.copied.length, 0);
});

for (const boundary of ["lock", "navigation"]) {
  test(`${boundary} before the initial yield cancels without reading or copying`, async t => {
    const f = await fixture(t);
    const pending = f.api.copyForTest(); f[boundary](); await pending;
    assert.equal(f.calls.length, 0); assert.equal(f.copied.length, 0);
  });
}

test("archive lock interrupts metadata measurement between pages", async t => {
  const f = await fixture(t, { onYield(f) { if (f.stats().metadataRows > 0) f.lock(); } });
  await f.api.copyForTest();
  assert.equal(f.stats().metadataRows, 128); assert.equal(f.stats().rowsHydrated, 0);
  assert.equal(f.copied.length, 0);
});

test("navigation interrupts body hydration between pages without partial output", async t => {
  const f = await fixture(t, { onYield(f) { if (f.stats().rowsHydrated > 0) f.navigation(); } });
  await f.api.copyForTest();
  assert.equal(f.stats().rowsHydrated, 32); assert.equal(f.copied.length, 0);
  assert.equal(f.button.innerHTML, "Copy", "obsolete work must not paint success or failure");
});

test("a disposed owning source aborts preparation before another database read", async t => {
  const f = await fixture(t, { onYield(f) { f.source.dispose(); } });
  await f.api.copyForTest();
  assert.equal(f.calls.length, 0); assert.equal(f.copied.length, 0);
  assert.equal(f.button.innerHTML, "Copy failed");
});

test("duplicate copy actions share one preparation rather than hydrating twice", async t => {
  const f = await fixture(t);
  await Promise.all([f.api.copyForTest(), f.api.copyForTest(), f.api.copyForTest()]);
  assert.equal(f.copied.length, 1); assert.equal(f.stats().rowsHydrated, 301);
  assert.equal(f.button.disabled, false);
});

test("database read failures never send a partially assembled transcript", async t => {
  let bodyQueries = 0;
  const f = await fixture(t, { failQuery(query) { return query.includes("copy_content") && ++bodyQueries === 2; } });
  await f.api.copyForTest();
  assert.equal(f.stats().rowsHydrated, 32); assert.equal(f.copied.length, 0);
  assert.equal(f.button.innerHTML, "Copy failed"); assert.equal(f.button.disabled, false);
});

test("missing rows cannot turn an incomplete inventory into a successful copy", async t => {
  const f = await fixture(t);
  f.edit(sql => sql.exec("DELETE FROM messages WHERE id=1"));
  await f.api.copyForTest();
  assert.equal(f.copied.length, 0); assert.equal(f.stats().rowsHydrated, 0);
  assert.equal(f.button.innerHTML, "Copy failed");
});

test("body growth after metadata measurement is refused by SQL before hydration", async t => {
  let edited = false;
  const f = await fixture(t, { count: 1, onYield(f) {
    if (!edited && f.stats().metadataRows === 1) {
      edited = true;
      f.edit(sql => sql.prepare("UPDATE messages SET content=? WHERE conversation_id=1").run("x".repeat(MAX + 1)));
    }
  } });
  await f.api.copyForTest();
  assert.equal(f.copied.length, 0); assert.equal(f.stats().rowsHydrated, 0);
  assert.equal(f.button.innerHTML, "Copy limit: 8 MiB");
});

for (const promiseClipboard of [true, false]) {
  for (const boundary of ["lock", "navigation"]) {
    test(`${promiseClipboard ? "promise item" : "shared text helper"} cannot retry after ${boundary} during clipboard wait`, async t => {
      let entered;
      const started = new Promise(resolve => { entered = resolve; });
      let reject;
      const pending = new Promise((_, no) => { reject = no; });
      const f = await fixture(t, { count: 1, promiseClipboard, native() { entered(); return pending; } });
      if (!promiseClipboard) await f.api.copyForTest(); // Prepare for a fresh legacy click.
      const copying = f.api.copyForTest();
      await started; f[boundary](); reject(new Error("permission denied")); await copying;
      assert.equal(f.copied.length, 1, "only the already-issued native write is observable");
      assert.equal(f.stats().fallbackCalls, 0);
      assert.notEqual(f.button.innerHTML, "Copied!");
    });
  }
}

test("the actual copy-button handler starts the bounded path", async t => {
  let entered;
  const copied = new Promise(resolve => { entered = resolve; });
  const f = await fixture(t, { count: 1, native() { entered(); return Promise.resolve(); } });
  f.button.dispatch("click"); await copied;
  assert.equal(f.stats().metadataRows, 1); assert.equal(f.stats().rowsHydrated, 1);
  assert.equal(f.copied[0], expectedText(f));
});

test("an 8193-message transcript is complete without filling the browsing body cache", async t => {
  const f = await fixture(t, { count: 8193 });
  await f.api.copyForTest();
  assert.deepEqual(f.copied, [expectedText(f)]);
  assert.equal(f.stats().metadataRows, 8193); assert.equal(f.stats().rowsHydrated, 8193);
  assert.equal(f.stats().sourceTraversals, 0); assert.ok(f.stats().maxHydratedRows <= 32);
  assert.ok(new TextEncoder().encode(f.copied[0]).byteLength <= MAX);
});

test("a large but admitted single message is not silently truncated", async t => {
  const f = await fixture(t, { count: 1 });
  f.edit(sql => sql.prepare("UPDATE messages SET content=? WHERE conversation_id=1").run("x".repeat(MAX - 1024)));
  await f.api.copyForTest();
  assert.deepEqual(f.copied, [expectedText(f)]);
  assert.equal(f.stats().rowsHydrated, 1);
});

for (const kind of ["legacy", "rejected item"]) {
  test(`${kind} clipboard gets prepared text only on a fresh explicit click`, async t => {
    const f = await fixture(t, { count: 301, promiseClipboard: kind !== "legacy", rejectItem: kind !== "legacy" });
    await f.api.copyForTest();
    assert.equal(f.copied.length, 0); assert.equal(f.stats().fallbackCalls, 0);
    assert.equal(f.button.innerHTML, "Ready — click to copy");
    const queries = f.calls.length;
    await f.api.copyForTest();
    assert.equal(f.calls.length, queries, "the second click must not repeat preparation");
    assert.deepEqual(f.copied, [expectedText(f)]);
    assert.equal(f.button.innerHTML, "Copied!");
  });
}

test("archive lock discards prepared fallback text instead of copying it later", async t => {
  const f = await fixture(t, { count: 1, promiseClipboard: false });
  await f.api.copyForTest();
  assert.equal(f.button.innerHTML, "Ready — click to copy");
  f.lock(); await f.api.copyForTest();
  assert.equal(f.copied.length, 0); assert.equal(f.stats().fallbackCalls, 0);
});
