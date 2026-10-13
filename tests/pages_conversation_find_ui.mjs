/** Actual viewer/controller code with a minimal event DOM and controlled source.
 * Run: node --experimental-vm-modules --test tests/pages_conversation_find_ui.mjs
 * Browser layout, the conversation renderer and sqlite-wasm are not mocked into
 * passing claims: they are outside this controller test. Backend SQL is exercised
 * separately by pages_conversation_find.mjs against real SQLite.
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { DatabaseSync } from "node:sqlite";
import vm from "node:vm";

const sourceText = await readFile(new URL("../src/pages_assets/viewer.js", import.meta.url), "utf8");

class Events {
  listeners = new Map();
  addEventListener(type, fn, capture = false) {
    const entries = this.listeners.get(type) ?? [];
    if (!entries.some((e) => e.fn === fn && e.capture === capture)) entries.push({ fn, capture });
    this.listeners.set(type, entries);
  }
  removeEventListener(type, fn, capture = false) {
    this.listeners.set(type, (this.listeners.get(type) ?? []).filter((e) => e.fn !== fn || e.capture !== capture));
  }
  fire(type, props = {}) {
    const event = { target: this, defaultPrevented: false, stopped: false,
      preventDefault() { this.defaultPrevented = true; },
      stopPropagation() { this.stopped = true; }, ...props };
    for (const { fn } of this.listeners.get(type) ?? []) fn(event);
    return event;
  }
  dispatchEvent(event) { return this.fire(event.type, event); }
}

class Element extends Events {
  constructor(document, id = "") {
    super();
    this.document = document;
    this.id = id;
    this.value = "";
    this.textContent = "";
    this.attrs = new Map();
    this.dataset = {};
    this.classes = new Set();
    this.classList = {
      add: (...names) => names.forEach((name) => this.classes.add(name)),
      remove: (...names) => names.forEach((name) => this.classes.delete(name)),
      contains: (name) => this.classes.has(name),
      toggle: (name, force = !this.classes.has(name)) => {
        if (force) this.classes.add(name); else this.classes.delete(name);
        return force;
      },
    };
  }
  set innerHTML(value) {
    this.html = value;
    // Establish IDs/classes only; there is deliberately no layout simulation.
    for (const match of value.matchAll(/<[^>]*\bid="([^"]+)"[^>]*>/g)) {
      const element = new Element(this.document, match[1]);
      const classes = match[0].match(/class="([^"]*)"/)?.[1] ?? "";
      element.classList.add(...classes.split(/\s+/).filter(Boolean));
      this.document.nodes.set(element.id, element);
    }
  }
  get innerHTML() { return this.html ?? this.textContent.replaceAll("&", "&amp;").replaceAll("<", "&lt;"); }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  removeAttribute(name) { this.attrs.delete(name); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  querySelectorAll() { return []; }
  focus() { this.document.activeElement = this; }
  select() { this.selected = true; }
  closest(selector) { return selector === "[data-find-action]" && this.dataset.findAction ? this : null; }
  appendChild() {}
  remove() {}
}

function deferred() {
  let resolve, reject;
  const promise = new Promise((a, b) => { resolve = a; reject = b; });
  return { promise, resolve, reject };
}

async function fixture(realRows = null) {
  const document = { nodes: new Map(), activeElement: null,
    getElementById(id) { return this.nodes.get(id) ?? null; },
    createElement() { return new Element(this); },
  };
  for (const id of ["app-content", "root", "body"]) document.nodes.set(id, new Element(document, id));
  document.documentElement = document.nodes.get("root");
  document.body = document.nodes.get("body");
  const window = new Events();
  window.location = { hash: "#/", href: "https://example.invalid/archive/#/" };
  window.history = { replaceState(_state, _unused, href) {
    window.location.href = href;
    window.location.hash = new URL(href).hash;
  } };
  let ready = true;
  let onNavigate;
  let throwNavigation = false;
  const pendingRoutes = [];
  const routes = [];
  const calls = [];
  const sources = [];
  const rendered = [];
  let currentConversation = null;
  const logs = [];
  const stubRouter = {
    navigate(path, options) {
      if (throwNavigation) throw new Error("router failure");
      routes.push({ path, options });
      // Match production: hash changes synchronously, the route event follows.
      window.location.hash = `#${path}`;
      pendingRoutes.push(path);
    },
    goToConversation(id, messageId) { this.navigate(messageId ? `/c/${id}/m/${messageId}` : `/c/${id}`); },
    destroy() {},
  };
  const navigate = (view, conversationId = null, messageId = null) => {
    const path = view === "conversation" ? `/c/${conversationId}${messageId ? `/m/${messageId}` : ""}` : `/${view}`;
    window.location.hash = `#${path}`;
    onNavigate({ view, params: { conversationId, messageId, path }, query: {}, raw: path });
  };
  class Source {
    constructor(id) { this.id = id; this.disposed = false; sources.push(this); }
    findText(text, options) {
      const d = deferred();
      calls.push({ ...d, source: this, text, options });
      return d.promise;
    }
    dispose() { this.disposed = true; }
  }
  const noOp = () => {};
  const imports = {
    "./conversation.js": { cleanupConversationViewer: () => { currentConversation = null; },
      clearViewer: () => { currentConversation = null; }, getCurrentConversation: () => currentConversation,
      initConversationViewer: noOp, loadConversation: (id, message) => {
        currentConversation = { id };
        rendered.push({ id, message });
        document.getElementById("conversation-view").innerHTML = "<article>rendered message</article>";
        return Promise.resolve();
      } },
    "./database.js": { ConversationMessageSource: Source, closeDatabase: () => { ready = false; },
      getStatistics: () => ({ conversations: 2, messages: 1000, agents: [] }), isDatabaseReady: () => ready },
    "./router.js": { buildConversationPath: (id, m) => `/c/${id}${m ? `/m/${m}` : ""}`,
      buildSearchPath: () => "/search", createRouter: (options) => {
        onNavigate = options.onNavigate; navigate("search"); return stubRouter;
      }, getRouter: () => stubRouter,
      parseSearchParams: () => ({ query: "", agent: null, since: null, until: null, timePreset: null }) },
    "./search.js": { clearSearch: noOp, getSearchState: () => ({ query: "", filters: {} }),
      initSearch: noOp, setSearchRoute: async () => {} },
    "./settings.js": { cleanupSettings: noOp, initSettings: async () => {}, render: async () => {} },
    "./share.js": { copyConversationLink: async () => ({ success: true }), getConversationLink: (id, m) => `${id}/${m}`,
      isWebShareAvailable: () => false, shareConversation: async () => false },
    "./stats.js": { clearStatsCache: noOp, initStats: noOp, renderStatsDashboard: noOp },
    "./storage.js": { initStorage: async () => {}, StorageKeys: { THEME: "theme" } },
  };
  const context = vm.createContext({ document, window, AbortController, URL, setTimeout,
    console: Object.fromEntries(["log", "debug", "warn", "error"].map((key) => [key, (...args) => logs.push(args)])),
    localStorage: { getItem: () => null }, CustomEvent: class { constructor(type, options) { this.type = type; this.detail = options.detail; } },
  });
  const sqlReads = [];
  if (realRows) {
    const native = new DatabaseSync(":memory:");
    native.exec(`CREATE TABLE messages (
      id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, idx INTEGER, content TEXT);
      CREATE INDEX message_order ON messages(conversation_id, idx);`);
    const insert = native.prepare("INSERT INTO messages VALUES(?, ?, ?, ?)");
    for (const row of realRows) insert.run(row.id, row.conversation, row.idx, row.content);
    const handle = {
      prepare(sql) {
        const statement = native.prepare(sql);
        const log = { sql, params: [], rows: [] };
        sqlReads.push(log);
        let iterator, row;
        return {
          bind(params) { log.params = params; },
          step() {
            iterator ??= statement.iterate(...log.params);
            const next = iterator.next(); row = next.value;
            if (!next.done) log.rows.push({ ...row });
            return !next.done;
          },
          get(key) { return typeof key === "number" ? Object.values(row)[key] : { ...row }; },
          finalize() { iterator?.return(); },
        };
      },
      close() { native.close(); },
    };
    const databaseText = await readFile(new URL("../src/pages_assets/database.js", import.meta.url), "utf8");
    const dbModule = new vm.SourceTextModule(databaseText + `
      export function installFixture(handle) { db = handle; isInitialized = true; lifecycleGeneration++; }
    `, { context });
    await dbModule.link(() => { throw new Error("unexpected database import"); });
    await dbModule.evaluate();
    dbModule.namespace.installFixture(handle);
    for (const name of ["ConversationMessageSource", "closeDatabase", "isDatabaseReady"]) {
      imports["./database.js"][name] = dbModule.namespace[name];
    }
  }
  const module = new vm.SourceTextModule(sourceText + `
    export const findTest = { runTranscriptFind, openTranscriptFind, closeTranscriptFind };
  `, { context });
  await module.link(async (specifier) => {
    const values = imports[specifier];
    assert(values, `unexpected dependency ${specifier}`);
    return new vm.SyntheticModule(Object.keys(values), function () {
      for (const [key, value] of Object.entries(values)) this.setExport(key, value);
    }, { context });
  });
  await module.evaluate();
  const api = module.namespace;
  api.init();
  await Promise.resolve();
  const node = (id) => document.getElementById(`transcript-find${id ? `-${id}` : ""}`);
  return { api, window, document, calls, sources, routes, rendered, logs, navigate, node, sqlReads,
    input(text) { node("input").value = text; node("input").fire("input"); },
    click(action) { const button = new Element(document); button.dataset.findAction = action;
      return node("").fire("click", { target: button }); },
    key(key, options = {}) { return window.fire("keydown", { key, target: document.activeElement,
      ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, isComposing: false, ...options }); },
    flushRoute() {
      const path = pendingRoutes.shift();
      if (!path) return;
      const [_, __, id, ___, messageId] = path.split("/");
      navigate("conversation", Number(id), messageId ? Number(messageId) : null);
    },
    breakNavigation() { throwNavigation = true; },
    pendingConversation() { currentConversation = null; },
    unlock() { ready = true; window.fire("cass:db-ready"); api.init(); },
    close() { api.cleanup(); },
  };
}

const found = (messageId = 812, index = 900, wrapped = false) => ({ messageId, index, total: 1000, checked: 128, wrapped });
const settle = async () => { await Promise.resolve(); await Promise.resolve(); };

test("find bar is mounted on real viewer initialization and keyboard-scoped to conversations", async () => {
  const f = await fixture();
  try {
    assert(f.node("").classList.contains("hidden"));
    assert.equal(f.key("f", { ctrlKey: true }).defaultPrevented, false);
    f.navigate("conversation", 7);
    assert(!f.node("").classList.contains("hidden"));
    assert(f.node("form").classList.contains("hidden"));
    const open = f.key("f", { metaKey: true });
    assert(open.defaultPrevented && open.stopped);
    assert.equal(f.document.activeElement, f.node("input"));
    assert.equal(f.node("open").getAttribute("aria-expanded"), "true");
    assert.match(f.node("status").textContent, /case-sensitive/);
    assert.equal(f.sources.length, 0, "opening controls must not read the archive");
    for (const options of [{ ctrlKey: true, shiftKey: true }, { ctrlKey: true, altKey: true },
      { ctrlKey: true, isComposing: true }, { ctrlKey: true, defaultPrevented: true }]) {
      const e = f.key("f", options);
      assert.equal(e.stopped, false);
    }
  } finally { f.close(); }
});

test("buttons and submit navigate matched message IDs and retain focus/query across rerender", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7);
    f.click("open");
    const input = f.node("input");
    f.input("private exact text");
    assert(f.node("form").fire("submit").defaultPrevented);
    assert.equal(f.calls[0].options.afterMessageId, null);
    f.calls[0].options.onProgress({ checked: 128, total: 1000 });
    assert.match(f.node("status").textContent, /128 of 1000/);
    f.calls[0].resolve(found());
    await settle();
    assert.deepEqual(f.routes.map((r) => r.path), ["/c/7/m/812"]);
    assert.equal(f.routes[0].options.replace, true);
    f.flushRoute();
    assert.deepEqual(f.rendered.at(-1), { id: 7, message: 812 });
    assert.equal(f.node("input"), input);
    assert.equal(input.value, "private exact text");
    assert.equal(f.document.activeElement, input);
    assert.match(f.node("status").textContent, /Found message 901 of 1000/);
    assert.equal(f.api.getCurrentShareLink(), "7/812");
    assert.equal(f.node("form").getAttribute("aria-busy"), "false");
    f.click("previous");
    assert.equal(f.calls[1].options.direction, -1);
    assert.equal(f.calls[1].options.afterMessageId, 812);
    f.calls[1].resolve(found(16, 3, true));
    await settle();
    assert.match(f.node("status").textContent, /message 4 of 1000 \(wrapped\)/);
    assert(!JSON.stringify(f.routes).includes("private exact text"));
    assert(!JSON.stringify(f.logs).includes("private exact text"));
  } finally { f.close(); }
});

test("a sole match scrolls again even when no hash-change event would occur", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7, 812);
    f.click("open"); f.input("sole");
    const operation = f.api.findTest.runTranscriptFind(1);
    f.calls[0].resolve(found()); await operation;
    assert.equal(f.routes.length, 0);
    assert.equal(f.rendered.length, 2);
    assert.deepEqual(f.rendered.at(-1), { id: 7, message: 812 });
  } finally { f.close(); }
});

test("Enter, Shift+Enter and F3 dispatch once and Escape cancels without navigating back", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("text");
    assert(f.key("Enter").defaultPrevented);
    assert.equal(f.calls.length, 1);
    assert.equal(f.calls[0].options.direction, 1);
    assert(f.key("Enter", { shiftKey: true }).defaultPrevented);
    assert(f.calls[0].options.signal.aborted);
    assert.equal(f.calls[1].options.direction, -1);
    f.key("F3"); assert.equal(f.calls[2].options.direction, 1);
    f.key("F3", { shiftKey: true }); assert.equal(f.calls[3].options.direction, -1);
    const close = f.key("Escape");
    assert(close.defaultPrevented && close.stopped);
    assert.equal(f.api.getState().view, "conversation");
    assert.equal(f.node("input").value, "");
    assert(f.node("form").classList.contains("hidden"));
    assert(f.calls.every((call) => call.options.signal.aborted));
    for (const call of f.calls) call.resolve(found());
    await settle(); assert.equal(f.routes.length, 0);
    assert.equal(f.document.activeElement, f.node("open"));
    assert.equal(f.key("Escape").defaultPrevented, false);
  } finally { f.close(); }
});

test("changed input suppresses stale progress, success and rejection from an earlier request", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("old query");
    const first = f.api.findTest.runTranscriptFind(1);
    f.input("new query");
    assert(f.calls[0].options.signal.aborted);
    f.calls[0].options.onProgress({ checked: 999, total: 1000 });
    assert.doesNotMatch(f.node("status").textContent, /999/);
    const next = f.api.findTest.runTranscriptFind(1);
    assert.equal(f.calls[1].options.afterMessageId, null);
    f.calls[0].resolve(found(1, 0)); await first;
    assert.equal(f.routes.length, 0);
    assert.equal(f.node("form").getAttribute("aria-busy"), "true");
    f.calls[1].resolve(found(2, 1)); await next;
    assert.deepEqual(f.routes.map((r) => r.path), ["/c/7/m/2"]);
    f.flushRoute();
    const stale = f.api.findTest.runTranscriptFind(1);
    f.input("third query");
    f.calls[2].reject(new Error("private old query failure")); await stale;
    assert.match(f.node("status").textContent, /case-sensitive/);
    assert(!JSON.stringify(f.logs).includes("private old query failure"));
  } finally { f.close(); }
});

test("leaving the conversation clears private state and aborts pending work for every route", async () => {
  for (const route of ["search", "settings", "stats", "not-found"]) {
    const f = await fixture();
    try {
      f.navigate("conversation", 7); f.click("open"); f.input("private");
      const operation = f.api.findTest.runTranscriptFind(1);
      f.navigate(route);
      assert(f.calls[0].options.signal.aborted);
      assert(f.sources[0].disposed);
      assert.equal(f.node("input").value, "");
      assert(f.node("").classList.contains("hidden"));
      f.calls[0].resolve(found()); await operation;
      assert.equal(f.routes.length, 0);
      assert.equal(f.key("F3").defaultPrevented, false);
    } finally { f.close(); }
  }
});

test("changing conversation cannot reuse an old source, needle or anchor", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("private");
    const old = f.api.findTest.runTranscriptFind(1);
    f.navigate("conversation", 8, 20);
    assert(f.sources[0].disposed);
    assert.equal(f.node("input").value, "");
    f.calls[0].resolve(found()); await old;
    assert.equal(f.routes.length, 0);
    f.click("open"); f.input("other");
    const fresh = f.api.findTest.runTranscriptFind(-1);
    assert.equal(f.sources[1].id, 8);
    assert.equal(f.calls[1].options.afterMessageId, null);
    f.calls[1].resolve(found(22, 21)); await fresh;
    assert.equal(f.routes.at(-1).path, "/c/8/m/22");
  } finally { f.close(); }
});

test("a hash change not yet delivered to the router suppresses late completion", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("private");
    const operation = f.api.findTest.runTranscriptFind(1);
    f.window.location.hash = "#/settings";
    f.calls[0].resolve(found()); await operation;
    assert.equal(f.routes.length, 0);
    assert.equal(f.node("form").getAttribute("aria-busy"), "false");
  } finally { f.close(); }
});

test("same-conversation navigation cancels a request without losing the find controls", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("text");
    const operation = f.api.findTest.runTranscriptFind(1);
    f.navigate("conversation", 7, 6);
    assert(f.calls[0].options.signal.aborted);
    f.calls[0].resolve(found()); await operation;
    assert.match(f.node("status").textContent, /cancelled by navigation/);
    assert.equal(f.node("input").value, "text");
    const again = f.api.findTest.runTranscriptFind(1);
    assert.equal(f.calls[1].options.afterMessageId, 6);
    f.calls[1].resolve(found()); await again;
  } finally { f.close(); }
});

test("lock and cleanup clear controls, remove listeners, and fence late results across reinitialization", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("private");
    const oldInput = f.node("input"), oldForm = f.node("form");
    const operation = f.api.findTest.runTranscriptFind(1);
    f.window.fire("cass:lock", { detail: { source: "auth" } });
    assert(f.calls[0].options.signal.aborted);
    assert(f.sources[0].disposed);
    assert.equal(oldInput.value, "");
    assert.equal(oldForm.listeners.get("submit").length, 0);
    assert.equal(f.window.listeners.get("keydown").length, 0);
    f.unlock();
    f.navigate("conversation", 7); f.click("open"); f.input("new owner");
    const newer = f.api.findTest.runTranscriptFind(1);
    f.calls[0].resolve(found()); await operation;
    assert.equal(f.routes.length, 0);
    f.calls[1].resolve(found(25, 24)); await newer;
    assert.equal(f.routes.at(-1).path, "/c/7/m/25");
    assert.equal(f.window.listeners.get("keydown").length, 1);
  } finally { f.close(); }
});

test("no match, invalid input, backend failure and navigation failure stay distinguishable", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open");
    await f.api.findTest.runTranscriptFind(1);
    assert.equal(f.calls.length, 0);
    assert.match(f.node("status").textContent, /Enter text/);
    f.input("nothing");
    const empty = f.api.findTest.runTranscriptFind(1);
    f.calls[0].resolve({ messageId: null, index: -1, total: 1000 }); await empty;
    assert.match(f.node("status").textContent, /No matching messages.*all 1000/);
    const error = f.api.findTest.runTranscriptFind(1);
    f.calls[1].reject(new Error("private database detail")); await error;
    assert.match(f.node("status").textContent, /Could not search/);
    assert.doesNotMatch(f.node("status").textContent, /No matching/);
    assert(f.sources[0].disposed);
    assert(!JSON.stringify(f.logs).includes("private database detail"));
    const retry = f.api.findTest.runTranscriptFind(1);
    assert.equal(f.sources.length, 2);
    f.breakNavigation();
    f.calls[2].resolve(found()); await retry;
    assert.match(f.node("status").textContent, /navigation failed/);
    assert.equal(f.node("form").getAttribute("aria-busy"), "false");
  } finally { f.close(); }
});

test("close button erases the query and releases the source; reopening starts a fresh find", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("  literal whitespace  ");
    const operation = f.api.findTest.runTranscriptFind(1);
    assert.equal(f.calls[0].text, "  literal whitespace  ");
    f.click("close");
    f.calls[0].resolve(found()); await operation;
    assert(f.sources[0].disposed);
    assert.equal(f.node("input").value, "");
    assert.equal(f.node("open").getAttribute("aria-expanded"), "false");
    f.key("F3");
    assert.equal(f.calls.length, 1, "F3 with empty query should focus, not scan");
    assert.equal(f.document.activeElement, f.node("input"));
  } finally { f.close(); }
});


test("viewer and real database backend find offscreen literal text and select only the matching conversation", async () => {
  const needle = "private exact %_ ' 東京";
  const rows = Array.from({ length: 500 }, (_, i) => ({
    id: 10000 - i, conversation: 7, idx: i * 2, content: i === 402 ? needle : "ordinary message",
  }));
  rows.push({ id: 1, conversation: 8, idx: 0, content: needle });
  const f = await fixture(rows);
  try {
    f.navigate("conversation", 7); f.click("open"); f.input(needle);
    await f.api.findTest.runTranscriptFind(1);
    assert.equal(f.routes.length, 1);
    assert.equal(f.routes[0].path, "/c/7/m/9598");
    f.flushRoute();
    assert.deepEqual(f.rendered.at(-1), { id: 7, message: 9598 });
    assert.match(f.node("status").textContent, /Found message 403 of 500/);
    await f.api.findTest.runTranscriptFind(-1);
    assert.match(f.node("status").textContent, /message 403 of 500 \(wrapped\)/);
    assert.equal(f.rendered.length, 3, "sole match must be reselected without another hash event");
    const scans = f.sqlReads.filter((q) => q.sql.includes("AS matched"));
    assert(scans.length > 3);
    assert(scans.every((q) => q.rows.length <= 128 && q.rows.every((r) => !Object.hasOwn(r, "content"))));
    assert(!JSON.stringify(f.routes).includes(needle));
  } finally { f.close(); }
});


test("loading or missing conversation metadata is not mistaken for a successfully empty transcript", async () => {
  const f = await fixture();
  try {
    f.navigate("conversation", 7); f.click("open"); f.input("text");
    f.pendingConversation();
    await f.api.findTest.runTranscriptFind(1);
    assert.equal(f.sources.length, 0);
    assert.match(f.node("status").textContent, /loading or unavailable/);
    assert.doesNotMatch(f.node("status").textContent, /No matching messages/);
    assert.equal(f.node("form").getAttribute("aria-busy"), "false");
  } finally { f.close(); }
});
