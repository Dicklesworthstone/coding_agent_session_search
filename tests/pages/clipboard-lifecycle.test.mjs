/** Run: node --experimental-vm-modules --test tests/pages/clipboard-lifecycle.test.mjs
 * Executes shipped share.js and auth.js with controlled browser boundaries.
 * This exercises lock/expiry cancellation, not browser permissions, cameras or SQLite-WASM.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(process.env.CASS_SHARE_SOURCE ||
  new URL("../../src/pages_assets/share.js", import.meta.url), "utf8");

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function fixture({ native, focus, select, copy } = {}) {
  const listeners = new Set();
  const nodes = [];
  const calls = [];
  const window = {
    location: { href: "https://example.invalid/archive/" },
    addEventListener(type, handler) { assert.equal(type, "cass:lock"); listeners.add(handler); },
    removeEventListener(type, handler) { assert.equal(type, "cass:lock"); listeners.delete(handler); },
  };
  const lock = () => { for (const handler of [...listeners]) handler(); };
  const document = {
    body: {
      appendChild(node) { calls.push("append"); node.parentNode = this; },
      removeChild(node) { calls.push("remove"); node.parentNode = null; },
    },
    createElement(tag) {
      assert.equal(tag, "textarea"); calls.push("create");
      const node = { value: "", style: {}, setAttribute() {},
        focus() { calls.push("focus"); focus?.(lock); },
        select() { calls.push("select"); select?.(lock); },
      };
      nodes.push(node); return node;
    },
    execCommand(command) {
      assert.equal(command, "copy"); calls.push("copy");
      return copy ? copy(lock) : true;
    },
  };
  const context = vm.createContext({ window, document,
    navigator: native ? { clipboard: { writeText(text) { calls.push(["native", text]); return native(); } } } : {},
    console: { error() {}, debug() {} }, URL, URLSearchParams,
  });
  const router = new vm.SyntheticModule([
    "buildConversationPath", "buildSearchPath", "parseConversationRouteParts", "splitRouteQuery",
  ], function () {
    this.setExport("buildConversationPath", id => `/c/${id}`);
    this.setExport("buildSearchPath", () => "/search");
    this.setExport("parseConversationRouteParts", () => null);
    this.setExport("splitRouteQuery", value => value.split("?"));
  }, { context });
  const module = new vm.SourceTextModule(source, { context });
  await module.link(name => { assert.equal(name, "./router.js"); return router; });
  await module.evaluate();
  return { api: module.namespace, lock, calls, nodes, listeners };
}

for (const boundary of ["lock", "abort", "navigation"]) {
  test(`pending native rejection cannot start fallback after ${boundary}`, async () => {
    const pending = deferred();
    const f = await fixture({ native: () => pending.promise });
    const controller = new AbortController();
    let current = true;
    const result = f.api.copyTextToClipboard("private transcript", {
      signal: controller.signal, isCurrent: () => current,
    });
    assert.equal(f.calls.length, 1);
    if (boundary === "lock") f.lock();
    if (boundary === "abort") controller.abort();
    if (boundary === "navigation") current = false;
    pending.reject(new Error("permission denied"));
    assert.equal(await result, false);
    assert.equal(f.nodes.length, 0, "cancelled plaintext must not enter the DOM");
    assert.equal(f.listeners.size, 0);
  });
}

test("native success after lock is not reported as a current successful copy", async () => {
  const pending = deferred();
  const f = await fixture({ native: () => pending.promise });
  const result = f.api.copyTextToClipboard("sent to browser");
  f.lock(); pending.resolve();
  assert.equal(await result, false);
  assert.equal(f.calls.length, 1, "already-issued native write cannot be retracted or retried");
  assert.equal(f.nodes.length, 0);
  assert.equal(f.listeners.size, 0);
});

test("already aborted or obsolete requests do not attempt any clipboard work", async () => {
  const f = await fixture({ native: () => Promise.resolve() });
  const controller = new AbortController(); controller.abort();
  assert.equal(await f.api.copyTextToClipboard("secret", { signal: controller.signal }), false);
  assert.equal(await f.api.copyTextToClipboard("secret", { isCurrent: () => false }), false);
  assert.deepEqual(f.calls, []);
  assert.equal(f.listeners.size, 0);
});

for (const trigger of ["focus", "select"]) {
  test(`synchronous lock during fallback ${trigger} clears plaintext before copy`, async () => {
    const f = await fixture({ [trigger]: lock => lock() });
    assert.equal(await f.api.copyTextToClipboard("private transcript"), false);
    assert.ok(!f.calls.includes("copy"));
    assert.equal(f.nodes.length, 1);
    assert.equal(f.nodes[0].value, "");
    assert.equal(f.nodes[0].parentNode, null);
    assert.equal(f.listeners.size, 0);
  });
}

test("current native success needs no fallback and releases the lock listener", async () => {
  const f = await fixture({ native: () => Promise.resolve() });
  assert.equal(await f.api.copyTextToClipboard("ordinary copy"), true);
  assert.deepEqual(f.calls, [["native", "ordinary copy"]]);
  assert.equal(f.listeners.size, 0);
});

for (const native of [undefined, () => Promise.reject(new Error("unavailable"))]) {
  test(`current ${native ? "denied" : "unavailable"} native clipboard still uses fallback`, async () => {
    const f = await fixture({ native });
    assert.equal(await f.api.copyTextToClipboard("ordinary copy"), true);
    assert.ok(f.calls.includes("copy"));
    assert.equal(f.nodes[0].value, "");
    assert.equal(f.nodes[0].parentNode, null);
    assert.equal(f.listeners.size, 0);
  });
}

test("fallback exceptions clear the temporary value and detach its DOM node", async () => {
  const f = await fixture({ copy: () => { throw new Error("copy failed"); } });
  assert.equal(await f.api.copyTextToClipboard("secret"), false);
  assert.equal(f.nodes[0].value, "");
  assert.equal(f.nodes[0].parentNode, null);
  assert.equal(f.listeners.size, 0);
});

test("invalid inputs reject before side effects and throwing currency checks fail closed", async () => {
  const f = await fixture({ native: () => Promise.resolve() });
  await assert.rejects(f.api.copyTextToClipboard(null), { name: "TypeError" });
  await assert.rejects(f.api.copyTextToClipboard("text", { isCurrent: true }), { name: "TypeError" });
  assert.equal(await f.api.copyTextToClipboard("text", { isCurrent: () => { throw Error("stale"); } }), false);
  assert.deepEqual(f.calls, []);
  assert.equal(f.listeners.size, 0);
});

test("existing link-copy entry points keep the ordinary successful result contract", async () => {
  const f = await fixture({ native: () => Promise.resolve() });
  const result = await f.api.copyConversationLink(17);
  assert.equal(result.success, true);
  assert.equal(result.link, "https://example.invalid/archive/#/c/17");
  assert.equal(f.listeners.size, 0);
});

// Evaluate the complete authentication module. The appended exports only expose
// private entry points and fixture state; no production function is replaced.
const authSource = readFileSync(process.env.CASS_AUTH_SOURCE ||
  new URL("../../src/pages_assets/auth.js", import.meta.url), "utf8");
const sessionSource = readFileSync(
  new URL("../../src/pages_assets/session.js", import.meta.url), "utf8");
const databaseSource = readFileSync(
  new URL("../../src/pages_assets/database.js", import.meta.url), "utf8");
const AUTH_DEK = Buffer.alloc(32, 23).toString("base64");

async function authFixture(options = {}) {
  let now = 1_700_000_000_000;
  let nextTimer = 1;
  const timers = new Map(), nodes = new Map(), listeners = new Map();
  const events = [], workers = [], dbCalls = [], imports = [];
  const node = id => {
    if (!nodes.has(id)) {
      const classes = new Set();
      nodes.set(id, {
        value: "", textContent: "", disabled: false, style: {},
        classList: {
          add(name) { classes.add(name); }, remove(name) { classes.delete(name); },
          contains(name) { return classes.has(name); },
        },
        querySelector(selector) { return node(`${id}:${selector}`); },
        replaceChildren() {}, addEventListener() {}, focus() {},
      });
    }
    return nodes.get(id);
  };
  const makeStorage = () => {
    const values = new Map();
    return { values, getItem(key) { return values.get(key) ?? null; },
      setItem(key, value) { values.set(key, String(value)); },
      removeItem(key) { values.delete(key); } };
  };
  const sessionStorage = makeStorage(), localStorage = makeStorage();
  const window = {
    cassSession: null,
    setTimeout(callback, delay) {
      if (options.timerFailure) throw new Error("timer unavailable");
      const id = nextTimer++; timers.set(id, { callback, delay }); return id;
    },
    addEventListener(type, handler) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(handler);
    },
    dispatchEvent(event) {
      events.push(event);
      for (const handler of listeners.get(event.type) || []) handler(event);
    },
  };
  const document = { readyState: "loading", hidden: false,
    getElementById: node, addEventListener() {} };
  const context = vm.createContext({ window, document, localStorage, sessionStorage,
    URL, Uint8Array, ArrayBuffer, atob, btoa,
    Date: class extends Date { static now() { return now; } },
    clearTimeout(id) { timers.delete(id); },
    console: { error() {}, warn() {}, debug() {}, log() {} },
    CustomEvent: class { constructor(type, options = {}) { this.type = type; this.detail = options.detail; } },
    Worker: class {
      constructor() { this.messages = []; this.terminated = false; workers.push(this); }
      postMessage(message) { this.messages.push(message); }
      terminate() { this.terminated = true; }
    },
    fetch: options.fetch,
  });
  const synthetic = entries => new vm.SyntheticModule(Object.keys(entries), function () {
    for (const [key, value] of Object.entries(entries)) this.setExport(key, value);
  }, { context });
  const storage = synthetic({
    StorageMode: { MEMORY: "memory", SESSION: "session", LOCAL: "local" },
    getArchiveScopeId: () => "fixture",
    getStorageMode: () => options.storageMode || "session",
    getStoredMode: () => options.storageMode || "session",
  });
  const session = new vm.SourceTextModule(sessionSource, { context });
  await session.link(name => { assert.equal(name, "./storage.js"); return storage; });
  await session.evaluate();
  const dependencies = {
    "./storage.js": storage,
    "./session.js": session,
    "./coi-detector.js": synthetic({ COI_STATE: {}, getCOIState() {},
      initCOIDetection() {}, onServiceWorkerActivated() {} }),
    "./password-strength.js": synthetic({ createStrengthMeter() {} }),
    "./sw-register.js": synthetic({ registerServiceWorker() {} }),
  };
  let database = synthetic({
    async initDatabase(bytes) {
      dbCalls.push(["init", [...bytes]]);
      await options.initializeDatabase?.(dbCalls.filter(call => call[0] === "init").length);
    },
    getStatistics() { return { conversations: 1, messages: 1 }; },
    closeDatabase() { dbCalls.push(["close"]); },
  });
  if (options.productionDatabase) {
    // Keep the actual database generation/admission/cleanup code. Only the
    // external SQLite-WASM runtime is controlled to pause its startup.
    const heap = new Uint8Array(1024);
    const sqlite = {
      oo1: { DB: class {
        constructor() { this.pointer = 1; dbCalls.push(["native-open"]); }
        checkRc(code) { assert.equal(code, 0); }
        close() { dbCalls.push(["native-close"]); }
      } },
      wasm: { alloc: () => 8, heap8u: () => heap, dealloc() {} },
      capi: { sqlite3_deserialize: () => 0 },
    };
    const vendor = synthetic({ default: async () => {
      await options.initializeWasm?.();
      return sqlite;
    } });
    await vendor.link(() => { throw new Error("unexpected vendor dependency"); });
    await vendor.evaluate();
    database = new vm.SourceTextModule(databaseSource, {
      context, initializeImportMeta(meta) { meta.url = "https://example.invalid/archive/database.js"; },
      async importModuleDynamically(name) {
        assert.equal(name, "https://example.invalid/archive/vendor/sqlite3.mjs");
        return vendor;
      },
    });
  }
  await database.link(() => { throw new Error("unexpected database dependency"); });
  await database.evaluate();
  dependencies["./database.js"] = database;
  const module = new vm.SourceTextModule(`${authSource}
    export { lockArchive, restoreSession, persistSession, expireActiveSession,
      handleWorkerError, handleDecryptFailed, recoverFromAppInitFailure,
      handleDecryptSuccess, loadUnencryptedDatabase, closeLiveDatabase,
      beginAppInitAttempt, handleSessionVisibilityChange, getSessionKeys,
      handleUnlockClick, unlockWithRecoveryText, openQrScanner, closeQrScanner };
    export function prepareFixture(scanner, key, fixtureConfig) {
      cacheElements();
      config = fixtureConfig || { encrypted: true, export_id: "fixture" };
      qrScanner = scanner;
      window.cassSession = key ? { dek: key, config } : null;
      elements.appScreen.classList.remove("hidden");
      elements.authScreen.classList.add("hidden");
      elements.passwordInput.value = "private password";
      initializeCryptoWorker();
    }
  `, { context, initializeImportMeta(meta) { meta.url = "https://example.invalid/archive/auth.js"; },
    async importModuleDynamically(name) {
      assert.equal(name, "./database.js");
      imports.push(name);
      await options.importDatabase?.(imports.length);
      return database;
    },
  });
  await module.link(name => {
    assert.ok(dependencies[name], `unexpected auth dependency ${name}`);
    return dependencies[name];
  });
  await module.evaluate();
  const api = module.namespace;
  api.prepareFixture(options.scanner || null, AUTH_DEK, options.config);
  const keys = api.getSessionKeys();
  const activeStorage = options.storageMode === "local" ? localStorage : sessionStorage;
  const seedSession = expiry => {
    activeStorage.setItem(keys.DEK, AUTH_DEK);
    activeStorage.setItem(keys.UNLOCKED, "true");
    if (expiry !== null) activeStorage.setItem(keys.EXPIRY, expiry);
  };
  return { api, node, window, events, workers, timers, dbCalls, imports,
    database: database.namespace,
    sessionStorage, localStorage, activeStorage, keys, seedSession,
    now: () => now, advance(ms) { now += ms; } };
}

for (const expiry of [null, "", "garbage", "NaN", "Infinity", "1700000060000suffix",
  "1700000060000.5", " 1700000060000", "+1700000060000", "1.7e12", "01700000060000",
  "0", "1699999999999", "1700000000000", "9007199254740992", "1702147483648"]) {
  test(`authentication refuses unusable stored expiry ${JSON.stringify(expiry)}`, async () => {
    const f = await authFixture();
    f.seedSession(expiry);
    assert.equal(f.api.restoreSession(), false);
    assert.equal(f.window.cassSession, null);
    assert.equal(f.timers.size, 0);
    assert.equal(f.activeStorage.values.size, 0, "rejected DEK and unlock markers must be cleared");
  });
}

for (const storageMode of ["session", "local"]) {
  test(`authentication restores a valid ${storageMode} deadline and enforces it on visibility`, async () => {
    const f = await authFixture({ storageMode });
    f.seedSession(String(f.now() + 60_000));
    assert.equal(f.api.restoreSession(), true);
    assert.equal(f.window.cassSession.dek, AUTH_DEK);
    assert.equal(f.timers.size, 1);
    assert.equal([...f.timers.values()][0].delay, 60_000);
    f.advance(60_000);
    f.api.handleSessionVisibilityChange();
    assert.equal(f.window.cassSession, null);
    assert.ok(f.node("app-screen").classList.contains("hidden"));
    assert.equal(f.events.at(-1).detail.action, "expired");
    await Promise.resolve();
  });
}

test("authentication never publishes a restored key when its expiry timer fails", async () => {
  const f = await authFixture({ timerFailure: true });
  f.seedSession(String(f.now() + 60_000));
  assert.equal(f.api.restoreSession(), false);
  assert.equal(f.window.cassSession, null);
  assert.equal(f.activeStorage.values.size, 0);
});

test("changing persistence cannot renew an already expired authentication session", async () => {
  const f = await authFixture();
  assert.equal(f.api.persistSession(AUTH_DEK, f.now()), false);
  assert.equal(f.window.cassSession, null);
  assert.equal(f.timers.size, 0);
  assert.equal(f.activeStorage.values.size, 0);
  assert.equal(f.events.at(-1).type, "cass:lock");
});

test("fresh authentication keeps the default deadline when persistence changes", async () => {
  const options = { storageMode: "memory" };
  const f = await authFixture(options);
  f.api.persistSession(AUTH_DEK);
  const deadline = f.now() + 4 * 60 * 60 * 1000;
  assert.equal(f.sessionStorage.values.size + f.localStorage.values.size, 0);
  assert.equal([...f.timers.values()][0].delay, 4 * 60 * 60 * 1000);
  f.advance(10_000);
  options.storageMode = "session";
  f.api.persistSession(AUTH_DEK);
  assert.equal(f.sessionStorage.getItem(f.keys.EXPIRY), String(deadline));
  options.storageMode = "local";
  f.api.persistSession(AUTH_DEK);
  assert.equal(f.sessionStorage.values.size, 0);
  assert.equal(f.localStorage.getItem(f.keys.EXPIRY), String(deadline));
  assert.equal([...f.timers.values()][0].delay, deadline - f.now());
});

for (const boundary of ["lock", "expiry", "worker-error", "decrypt-error", "init-error"]) {
  test(`authentication ${boundary} fences plaintext before a stalled camera can finish`, async () => {
    const scanner = deferred();
    const f = await authFixture({ scanner: { stop: () => scanner.promise, clear() {} } });
    f.seedSession(String(f.now() + 60_000));
    const observed = [];
    f.window.addEventListener("cass:lock", () => observed.push({
      key: f.window.cassSession, hidden: f.node("app-screen").classList.contains("hidden"),
    }));
    let pending;
    try {
      if (boundary === "lock") pending = f.api.lockArchive({ broadcast: true });
      if (boundary === "expiry") pending = f.api.expireActiveSession();
      if (boundary === "worker-error") pending = f.api.handleWorkerError(new Error("worker stopped"));
      if (boundary === "decrypt-error") pending = f.api.handleDecryptFailed({ error: "bad payload" });
      if (boundary === "init-error") pending = f.api.recoverFromAppInitFailure("init failed", new Error("bad DB"));
      assert.equal(f.window.cassSession, null);
      assert.ok(f.node("app-screen").classList.contains("hidden"));
      assert.ok(!f.node("auth-screen").classList.contains("hidden"));
      assert.equal(f.node("password").value, "");
      assert.equal(f.activeStorage.values.size, 0);
      assert.deepEqual(observed, [{ key: null, hidden: true }]);
      assert.ok(f.workers[0].terminated);
      await pending;
      // This wait completes while stop() is still unresolved: database cleanup
      // and the retry form must not depend on camera hardware making progress.
      await new Promise(resolve => setImmediate(resolve));
      assert.ok(f.dbCalls.some(call => call[0] === "close"));
    } finally { scanner.resolve(); }
  });
}

test("late decrypted payload cannot initialize or close a newer archive after lock", async () => {
  const importGate = deferred(), importing = deferred();
  const f = await authFixture({ importDatabase: async count => {
    if (count === 1) { importing.resolve(); await importGate.promise; }
  } });
  const staleBytes = new Uint8Array([9, 8, 7]);
  const stale = f.api.handleDecryptSuccess({ dbBytes: staleBytes });
  await importing.promise;
  await f.api.lockArchive({ broadcast: true });
  f.api.beginAppInitAttempt();
  f.window.cassSession = { dek: AUTH_DEK };
  await f.api.handleDecryptSuccess({ dbBytes: new Uint8Array([1, 2, 3]) });
  importGate.resolve();
  await stale;
  assert.deepEqual(f.dbCalls, [["close"], ["init", [1, 2, 3]]]);
  assert.deepEqual([...staleBytes], [0, 0, 0]);
  assert.equal(f.window.cassSession.dek, AUTH_DEK);
  assert.equal(f.events.filter(event => event.type === "cass:db-ready").length, 1);
});

test("late cancelled initialization cannot close the replacement database", async () => {
  const initializing = deferred(), initGate = deferred();
  const f = await authFixture({ initializeDatabase: async count => {
    if (count === 1) { initializing.resolve(); await initGate.promise; }
  } });
  const bytes = new Uint8Array([9, 8, 7]);
  const stale = f.api.handleDecryptSuccess({ dbBytes: bytes });
  await initializing.promise;
  await f.api.lockArchive({ broadcast: true });
  f.api.beginAppInitAttempt();
  f.window.cassSession = { dek: AUTH_DEK };
  await f.api.handleDecryptSuccess({ dbBytes: new Uint8Array([1, 2, 3]) });
  initGate.reject(new Error("Database initialization was cancelled"));
  await stale;
  assert.equal(f.dbCalls.filter(call => call[0] === "close").length, 1);
  assert.equal(f.events.filter(event => event.type === "cass:db-ready").length, 1);
  assert.deepEqual([...bytes], [0, 0, 0]);
  assert.equal(f.window.cassSession.dek, AUTH_DEK);
});

test("an obsolete cleanup cannot close a newer database", async () => {
  const f = await authFixture();
  const oldToken = f.api.beginAppInitAttempt();
  f.api.beginAppInitAttempt();
  f.window.cassSession = { dek: AUTH_DEK };
  await f.api.handleDecryptSuccess({ dbBytes: new Uint8Array([1, 2, 3]) });
  await f.api.closeLiveDatabase(oldToken);
  assert.deepEqual(f.dbCalls, [["init", [1, 2, 3]]]);
});

test("plain archive loading is cancelled before a late database import materializes it", async () => {
  const importing = deferred(), importGate = deferred();
  const f = await authFixture({
    config: { encrypted: false, payload: { size_bytes: 3 } },
    fetch: async () => new Response(new Uint8Array([9, 8, 7])),
    importDatabase: async count => {
      if (count === 1) { importing.resolve(); await importGate.promise; }
    },
  });
  const stale = f.api.loadUnencryptedDatabase();
  await importing.promise;
  await f.api.lockArchive({ broadcast: true });
  importGate.resolve();
  assert.equal(await stale, false);
  assert.deepEqual(f.dbCalls, [["close"]]);
  assert.equal(f.events.filter(event => event.type === "cass:db-ready").length, 0);
});

test("lock revokes production database initialization before a late WASM runtime returns", async () => {
  const booting = deferred(), runtime = deferred();
  const f = await authFixture({ productionDatabase: true, initializeWasm: async () => {
    booting.resolve(); await runtime.promise;
  } });
  const bytes = new Uint8Array([9, 8, 7]);
  const stale = f.api.handleDecryptSuccess({ dbBytes: bytes });
  await booting.promise;
  const lock = f.api.lockArchive({ broadcast: false });
  assert.equal(f.database.isDatabaseReady(), false);
  runtime.resolve();
  await Promise.all([lock, stale]);
  assert.equal(f.database.isDatabaseReady(), false);
  assert.deepEqual(f.dbCalls, [], "cancelled startup must not construct a native handle");
  assert.deepEqual([...bytes], [0, 0, 0]);
  assert.throws(() => f.database.queryAll("SELECT 1"), /not initialized/i);
  // A current generation remains usable after the cancelled attempt settles.
  await f.database.initDatabase(new Uint8Array([1, 2, 3]));
  assert.equal(f.database.isDatabaseReady(), true);
  const relock = f.api.lockArchive({ broadcast: false });
  assert.equal(f.database.isDatabaseReady(), false, "close must happen before lock yields");
  assert.deepEqual(f.dbCalls, [["native-open"], ["native-close"]]);
  await relock;
});

test("worker failure synchronously closes the production database without a viewer listener", async () => {
  const f = await authFixture({ productionDatabase: true });
  await f.database.initDatabase(new Uint8Array([1, 2, 3]));
  assert.equal(f.database.isDatabaseReady(), true);
  const failure = f.api.handleWorkerError(new Error("stopped"));
  assert.equal(f.database.isDatabaseReady(), false);
  assert.deepEqual(f.dbCalls, [["native-open"], ["native-close"]]);
  await failure;
});

test("lock rejects queued unlock callbacks but permits a fresh retry after cleanup", async () => {
  const f = await authFixture();
  f.window.addEventListener("cass:lock", () => {
    f.node("password").value = "queued password";
    void f.api.handleUnlockClick();
    f.api.unlockWithRecoveryText(AUTH_DEK, "queued recovery");
  });
  await f.api.lockArchive({ broadcast: true });
  assert.equal(f.workers.at(-1).messages.length, 0);
  f.node("password").value = "fresh password";
  await f.api.handleUnlockClick();
  assert.equal(f.workers.at(-1).messages.length, 1);
  assert.equal(f.workers.at(-1).messages[0].type, "UNLOCK_PASSWORD");
});

test("a stopped camera callback cannot unlock again after the archive is locked", async () => {
  const f = await authFixture();
  const stopped = deferred();
  let onScan;
  f.window.Html5Qrcode = class {
    async start(_camera, _options, success) { onScan = success; }
    stop() { return stopped.promise; }
    clear() {}
  };
  try {
    await f.api.openQrScanner();
    assert.equal(typeof onScan, "function");
    await f.api.lockArchive({ broadcast: true });
    onScan(AUTH_DEK);
    assert.equal(f.window.cassSession, null);
    assert.equal(f.workers.at(-1).messages.length, 0);
    assert.ok(f.node("app-screen").classList.contains("hidden"));
  } finally { stopped.resolve(); }
});

test("a scanner-open request queued before lock cannot reopen the camera afterward", async () => {
  const stopped = deferred();
  const f = await authFixture({ scanner: { stop: () => stopped.promise, clear() {} } });
  let opened = 0;
  f.window.Html5Qrcode = class { constructor() { opened += 1; } async start() {} };
  const closing = f.api.closeQrScanner();
  const opening = f.api.openQrScanner();
  await f.api.lockArchive({ broadcast: true });
  stopped.resolve();
  await Promise.all([closing, opening]);
  assert.equal(opened, 0);
});
