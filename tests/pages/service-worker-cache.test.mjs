/** Run: node --test tests/pages/service-worker-cache.test.mjs
 * Executes the shipped worker's real fetch/message handlers with native web
 * Request/Response streams and controlled CacheStorage/network boundaries.
 * This is not browser persistence, service-worker scheduling or OS erasure proof.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(process.env.CASS_SW_SOURCE ||
  new URL("../../src/pages_assets/sw.js", import.meta.url), "utf8");
const scope = "https://example.invalid/archive/";
const turn = () => new Promise(resolve => setImmediate(resolve));

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function fixture(t) {
  const handlers = new Map(), stores = new Map(), hooks = {}, calls = [], pending = [], gates = [];
  const urlOf = input => typeof input === "string" ? new URL(input, scope).href : input.url;
  const caches = {
    async open(name) {
      calls.push(["open", name]);
      await hooks.open?.(name);
      if (!stores.has(name)) stores.set(name, new Map());
      const store = stores.get(name); // Remains detached, not resurrected, after delete(name).
      return {
        async put(request, response) {
          const url = urlOf(request);
          calls.push(["put", name, url]);
          await hooks.put?.(name, url);
          const bytes = await response.arrayBuffer();
          store.set(url, new Response(bytes, { status: response.status, headers: response.headers }));
        },
        async match(request) {
          const url = urlOf(request);
          const snapshot = store.get(url)?.clone();
          calls.push(["match", name, url]);
          await hooks.match?.(name, url);
          return snapshot;
        },
      };
    },
    async match(request, { cacheName } = {}) {
      assert.equal(typeof cacheName, "string", "fallback must name only this generation");
      const url = urlOf(request);
      const snapshot = stores.get(cacheName)?.get(url)?.clone();
      calls.push(["match", cacheName, url]);
      await hooks.match?.(cacheName, url);
      return snapshot;
    },
    async keys() {
      calls.push(["keys"]);
      await hooks.keys?.();
      return [...stores.keys()];
    },
    async delete(name) {
      calls.push(["delete", name]);
      await hooks.delete?.(name);
      return stores.delete(name);
    },
  };
  const self = { registration: { scope }, location: { href: scope + "sw.js" },
    clients: { claim: async () => {} }, skipWaiting: async () => {},
    addEventListener(type, handler) { handlers.set(type, handler); },
  };
  let network = async () => new Response("network body");
  vm.runInNewContext(source, { self, caches, URL, Headers, Response,
    fetch: request => network(request), console: { log() {} } }, { filename: "sw.js" });
  function message(type = "CLEAR_CACHE") {
    const replies = [], waits = [];
    handlers.get("message")({ data: { type }, ports: [{ postMessage(reply) { replies.push(reply); } }],
      waitUntil(task) { waits.push(task); } });
    const settled = Promise.all(waits);
    pending.push(settled);
    return { replies, settled };
  }
  const name = message("GET_VERSION").replies[0].version;
  const prefix = name.slice(0, name.lastIndexOf("-") + 1);
  function request(path = "payload.enc", options = {}) {
    const raw = new Request(new URL(path, scope), options);
    const req = options.navigate ? { url: raw.url, method: raw.method,
      headers: raw.headers, cache: raw.cache, mode: "navigate" } : raw;
    let response;
    const waits = [];
    handlers.get("fetch")({ request: req,
      respondWith(value) { response = value; }, waitUntil(task) { waits.push(task); } });
    const settled = Promise.all(waits);
    pending.push(settled);
    return { response, settled };
  }
  const f = { name, prefix, stores, calls, hooks, message, request,
    network(fn) { network = fn; },
    offline() { network = async () => { throw new Error("offline"); }; },
    hold() { const gate = deferred(); gates.push(gate); return gate; },
    seed(path, body = "cached body", cacheName = name) {
      if (!stores.has(cacheName)) stores.set(cacheName, new Map());
      stores.get(cacheName).set(urlOf(path), new Response(body));
    },
  };
  t.after(async () => {
    for (const gate of gates) gate.resolve();
    await Promise.allSettled(pending);
  });
  return f;
}

async function body(result) {
  const response = await result.response;
  return [response, await response.text()];
}

async function cleared(f) {
  const operation = f.message();
  await operation.settled;
  assert.equal(operation.replies.length, 1);
  assert.equal(operation.replies[0].type, "CACHE_CLEARED");
  return operation;
}

test("online responses remain immediate and become scoped offline responses with security headers", async t => {
  const f = fixture(t), gate = f.hold();
  f.hooks.put = () => gate.promise;
  const online = f.request();
  const [response, text] = await body(online);
  assert.equal(text, "network body");
  assert.equal(response.headers.get("cross-origin-opener-policy"), "same-origin");
  assert.equal(response.headers.get("cross-origin-embedder-policy"), "require-corp");
  assert.equal(response.headers.get("referrer-policy"), "no-referrer");
  let finished = false; void online.settled.then(() => { finished = true; });
  await turn(); assert.equal(finished, false, "waitUntil must retain a pending cache write");
  gate.resolve(); await online.settled;
  f.offline();
  assert.equal((await body(f.request()))[1], "network body");
});

test("offline misses do not recreate a cache after successful cleanup", async t => {
  const f = fixture(t);
  f.seed("payload.enc"); await cleared(f); f.offline();
  const [response] = await body(f.request());
  assert.equal(response.status, 503);
  assert.equal(f.stores.size, 0);
  assert.equal(f.calls.filter(call => call[0] === "open").length, 0);
});

test("a pre-clear network response cannot restore data but a fresh request can cache again", async t => {
  const f = fixture(t), gate = f.hold();
  f.network(() => gate.promise);
  const stale = f.request();
  await cleared(f);
  gate.resolve(new Response("old request"));
  assert.equal((await body(stale))[1], "old request", "network delivery is not cancelled");
  await stale.settled;
  assert.equal(f.stores.size, 0);
  f.network(async () => new Response("fresh request"));
  const fresh = f.request(); await body(fresh); await fresh.settled;
  f.offline();
  assert.equal((await body(f.request()))[1], "fresh request");
});

test("requests begun during cleanup stay ineligible even if their network completes later", async t => {
  const f = fixture(t), deletion = f.hold(), network = f.hold();
  f.seed("payload.enc"); f.hooks.delete = () => deletion.promise;
  const clear = f.message(); await turn();
  f.network(() => network.promise);
  const during = f.request();
  deletion.resolve(); await clear.settled;
  assert.equal(clear.replies[0].type, "CACHE_CLEARED");
  network.resolve(new Response("during clear")); await body(during); await during.settled;
  assert.equal(f.stores.size, 0);
});

for (const boundary of ["open", "put"]) {
  test(`clear waits for already-started cache ${boundary} before acknowledging removal`, async t => {
    const f = fixture(t), gate = f.hold();
    f.hooks[boundary] = () => gate.promise;
    const download = f.request(); await body(download); await turn();
    assert.ok(f.calls.some(call => call[0] === boundary));
    const clear = f.message(); await turn();
    const premature = clear.replies.length;
    gate.resolve(); await download.settled; await clear.settled;
    assert.equal(premature, 0, "must not acknowledge while a cache mutation is unresolved");
    assert.equal(clear.replies[0].type, "CACHE_CLEARED");
    assert.equal(f.stores.size, 0);
    if (boundary === "open") assert.equal(f.calls.filter(call => call[0] === "put").length, 0);
  });
}

for (const navigation of [false, true]) {
  test(`pending ${navigation ? "navigation index" : "resource"} cache read cannot cross a clear boundary`, async t => {
    const f = fixture(t), gate = f.hold();
    const cachedPath = navigation ? "index.html" : "payload.enc";
    f.seed(cachedPath, "pre-clear plaintext"); f.offline();
    f.hooks.match = (_name, url) => url.endsWith(cachedPath) ? gate.promise : undefined;
    const old = f.request(navigation ? "nested/" : cachedPath, { navigate: navigation });
    await turn();
    assert.ok(f.calls.some(call => call[0] === "match" && call[2].endsWith(cachedPath)));
    await cleared(f); gate.resolve();
    const [response, text] = await body(old);
    assert.equal(response.status, 503);
    assert.ok(!text.includes("pre-clear plaintext"));
    assert.equal(f.stores.size, 0);
  });
}

test("old network failures cannot fall back even to a newly populated generation", async t => {
  const f = fixture(t), gate = f.hold();
  f.network(() => gate.promise);
  const old = f.request();
  await cleared(f); f.seed("payload.enc", "newly cached body");
  gate.reject(new Error("late network failure"));
  assert.equal((await body(old))[0].status, 503);
  assert.equal(f.calls.filter(call => call[0] === "match").length, 0);
});

test("overlapping clears keep admission closed until the final clear finishes", async t => {
  const f = fixture(t), gate = f.hold(), network = f.hold();
  f.seed("payload.enc"); f.hooks.delete = () => gate.promise;
  const first = f.message(), second = f.message(); await turn();
  f.network(() => network.promise); const during = f.request();
  assert.equal(first.replies.length + second.replies.length, 0);
  gate.resolve(); await first.settled; await second.settled;
  network.resolve(new Response("not to retain")); await body(during); await during.settled;
  assert.equal(first.replies[0].type, "CACHE_CLEARED");
  assert.equal(second.replies[0].type, "CACHE_CLEARED");
  assert.equal(f.stores.size, 0);
});

test("failed cleanup reports failure, preserves unrelated scopes and permits a real retry", async t => {
  const f = fixture(t);
  f.seed("payload.enc"); f.seed("old.enc", "old generation", f.prefix + "v1");
  f.seed("other", "keep", "cass-archive-unrelated-v8"); f.seed("other", "keep", "other-app");
  f.hooks.delete = () => { throw new Error("storage deletion refused"); };
  const failed = f.message(); await failed.settled;
  assert.equal(failed.replies[0].type, "CACHE_CLEAR_FAILED");
  assert.equal(f.stores.size, 4);
  f.hooks.delete = null; await cleared(f);
  assert.deepEqual([...f.stores.keys()].sort(), ["cass-archive-unrelated-v8", "other-app"]);
});

test("a failed put neither fails the network response nor poisons subsequent cleanup", async t => {
  const f = fixture(t);
  f.hooks.put = () => { throw new Error("quota exceeded"); };
  const request = f.request();
  assert.equal((await body(request))[1], "network body"); await request.settled;
  await cleared(f); assert.equal(f.stores.size, 0);
});

test("private request variants bypass cache reads and writes, and unrelated requests are not intercepted", async t => {
  const f = fixture(t); f.seed("payload.enc");
  const variants = [
    ["payload.enc?token=secret", {}], ["payload.enc", { headers: { authorization: "Bearer secret" } }],
    ["payload.enc", { headers: { range: "bytes=0-1" } }], ["payload.enc", { cache: "no-store" }],
  ];
  for (const [path, options] of variants) {
    const online = f.request(path, options); await body(online); await online.settled;
  }
  f.offline();
  for (const [path, options] of variants) assert.equal((await body(f.request(path, options)))[0].status, 503);
  for (const path of ["/other-app/file", "https://elsewhere.invalid/archive/file"]) {
    assert.equal(f.request(path).response, undefined);
  }
  assert.equal(f.request("payload.enc", { method: "POST" }).response, undefined);
  assert.equal(f.calls.length, 0);
});

test("cache enumeration failures cannot produce a successful clear receipt", async t => {
  const f = fixture(t); f.seed("payload.enc");
  f.hooks.keys = () => { throw new Error("cannot enumerate storage"); };
  const clear = f.message(); await clear.settled;
  assert.equal(clear.replies[0].type, "CACHE_CLEAR_FAILED");
  f.hooks.keys = null; await cleared(f);
  assert.equal(f.stores.size, 0);
});
