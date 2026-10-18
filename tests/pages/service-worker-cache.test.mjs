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
        async add(request) {
          const response = await network(urlOf(request));
          if (!response.ok) throw new Error("Cache.add received an unsuccessful response");
          await this.put(request, response);
        },
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
        async delete(request, options) {
          assert.equal(options?.ignoreVary, true, "revocation removes every Vary representation");
          const url = urlOf(request);
          calls.push(["entry-delete", name, url]);
          await hooks.entryDelete?.(name, url);
          return store.delete(url);
        },
      };
    },
    async has(name) {
      calls.push(["has", name]);
      await hooks.has?.(name);
      return stores.has(name);
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
  const context = vm.createContext({ self, caches, URL, Headers, Response,
    fetch: request => network(request), console: { log() {} } });
  vm.runInContext(source, context, { filename: "sw.js" });
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
  function install() {
    const waits = [];
    handlers.get("install")({ waitUntil(task) { waits.push(task); } });
    const settled = Promise.all(waits);
    void settled.catch(() => {});
    pending.push(settled);
    return settled;
  }
  const f = { name, prefix, stores, calls, hooks, message, request, install,
    resources: () => vm.runInContext("typeof cacheResources === 'undefined' ? 0 : cacheResources.size", context),
    network(fn) { network = fn; },
    offline() { network = async () => { throw new Error("offline"); }; },
    hold() {
      const gate = deferred();
      // Baseline implementations may never reach a boundary under test.
      // Observe an unused rejection without changing what awaiters receive.
      void gate.promise.catch(() => {});
      gates.push(gate); return gate;
    },
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

for (const directive of ["no-store", "private", "no-cache", 'max-age=0, PRIVATE="set-cookie"']) {
  test(`a new ${directive} response removes the older offline copy`, async t => {
    const f = fixture(t);
    f.seed("payload.enc", "old retained data"); f.seed("other.enc", "unrelated resource");
    f.network(async () => new Response("new private response", { headers: { "cache-control": directive } }));
    const request = f.request();
    assert.equal((await body(request))[1], "new private response"); await request.settled;
    assert.ok(!f.stores.get(f.name).has(scope + "payload.enc"));
    f.offline();
    assert.equal((await body(f.request()))[0].status, 503);
    assert.equal((await body(f.request("other.enc")))[1], "unrelated resource");
    assert.equal(f.resources(), 0);
  });
}

for (const status of [401, 403, 404, 410]) {
  test(`authoritative HTTP ${status} is returned, not replaced, and evicts stale offline data`, async t => {
    const f = fixture(t); f.seed("payload.enc", "old retained data");
    f.network(async () => new Response("server refusal", { status }));
    const request = f.request();
    const [response, text] = await body(request);
    assert.equal(response.status, status); assert.equal(text, "server refusal");
    await request.settled; f.offline();
    assert.equal((await body(f.request()))[0].status, 503);
  });
}

test("transient server failures do not erase useful offline data or masquerade as cached success", async t => {
  const f = fixture(t); f.seed("payload.enc", "last usable response");
  f.network(async () => new Response("unavailable", { status: 503 }));
  const request = f.request(); assert.equal((await body(request))[0].status, 503); await request.settled;
  f.offline(); assert.equal((await body(f.request()))[1], "last usable response");
});

test("an older network response cannot resurrect a resource after a newer privacy response", async t => {
  const f = fixture(t), oldNetwork = f.hold();
  f.seed("payload.enc"); f.network(() => oldNetwork.promise);
  const old = f.request();
  f.network(async () => new Response("now private", { headers: { "cache-control": "no-store" } }));
  const privateRequest = f.request(); await body(privateRequest); await privateRequest.settled;
  oldNetwork.resolve(new Response("old public data")); await body(old); await old.settled;
  f.offline(); assert.equal((await body(f.request()))[0].status, 503);
  assert.equal(f.resources(), 0);
});

test("revocation waits for an older body write without delaying the new network response", async t => {
  const f = fixture(t), put = f.hold();
  f.hooks.put = () => put.promise;
  const old = f.request(); await body(old); await turn();
  f.network(async () => new Response("private", { headers: { "cache-control": "private" } }));
  const restricted = f.request(); assert.equal((await body(restricted))[1], "private");
  let evicted = false; void restricted.settled.then(() => { evicted = true; });
  await turn(); const premature = evicted;
  f.offline(); assert.equal((await body(f.request()))[0].status, 503);
  put.resolve(); await old.settled; await restricted.settled;
  assert.equal(premature, false);
  assert.equal((await body(f.request()))[0].status, 503);
  assert.equal(f.resources(), 0);
});

test("a pending offline match cannot return revoked text after a server denial", async t => {
  const f = fixture(t), read = f.hold();
  f.seed("payload.enc", "revoked text"); f.offline(); f.hooks.match = () => read.promise;
  const old = f.request(); await turn();
  f.network(async () => new Response("gone", { status: 410 }));
  const denied = f.request(); await body(denied); await denied.settled;
  read.resolve(); assert.equal((await body(old))[0].status, 503);
});

test("resource eviction does not wait for a different URL's large cache write", async t => {
  const f = fixture(t), put = f.hold();
  f.seed("payload.enc"); f.hooks.put = () => put.promise;
  const other = f.request("large.enc"); await body(other); await turn();
  f.network(async () => new Response("gone", { status: 404 }));
  const gone = f.request(); await body(gone); await gone.settled;
  assert.ok(!f.stores.get(f.name).has(scope + "payload.enc"));
  put.resolve(); await other.settled;
  assert.equal(f.resources(), 0);
});

test("entry deletion failure disables fallback until an explicit successful clear", async t => {
  const f = fixture(t);
  f.seed("payload.enc", "revoked"); f.hooks.entryDelete = () => { throw new Error("storage fault"); };
  f.network(async () => new Response("private", { headers: { "cache-control": "no-store" } }));
  const restricted = f.request(); await body(restricted); await restricted.settled;
  f.offline(); assert.equal((await body(f.request()))[0].status, 503);
  assert.equal(f.resources(), 0, "failed URLs must not accumulate in a denylist");
  f.hooks.entryDelete = null; await cleared(f);
  f.network(async () => new Response("new public data"));
  const fresh = f.request(); await body(fresh); await fresh.settled;
  f.offline(); assert.equal((await body(f.request()))[1], "new public data");
});

test("a privacy response for a never-cached resource creates no empty persistent cache", async t => {
  const f = fixture(t);
  f.network(async () => new Response("private", { headers: { "cache-control": "no-store" } }));
  const request = f.request(); await body(request); await request.settled;
  assert.equal(f.stores.size, 0);
  assert.equal(f.calls.filter(call => call[0] === "open").length, 0);
  assert.equal(f.resources(), 0);
});

test("manual clear and an in-flight per-resource eviction cannot restore a cache", async t => {
  const f = fixture(t), eviction = f.hold();
  f.seed("payload.enc"); f.hooks.entryDelete = () => eviction.promise;
  f.network(async () => new Response("gone", { status: 410 }));
  const gone = f.request(); await body(gone); await turn();
  const clear = f.message(); await turn(); const premature = clear.replies.length;
  eviction.resolve(); await gone.settled; await clear.settled;
  assert.equal(premature, 0);
  assert.equal(clear.replies[0].type, "CACHE_CLEARED"); assert.equal(f.stores.size, 0);
});

test("resource ordering state is released after many distinct completed requests", async t => {
  const f = fixture(t);
  for (let n = 0; n < 100; n++) {
    const request = f.request(`${n}.enc`); await body(request); await request.settled;
  }
  assert.equal(f.resources(), 0);
});

test("opaque network responses are passed through, never converted to stale offline success", async t => {
  const f = fixture(t); f.seed("payload.enc", "must not substitute");
  // Status 0 is not constructible with new Response; Response.error exercises
  // that reconstruction boundary without claiming a real cross-origin fetch.
  const opaque = Response.error(); f.network(async () => opaque);
  const request = f.request(); assert.equal(await request.response, opaque); await request.settled;
  assert.equal(f.calls.filter(call => call[0] === "match").length, 0);
});

for (const directive of ["no-store", "private", "no-cache"]) {
  test(`installation refuses ${directive} assets instead of persisting them behind a ready worker`, async t => {
    const f = fixture(t);
    f.network(async () => new Response("must not persist", { headers: { "cache-control": directive } }));
    await assert.rejects(f.install());
    assert.equal(f.calls.filter(call => call[0] === "put").length, 0);
  });
}

test("installation still preloads the complete runtime and leaves older generations untouched", async t => {
  const f = fixture(t); f.seed("old.enc", "keep until activation", f.prefix + "v7");
  await f.install();
  const urls = [...f.stores.get(f.name).keys()];
  for (const path of ["index.html", "auth.js", "database.js", "conversation.js", "crypto_worker.js",
    "vendor/sqlite3.mjs", "vendor/sqlite3.wasm", "vendor/argon2.wasm"]) {
    assert.ok(urls.includes(scope + path), path);
  }
  assert.ok(f.stores.has(f.prefix + "v7"));
});

test("failed installation settles sibling writes before a clear may acknowledge", async t => {
  const f = fixture(t), put = f.hold();
  f.hooks.put = () => put.promise;
  f.network(async url => url.endsWith("styles.css")
    ? new Response("gone", { status: 404 }) : new Response("asset"));
  const installation = f.install();
  await turn();
  assert.ok(f.calls.some(call => call[0] === "put"));
  const clear = f.message(); await turn(); const premature = clear.replies.length;
  put.resolve(); await assert.rejects(installation); await clear.settled;
  assert.equal(premature, 0);
  assert.equal(clear.replies[0].type, "CACHE_CLEARED"); assert.equal(f.stores.size, 0);
});

for (const navigation of [false, true]) {
  test(`offline ${navigation ? "navigation" : "resource"} fallback refuses already-stored private responses`, async t => {
    const f = fixture(t), path = navigation ? "index.html" : "payload.enc";
    f.seed(path); f.stores.get(f.name).set(scope + path,
      new Response("private legacy cache", { headers: { "cache-control": "private" } }));
    f.offline();
    const request = f.request(navigation ? "nested/" : path, { navigate: navigation });
    assert.equal((await body(request))[0].status, 503);
  });
}

test("a request admitted during revocation cannot refill the URL when it completes later", async t => {
  const f = fixture(t), deletion = f.hold(), network = f.hold();
  f.seed("payload.enc"); f.hooks.entryDelete = () => deletion.promise;
  f.network(async () => new Response("private", { headers: { "cache-control": "private" } }));
  const restricted = f.request(); await body(restricted); await turn();
  f.network(() => network.promise); const during = f.request();
  deletion.resolve(); await restricted.settled;
  network.resolve(new Response("outdated public response")); await body(during); await during.settled;
  f.offline(); assert.equal((await body(f.request()))[0].status, 503);
  f.network(async () => new Response("fresh public response"));
  const fresh = f.request(); await body(fresh); await fresh.settled;
  f.offline(); assert.equal((await body(f.request()))[1], "fresh public response");
});

test("successful manual clear recovers even when an older eviction fails during the clear", async t => {
  const f = fixture(t), deletion = f.hold();
  f.seed("payload.enc"); f.hooks.entryDelete = () => deletion.promise;
  f.network(async () => new Response("gone", { status: 410 }));
  const restricted = f.request(); await body(restricted); await turn();
  const clear = f.message(); deletion.reject(new Error("entry deletion failed"));
  await restricted.settled; await clear.settled;
  assert.equal(clear.replies[0].type, "CACHE_CLEARED"); assert.equal(f.stores.size, 0);
  f.hooks.entryDelete = null; f.network(async () => new Response("fresh"));
  const fresh = f.request(); await body(fresh); await fresh.settled;
  f.offline(); assert.equal((await body(f.request()))[1], "fresh");
  assert.equal(f.resources(), 0);
});

test("revocation invalidates an older pending open before it can issue a put", async t => {
  const f = fixture(t), opening = f.hold();
  f.hooks.open = () => opening.promise;
  const old = f.request(); await body(old); await turn();
  f.network(async () => new Response("private", { headers: { "cache-control": "no-store" } }));
  const restricted = f.request(); await body(restricted);
  opening.resolve(); await old.settled; await restricted.settled;
  assert.equal(f.calls.filter(call => call[0] === "put").length, 0);
  f.offline(); assert.equal((await body(f.request()))[0].status, 503);
  assert.equal(f.resources(), 0);
});
