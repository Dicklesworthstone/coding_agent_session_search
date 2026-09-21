/**
 * Run: node --experimental-vm-modules --test tests/pages-message-source.test.mjs
 * Node >=22.13. Real SQLite with a sqlite-wasm statement adapter; no browser,
 * WASM runtime, DOM rendering or clipboard implementation is exercised here.
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../src/pages_assets/database.js", import.meta.url), "utf8");

async function fixture(t, count = 2003, content = (index) => `Message ${index}`, globals = {}) {
  const sqlite = new DatabaseSync(":memory:");
  t.after(() => sqlite.close());
  sqlite.exec(`
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, idx INTEGER,
            role TEXT, content TEXT, created_at INTEGER, updated_at INTEGER, model TEXT
        );
        CREATE INDEX message_order ON messages(conversation_id, idx, id);
        CREATE TABLE conversations (
            id INTEGER PRIMARY KEY, agent TEXT, workspace TEXT, title TEXT,
            source_path TEXT, started_at INTEGER, ended_at INTEGER,
            message_count INTEGER, metadata_json TEXT
        );
        INSERT INTO conversations VALUES (1, 'codex', '/test', 'Large session', '/test/log', 1000, NULL, 999999, NULL);
        INSERT INTO conversations VALUES (2, 'claude', '/other', 'Other session', '/other/log', 1000, NULL, 1, NULL);
    `);
  const insert = sqlite.prepare(
    "INSERT INTO messages VALUES (?, 1, ?, 'user', ?, NULL, NULL, NULL)",
  );
  for (let index = 0; index < count; index += 1) {
    // IDs are not ordinal positions; idx is sparse, tied, and sometimes NULL.
    insert.run(100 + index * 7, index < 2 ? null : Math.floor(index / 3) * 5, content(index));
  }
  sqlite.exec(
    "INSERT INTO messages VALUES (999999, 2, 0, 'user', 'Other conversation', NULL, NULL, NULL); PRAGMA query_only=ON",
  );
  const queries = [];
  const handle = {
    close() {},
    prepare(sql) {
      const statement = sqlite.prepare(sql);
      const record = { sql, params: [], rows: 0 };
      queries.push(record);
      let iterator;
      let row;
      return {
        bind(params) {
          record.params = Array.from(params);
        },
        step() {
          iterator ??= statement.iterate(...record.params);
          const next = iterator.next();
          row = next.value;
          if (!next.done) record.rows += 1;
          return !next.done;
        },
        get(column) {
          return typeof column === "number" ? Object.values(row)[column] : { ...row };
        },
        finalize() {
          iterator?.return();
        },
      };
    },
  };
  const context = vm.createContext({ console, URL, ...globals });
  const module = new vm.SourceTextModule(
    `${source}\nexport function attachTestDatabase(value) { db = value; }`,
    { context },
  );
  await module.link(() => {
    throw new Error("Unexpected static import");
  });
  await module.evaluate();
  module.namespace.attachTestDatabase(handle);
  const messages = new module.namespace.ConversationMessageSource(1);
  return { sqlite, queries, api: module.namespace, messages, handle, module, context };
}

function bodyQueries(queries) {
  return queries.filter((query) => /SELECT id, idx, role, content/.test(query.sql));
}

test("opening a long transcript reads no bodies; a distant row reads only its body", async (t) => {
  const { messages, queries } = await fixture(t);
  assert.equal(messages.length, 2003);
  assert.equal(queries.length, 1);
  assert.equal(bodyQueries(queries).length, 0);
  assert.equal(messages.get(2002).content, "Message 2002");
  assert.equal(bodyQueries(queries).length, 1);
  assert.equal(bodyQueries(queries)[0].rows, 1);
  assert.equal(queries[1].rows, 3, "last ID window contains only three IDs");
  assert.equal(messages.get(2003), undefined);
  assert.equal(messages.get(-1), undefined);
  assert.equal(messages.get(1.5), undefined);
  assert.equal(messages.get("1"), undefined);
  assert.equal(queries.length, 3, "invalid positions must not run SQL");
});

test("ID windows and immutable message bodies use bounded LRU caches", async (t) => {
  const { messages, queries } = await fixture(t);
  const first = messages.get(0);
  assert.equal(Object.isFrozen(first), true);
  messages.get(1);
  assert.equal(messages.get(0), first);
  assert.equal(bodyQueries(queries).length, 2);
  assert.equal(queries.filter((query) => query.sql.includes("LIMIT")).length, 1);
  for (let index = 2; index < 250; index += 1) messages.get(index);
  const stats = messages.getCacheStats();
  assert.equal(stats.idPages, 4);
  assert.equal(stats.messages, 64);
  const before = queries.length;
  assert.equal(messages.get(0).content, "Message 0");
  assert.equal(queries.length, before + 2, "evicted ID window and body are reloaded");
  assert.ok(
    queries.filter((query) => query.sql.includes("LIMIT")).every((query) => query.rows <= 50),
  );
});

test("deep links resolve sparse/tied/NULL indices without hydrating earlier bodies", async (t) => {
  const { messages, queries } = await fixture(t);
  for (const ordinal of [0, 1, 2, 3, 50, 999, 2002]) {
    assert.equal(messages.indexOfId(100 + ordinal * 7), ordinal);
  }
  assert.equal(messages.indexOfId(999999), -1, "foreign-conversation message must not resolve");
  assert.equal(messages.indexOfId(101), -1);
  assert.equal(messages.indexOfId("100"), -1);
  assert.equal(bodyQueries(queries).length, 0);
  assert.equal(messages.get(messages.indexOfId(7100)).id, 7100);
  assert.equal(bodyQueries(queries).length, 1);
});

test("explicit full traversal preserves every message without retaining every body", async (t) => {
  const { messages } = await fixture(t);
  const visited = [];
  messages.forEach((message, index) => visited.push([message.id, index]));
  assert.deepEqual(
    visited,
    Array.from({ length: 2003 }, (_, i) => [100 + 7 * i, i]),
  );
  assert.ok(messages.getCacheStats().messages <= 64);
  assert.ok(messages.getCacheStats().idPages <= 4);
  assert.deepEqual(Array.from(messages.map((message) => message.content)).slice(-3), [
    "Message 2000",
    "Message 2001",
    "Message 2002",
  ]);
});

test("retained text obeys its budget and oversized individual bodies are not cached", async (t) => {
  const { messages, queries } = await fixture(t, 30, (index) =>
    index === 29 ? "x".repeat(2 * 1024 * 1024) : "x".repeat(100000),
  );
  for (let i = 0; i < 29; i += 1) messages.get(i);
  const before = messages.getCacheStats();
  assert.ok(before.textUnits <= 2 * 1024 * 1024);
  assert.ok(before.messages < 29);
  assert.equal(messages.get(29).content.length, 2 * 1024 * 1024);
  assert.deepEqual(messages.getCacheStats(), before);
  const queryCount = queries.length;
  messages.get(29);
  assert.equal(queries.length, queryCount + 1, "uncached oversized body is read on demand");
});

test("clearing cache allows reuse; disposal fails closed without more SQL", async (t) => {
  const { messages, queries } = await fixture(t);
  messages.get(1000);
  messages.clearCache();
  assert.deepEqual({ ...messages.getCacheStats() }, { idPages: 0, messages: 0, textUnits: 0 });
  assert.equal(messages.get(1000).content, "Message 1000");
  messages.dispose();
  const before = queries.length;
  assert.throws(() => messages.length, /no longer active/);
  assert.throws(() => messages.get(0), /no longer active/);
  assert.throws(() => messages.indexOfId(100), /no longer active/);
  assert.throws(() => messages.forEach(() => {}), /no longer active/);
  assert.equal(queries.length, before);
});

test("database close/reopen cannot make an old source read a different archive", async (t) => {
  const { messages, queries, api, handle } = await fixture(t);
  messages.get(0);
  api.closeDatabase();
  api.attachTestDatabase(handle);
  const before = queries.length;
  assert.throws(() => messages.get(1), /no longer active/);
  assert.equal(queries.length, before);
  assert.equal(messages.getCacheStats().messages, 0);
  assert.equal(new api.ConversationMessageSource(1).get(1).content, "Message 1");
});

test("missing bodies fail visibly instead of returning a partially hydrated row", async (t) => {
  const { messages, sqlite } = await fixture(t);
  messages.get(0); // Cache the first ID window, but not the second body.
  sqlite.exec("PRAGMA query_only=OFF; DELETE FROM messages WHERE id=107; PRAGMA query_only=ON");
  assert.throws(() => messages.get(1), /missing from the archive/);
});

test("invalid conversation IDs are rejected before any database access", async (t) => {
  const { api, queries } = await fixture(t, 0);
  const before = queries.length;
  for (const id of [-1, 0, 1.5, "1", null, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => new api.ConversationMessageSource(id), /positive safe integer/);
  }
  assert.equal(queries.length, before);
  assert.equal(new api.ConversationMessageSource(3).length, 0);
});

// Controller integration uses DOM/virtual-list/clipboard doubles. Only the
// production database/source/viewer logic runs unchanged; this is not a browser
// rendering, markdown-sanitization, attachment-decryption or clipboard test.
async function viewerFixture(t, count = 2003) {
  const nodes = new Map();
  const windowListeners = new Map();
  const timers = new Map();
  const lists = [];
  const clipboard = [];
  const errors = [];
  let serial = 0;
  const later = (callback, delay = 0) => {
    const id = ++serial;
    timers.set(id, { callback, delay });
    return id;
  };
  class Element {
    constructor() {
      this.dataset = {};
      this.style = {};
      this.children = [];
      this.listeners = new Map();
      this.attributes = {};
      this.isConnected = true;
      this._html = "";
      this.classList = { add() {}, remove() {} };
    }
    set id(value) {
      this._id = value;
      nodes.set(value, this);
    }
    get id() {
      return this._id;
    }
    set innerHTML(value) {
      this._html = value;
      this.children = [];
    }
    get innerHTML() {
      return this._html;
    }
    set textContent(value) {
      this._text = value;
      this._html = String(value)
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;");
    }
    get textContent() {
      return this._text;
    }
    appendChild(child) {
      this.children.push(child);
    }
    setAttribute(name, value) {
      this.attributes[name] = value;
    }
    querySelector() {
      return new Element();
    }
    querySelectorAll() {
      return [];
    }
    addEventListener(type, callback) {
      this.listeners.set(type, callback);
    }
    remove() {
      nodes.delete(this.id);
    }
    scrollIntoView() {}
  }
  const container = new Element();
  const document = {
    createElement: () => new Element(),
    addEventListener() {},
    removeEventListener() {},
    getElementById(id) {
      if (
        !nodes.has(id) &&
        ["back-btn", "copy-btn", "error-back-btn", "messages-list"].includes(id)
      ) {
        const element = new Element();
        element.id = id;
      }
      return nodes.get(id) ?? null;
    },
  };
  const window = {
    DOMPurify: { sanitize: (html) => html },
    setTimeout: later,
    addEventListener(type, callback) {
      windowListeners.set(type, callback);
    },
    removeEventListener(type) {
      windowListeners.delete(type);
    },
  };
  const db = await fixture(t, count, undefined, {
    document,
    window,
    setTimeout: later,
    clearTimeout: (id) => timers.delete(id),
    requestAnimationFrame: (callback) => later(callback),
    console: {
      log() {},
      debug() {},
      warn() {},
      error(...args) {
        errors.push(args);
      },
    },
  });
  class VariableHeightVirtualList {
    constructor(options) {
      this.options = options;
      this.scrolls = [];
      this.destroyed = false;
      lists.push(this);
      // Simulate a five-message viewport, not all transcript messages.
      for (let index = 0; index < Math.min(5, options.totalCount); index += 1) {
        options.container.appendChild(options.renderItem(index));
      }
    }
    destroy() {
      this.destroyed = true;
    }
    scrollToIndex(index) {
      assert.equal(this.destroyed, false, "stale timer must not scroll a destroyed list");
      this.scrolls.push(index);
      this.options.container.appendChild(this.options.renderItem(index));
    }
  }
  const modules = { "./database.js": db.module };
  for (const [name, exports] of Object.entries({
    "./attachments.js": {
      createAttachmentElement() {},
      getMessageAttachments: () => [],
      initAttachments: async () => null,
      reset() {},
    },
    "./share.js": {
      copyTextToClipboard: async (text) => {
        clipboard.push(text);
        return true;
      },
    },
    "./virtual-list.js": { VariableHeightVirtualList },
  })) {
    modules[name] = new vm.SyntheticModule(
      Object.keys(exports),
      function () {
        for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
      },
      { context: db.context },
    );
  }
  const viewerSource = await readFile(
    new URL("../src/pages_assets/conversation.js", import.meta.url),
    "utf8",
  );
  const viewer = new vm.SourceTextModule(viewerSource, { context: db.context });
  await viewer.link((name) => {
    assert.ok(modules[name], `unexpected import: ${name}`);
    return modules[name];
  });
  await viewer.evaluate();
  viewer.namespace.initConversationViewer(container, () => {});
  return {
    ...db,
    viewer: viewer.namespace,
    container,
    nodes,
    lists,
    clipboard,
    errors,
    lock() {
      windowListeners.get("cass:lock")();
    },
    flush(delay = Infinity) {
      for (const [id, task] of [...timers]) {
        if (task.delay <= delay) {
          timers.delete(id);
          task.callback();
        }
      }
    },
  };
}

test("viewer hydrates only visible messages and jumps directly to a distant deep link", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1, 100 + 2002 * 7);
  assert.equal(f.errors.length, 0);
  assert.equal(f.lists[0].options.totalCount, 2003);
  assert.equal(bodyQueries(f.queries).length, 5);
  assert.equal(
    f.viewer.getCurrentConversation().message_count,
    2003,
    "actual count replaces stale metadata",
  );
  f.flush(100);
  assert.deepEqual(f.lists[0].scrolls, [2002]);
  assert.equal(bodyQueries(f.queries).length, 6);
  assert.equal(f.viewer.getCacheStats().cachedMessageBodies, 6);
});

test("late scrolling callbacks cannot read or scroll a replaced conversation", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1, 100 + 2002 * 7);
  const old = f.lists[0];
  await f.viewer.loadConversation(2);
  const before = f.queries.length;
  f.flush(100);
  old.options.renderItem(1000); // A queued virtual render from the old view.
  assert.equal(f.queries.length, before);
  assert.deepEqual(old.scrolls, []);
  assert.equal(f.viewer.getCurrentConversationId(), 2);
});

test("locking clears retained bodies and cancels pending deep-link rendering", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1, 100 + 2002 * 7);
  const old = f.lists[0];
  f.lock();
  const before = f.queries.length;
  f.flush(100);
  old.options.renderItem(1000);
  assert.equal(f.queries.length, before);
  assert.equal(f.viewer.getCurrentConversation(), null);
  assert.equal(f.viewer.getCacheStats().cachedCount, 0);
  assert.equal(f.viewer.getCacheStats().cachedMessageBodies, 0);
  assert.equal(f.container.innerHTML, "");
});

test("explicit Copy still includes the entire transcript, not just cached rows", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1);
  f.nodes.get("copy-btn").listeners.get("click")();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(f.clipboard.length, 1);
  assert.equal((f.clipboard[0].match(/^## User:/gm) ?? []).length, 2003);
  assert.match(f.clipboard[0], /Message 2002/);
  assert.ok(f.viewer.getCacheStats().cachedMessageBodies <= 64);
  assert.equal(f.errors.length, 0);
});

test("missing conversations clear active state and manual cache clearing preserves scrolling", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1);
  f.viewer.clearAllCache();
  assert.equal(f.viewer.getCacheStats().cachedCount, 0);
  const row = f.lists[0].options.renderItem(1000);
  assert.equal(row.dataset.messageId, 7100);
  await f.viewer.loadConversation(3);
  assert.equal(f.viewer.getCurrentConversation(), null);
  assert.match(f.container.innerHTML, /Conversation not found/);
});

test("late body-load failures render a visible error instead of breaking scrolling", async (t) => {
  const f = await viewerFixture(t);
  await f.viewer.loadConversation(1);
  f.sqlite.exec("PRAGMA query_only=OFF; DROP TABLE messages; PRAGMA query_only=ON");
  const notice = f.lists[0].options.renderItem(1000);
  assert.equal(notice.attributes.role, "alert");
  assert.match(notice.textContent, /Unable to load this message/);
  assert.equal(f.errors.length, 1);
});
