/**
 * Production classic-worker lifecycle tests with real WebCrypto, raw DEFLATE,
 * streams and transferable buffers. Network/crypto barriers force interleavings;
 * this is not browser/sqlite-WASM or process-RSS qualification.
 * Run: node --test tests/pages/crypto-worker-admission.test.mjs
 * CASS_CRYPTO_WORKER_SOURCE can select the unmodified baseline for negative runs.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { webcrypto } from "node:crypto";
import { deflateRawSync } from "node:zlib";
import { setImmediate as yieldTurn } from "node:timers/promises";
import vm from "node:vm";
import { createServer } from "node:http";
import { Worker } from "node:worker_threads";
import test from "node:test";

const source = readFileSync(process.env.CASS_CRYPTO_WORKER_SOURCE ||
  new URL("../../src/pages_assets/crypto_worker.js", import.meta.url), "utf8");
const b64 = (bytes) => Buffer.from(bytes).toString("base64");
const zeros = (bytes) => bytes.byteLength === 0 || bytes.every((value) => value === 0);
function barrier() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}
async function completed(promise) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("worker did not settle")), 2000);
    })]);
  } finally { clearTimeout(timer); }
}
async function until(predicate) {
  const deadline = Date.now() + 1500;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await yieldTurn();
  }
  assert.ok(predicate(), "expected asynchronous milestone");
}

async function archive(seed = 1, size = 389, chunkSize = 128) {
  const plain = Uint8Array.from({ length: size }, (_, index) => 33 + (index + seed) % 80);
  const dek = new Uint8Array(32).fill(seed);
  const exportId = new Uint8Array(16).fill(seed + 1);
  const baseNonce = new Uint8Array(12).fill(seed + 2);
  const recovery = new Uint8Array(32).fill(seed + 3);
  const salt = new Uint8Array(16).fill(seed + 4);
  const nonce = new Uint8Array(12).fill(seed + 5);
  const key = await webcrypto.subtle.importKey("raw", dek, "AES-GCM", false, ["encrypt"]);
  const chunks = [];
  async function seal(bytes, index) {
    const iv = new Uint8Array(baseNonce);
    new DataView(iv.buffer).setUint32(8, index, false);
    const aad = new Uint8Array(21);
    aad.set(exportId);
    new DataView(aad.buffer).setUint32(16, index, false);
    aad[20] = 2;
    return new Uint8Array(await webcrypto.subtle.encrypt(
      { name: "AES-GCM", iv, additionalData: aad }, key, deflateRawSync(bytes),
    ));
  }
  for (let offset = 0; offset < plain.length; offset += chunkSize) {
    chunks.push(await seal(plain.subarray(offset, offset + chunkSize), chunks.length));
  }
  const material = await webcrypto.subtle.importKey("raw", recovery, "HKDF", false, ["deriveBits"]);
  const kek = await webcrypto.subtle.deriveBits({
    name: "HKDF", hash: "SHA-256", salt, info: new TextEncoder().encode("cass-pages-kek-v2"),
  }, material, 256);
  const wrapping = await webcrypto.subtle.importKey("raw", kek, "AES-GCM", false, ["encrypt"]);
  const slotAad = new Uint8Array(17);
  slotAad.set(exportId);
  slotAad[16] = 7;
  const wrapped = await webcrypto.subtle.encrypt(
    { name: "AES-GCM", iv: nonce, additionalData: slotAad }, wrapping, dek,
  );
  const config = {
    version: 2, compression: "deflate", export_id: b64(exportId), base_nonce: b64(baseNonce),
    kdf_defaults: { memory_kb: 65536, iterations: 3, parallelism: 4 },
    key_slots: [{ id: 7, slot_type: "recovery", kdf: "hkdf-sha256", salt: b64(salt),
      nonce: b64(nonce), wrapped_dek: b64(wrapped) }],
    payload: {
      chunk_count: chunks.length, chunk_size: chunkSize, total_plaintext_size: size,
      total_compressed_size: chunks.reduce((sum, bytes) => sum + bytes.length, 0),
      files: chunks.map((_, index) => `payload/chunk-${String(index).padStart(5, "0")}.bin`),
    },
  };
  return { plain, dek, recovery, config, chunks, seal };
}

function harness(t, fixture, options = {}) {
  const messages = [];
  const arrays = [];
  const calls = [];
  const deadlines = new Map();
  const decryptOutputs = [];
  let nextTimer = 0;
  let fetchImpl = options.fetch;
  let importCount = 0;
  let decryptCount = 0;
  const trackedBytes = new Proxy(Uint8Array, {
    construct(target, args) {
      const bytes = Reflect.construct(target, args);
      if (typeof args[0] === "number") arrays.push(bytes);
      return bytes;
    },
  });
  const subtle = {
    async importKey(...args) {
      const result = await webcrypto.subtle.importKey(...args);
      await options.afterImport?.(++importCount, args, result);
      return result;
    },
    deriveBits: webcrypto.subtle.deriveBits.bind(webcrypto.subtle),
    async decrypt(...args) {
      const result = await webcrypto.subtle.decrypt(...args);
      decryptOutputs.push(result);
      await options.afterDecrypt?.(++decryptCount, args, result);
      return result;
    },
  };
  const sandbox = {
    crypto: { subtle }, Uint8Array: trackedBytes, ArrayBuffer, DataView,
    TextEncoder, TextDecoder, URL, AbortController, atob, btoa,
    DecompressionStream,
    console: { error() {}, warn() {}, debug() {} },
    setTimeout(callback, delay) {
      assert.equal(delay, 120_000, "deadline is per download, not a shortened production limit");
      const id = ++nextTimer;
      deadlines.set(id, callback);
      return id;
    },
    clearTimeout(id) { deadlines.delete(id); },
    async fetch(url, init) {
      const call = { url: String(url), signal: init?.signal };
      calls.push(call);
      if (fetchImpl) return fetchImpl(call, calls.length);
      const index = Number(call.url.match(/chunk-(\d+)\.bin$/)?.[1]);
      assert.ok(Number.isInteger(index), `unexpected URL: ${call.url}`);
      return new Response(new Uint8Array(fixture.chunks[index]));
    },
    postMessage(message, transfer = []) {
      messages.push(structuredClone(message, { transfer }));
    },
  };
  sandbox.self = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox, { filename: "crypto_worker.js" });
  const dispatch = (type, requestId, extra = {}) => sandbox.onmessage({
    data: { type, requestId, config: structuredClone(fixture.config), dek: b64(fixture.dek), ...extra },
  });
  t.after(() => { void dispatch("CLEAR_KEYS", "cleanup"); });
  return {
    messages, arrays, calls, deadlines, decryptOutputs, sandbox, dispatch,
    decrypt: (id, extra) => dispatch("DECRYPT_DATABASE", id, extra),
    replaceFetch(implementation) { fetchImpl = implementation; },
    fireDeadline() {
      assert.equal(deadlines.size, 1, "exactly one active download has a deadline");
      [...deadlines.values()][0]();
    },
    databaseBuffers: () => arrays.filter((bytes) => bytes.byteLength === fixture.plain.length),
    terminal: (id) => messages.filter((message) => message.requestId === id &&
      ["DECRYPT_SUCCESS", "DECRYPT_FAILED", "UNLOCK_SUCCESS", "UNLOCK_FAILED"].includes(message.type)),
    evaluate: (script) => vm.runInContext(script, sandbox),
  };
}
function failed(f, id, pattern) {
  const terminal = f.terminal(id);
  assert.equal(terminal.length, 1, `exactly one terminal response for ${id}`);
  assert.equal(terminal[0].type, "DECRYPT_FAILED");
  if (pattern) assert.match(terminal[0].error, pattern);
}
function succeeded(f, id, fixture) {
  const terminal = f.terminal(id);
  assert.equal(terminal.length, 1);
  assert.equal(terminal[0].type, "DECRYPT_SUCCESS", terminal[0].error);
  assert.equal(terminal[0].dbSize, fixture.plain.length);
  assert.deepEqual(new Uint8Array(terminal[0].dbBytes), fixture.plain);
}

for (const size of [1, 128, 389, 0]) {
  test(`real AES-GCM/raw-DEFLATE ${size}-byte archive transfers exact plaintext`, async (t) => {
    const fixture = await archive(1, size);
    const f = harness(t, fixture);
    await completed(f.decrypt("roundtrip"));
    succeeded(f, "roundtrip", fixture);
    assert.equal(f.calls.length, fixture.chunks.length);
    assert.equal(f.deadlines.size, 0);
    assert.ok(f.arrays.every(zeros), "owned key/plaintext arrays are wiped or transferred");
  });
}

test("overlapping jobs allocate only the active database and retain only the latest waiter", async (t) => {
  const fixture = await archive();
  const gate = barrier();
  t.after(gate.resolve);
  let entered = false;
  const f = harness(t, fixture, { afterImport: async (count) => {
    if (count === 1) { entered = true; await gate.promise; }
  } });
  const old = f.decrypt("old");
  await until(() => entered);
  const jobs = Array.from({ length: 80 }, (_, index) => f.decrypt(`new-${index}`));
  await yieldTurn();
  assert.equal(f.databaseBuffers().length, 1, "waiting requests must not allocate full databases");
  for (let index = 0; index < 79; index++) failed(f, `new-${index}`, /superseded/);
  assert.equal(f.terminal("new-79").length, 0);
  const keyBuffers = f.arrays.filter((bytes) => bytes.length === 32);
  assert.equal(keyBuffers.filter((bytes) => !zeros(bytes)).length, 2, "only active/latest DEKs remain");
  gate.resolve();
  await completed(Promise.all([old, ...jobs]));
  failed(f, "old", /superseded/);
  succeeded(f, "new-79", fixture);
  assert.ok(f.arrays.every(zeros));
});

test("CLEAR_KEYS scrubs copied plaintext while non-cancellable crypto still owns admission", async (t) => {
  const fixture = await archive();
  const gate = barrier();
  t.after(gate.resolve);
  let entered = false;
  const f = harness(t, fixture, { afterDecrypt: async (count) => {
    if (count === 2) { entered = true; await gate.promise; }
  } });
  const old = f.decrypt("old");
  await until(() => entered);
  const buffer = f.databaseBuffers()[0];
  assert.deepEqual(buffer.subarray(0, 128), fixture.plain.subarray(0, 128));
  await f.dispatch("CLEAR_KEYS", "clear");
  assert.ok(zeros(buffer), "reset must wipe already accumulated plaintext immediately");
  const queued = f.decrypt("latest");
  await yieldTurn();
  assert.equal(f.databaseBuffers().length, 1, "crypto must settle before another buffer is allocated");
  gate.resolve();
  await completed(Promise.all([old, queued]));
  failed(f, "old", /superseded/);
  succeeded(f, "latest", fixture);
  assert.ok(f.decryptOutputs.every((value) => zeros(new Uint8Array(value))));
});

test("reset settles a stalled header fetch, aborts its signal, and permits a fresh job", async (t) => {
  const fixture = await archive();
  const f = harness(t, fixture, { fetch: () => new Promise(() => {}) });
  const old = f.decrypt("old");
  await until(() => f.calls.length === 1);
  await f.dispatch("CLEAR_KEYS", "clear");
  await completed(old);
  assert.equal(f.calls[0].signal.aborted, true);
  failed(f, "old", /superseded/);
  assert.ok(f.databaseBuffers().every(zeros));
  f.replaceFetch(null);
  await completed(f.decrypt("fresh"));
  succeeded(f, "fresh", fixture);
});

test("a late header response after reset is cancelled rather than read or decrypted", async (t) => {
  const fixture = await archive();
  const headers = barrier();
  t.after(() => headers.resolve(new Response()));
  let cancelled = 0;
  const f = harness(t, fixture, { fetch: () => headers.promise });
  const old = f.decrypt("old");
  await until(() => f.calls.length === 1);
  await f.dispatch("CLEAR_KEYS", "clear");
  await completed(old);
  headers.resolve(new Response(new ReadableStream({ cancel() { cancelled++; } })));
  await until(() => cancelled === 1);
  assert.equal(f.decryptOutputs.length, 0);
  failed(f, "old", /superseded/);
});

for (const phase of ["headers", "body"]) {
  test(`download deadline releases a stalled ${phase} request without plaintext success`, async (t) => {
    const fixture = await archive();
    let cancelled = false;
    const f = harness(t, fixture, { fetch: () => phase === "headers" ? new Promise(() => {}) :
      new Response(new ReadableStream({ cancel() { cancelled = true; return new Promise(() => {}); } })) });
    const job = f.decrypt("timeout");
    await until(() => f.calls.length === 1);
    await yieldTurn();
    f.fireDeadline();
    await completed(job);
    failed(f, "timeout", /timed out.*retry/);
    assert.equal(f.calls[0].signal.aborted, true);
    if (phase === "body") assert.ok(cancelled);
    assert.equal(f.deadlines.size, 0);
    assert.ok(f.databaseBuffers().every(zeros));
  });
}

test("reset rejects the waiter immediately but keeps active crypto counted", async (t) => {
  const fixture = await archive();
  const gate = barrier();
  t.after(gate.resolve);
  let entered = false;
  const f = harness(t, fixture, { afterImport: async (count) => {
    if (count === 1) { entered = true; await gate.promise; }
  } });
  const active = f.decrypt("active");
  await until(() => entered);
  const waiting = f.decrypt("waiting");
  await f.dispatch("CLEAR_KEYS", "clear");
  await completed(waiting);
  failed(f, "waiting", /superseded/);
  assert.equal(f.databaseBuffers().length, 1);
  assert.equal(f.calls.length, 0);
  gate.resolve();
  await completed(active);
  failed(f, "active", /superseded/);
  assert.ok(f.arrays.every(zeros));
});

test("new recovery unlock cancels active/waiting decryptions using real HKDF and GCM", async (t) => {
  const fixture = await archive();
  const gate = barrier();
  t.after(gate.resolve);
  let entered = false;
  const f = harness(t, fixture, { afterImport: async (count) => {
    if (count === 1) { entered = true; await gate.promise; }
  } });
  const active = f.decrypt("active");
  await until(() => entered);
  const waiting = f.decrypt("waiting");
  await completed(f.dispatch("UNLOCK_RECOVERY", "unlock", { recoverySecret: b64(fixture.recovery) }));
  await completed(waiting);
  failed(f, "waiting", /superseded/);
  assert.equal(f.terminal("unlock")[0].type, "UNLOCK_SUCCESS");
  assert.equal(f.terminal("unlock")[0].dek, b64(fixture.dek));
  assert.equal(f.databaseBuffers().length, 1);
  gate.resolve();
  await completed(active);
  failed(f, "active", /superseded/);
  await completed(f.decrypt("fresh"));
  succeeded(f, "fresh", fixture);
});

test("authentication failure wipes its allocation and does not poison the next job", async (t) => {
  const fixture = await archive();
  const f = harness(t, fixture);
  await completed(f.decrypt("bad-key", { dek: b64(new Uint8Array(32).fill(77)) }));
  failed(f, "bad-key", /Failed to decrypt chunk 0/);
  assert.ok(f.databaseBuffers().every(zeros));
  await completed(f.decrypt("correct-key"));
  succeeded(f, "correct-key", fixture);
});

test("malformed configuration and short keys never start database allocation or I/O", async (t) => {
  const fixture = await archive();
  const f = harness(t, fixture);
  for (const [id, extra, error] of [
    ["version", { config: { ...fixture.config, version: 99 } }, /schema version/],
    ["path", { config: { ...fixture.config, payload: { ...fixture.config.payload, files: ["../secret"] } } }, /files list/],
    ["key", { dek: b64(new Uint8Array(31).fill(7)) }, /key length/],
  ]) {
    await completed(f.decrypt(id, extra));
    failed(f, id, error);
  }
  assert.equal(f.databaseBuffers().length, 0);
  assert.equal(f.calls.length, 0);
  assert.ok(f.arrays.every(zeros));
});

test("invalid superseding request still cancels the old session and waiter", async (t) => {
  const fixture = await archive();
  const gate = barrier();
  t.after(gate.resolve);
  let entered = false;
  const f = harness(t, fixture, { afterImport: async (count) => {
    if (count === 1) { entered = true; await gate.promise; }
  } });
  const active = f.decrypt("old");
  await until(() => entered);
  const waiting = f.decrypt("waiting");
  await completed(f.decrypt("invalid", { config: null }));
  failed(f, "invalid", /Invalid archive config/);
  await completed(waiting);
  failed(f, "waiting", /superseded/);
  gate.resolve();
  await completed(active);
  failed(f, "old", /superseded/);
  assert.equal(f.calls.length, 0);
});

test("oversized headers cancel unread response bodies and leave the slot retryable", async (t) => {
  const fixture = await archive();
  let cancelled = 0;
  const f = harness(t, fixture, { fetch: () => new Response(new ReadableStream({
    cancel() { cancelled++; return new Promise(() => {}); },
  }), { headers: { "content-length": "99999999" } }) });
  await completed(f.decrypt("oversize"));
  failed(f, "oversize", /download limit/);
  assert.equal(cancelled, 1);
  assert.equal(f.decryptOutputs.length, 0);
  f.replaceFetch(null);
  await completed(f.decrypt("retry"));
  succeeded(f, "retry", fixture);
});

test("stream overflow ignores misleading content-length and never waits for cancel", async (t) => {
  const fixture = await archive();
  const piece = new Uint8Array(70_000).fill(8);
  let cancelled = 0;
  const f = harness(t, fixture, { fetch: () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(piece); },
    cancel() { cancelled++; return new Promise(() => {}); },
  }), { headers: { "content-length": "1" } }) });
  await completed(f.decrypt("overflow"));
  failed(f, "overflow", /download limit/);
  assert.ok(zeros(piece));
  assert.equal(cancelled, 1);
  assert.equal(f.decryptOutputs.length, 0);
});

test("late body bytes are scrubbed even when an adversarial reader ignores cancellation", async (t) => {
  const fixture = await archive();
  const read = barrier();
  t.after(() => read.resolve({ done: true }));
  let reads = 0;
  let cancelled = 0;
  let released = false;
  const reader = {
    read() { reads++; return read.promise; },
    cancel() { cancelled++; return new Promise(() => {}); },
    releaseLock() { released = true; },
  };
  const f = harness(t, fixture, { fetch: () => ({ ok: true, body: { getReader: () => reader } }) });
  const job = f.decrypt("late-body");
  await until(() => reads === 1);
  await f.dispatch("CLEAR_KEYS", "clear");
  await completed(job);
  failed(f, "late-body", /superseded/);
  assert.equal(cancelled, 1);
  assert.ok(released);
  const piece = new Uint8Array(16).fill(9);
  read.resolve({ done: false, value: piece });
  await until(() => zeros(piece));
  assert.equal(f.decryptOutputs.length, 0);
});

test("missing chunks fail once, cancel bodies, and release download deadlines", async (t) => {
  const fixture = await archive();
  let cancelled = 0;
  const f = harness(t, fixture, { fetch: () => new Response(new ReadableStream({
    cancel() { cancelled++; },
  }), { status: 503 }) });
  await completed(f.decrypt("missing"));
  failed(f, "missing", /503/);
  assert.equal(cancelled, 1);
  assert.equal(f.deadlines.size, 0);
  assert.ok(f.databaseBuffers().every(zeros));
});

for (const field of ["total_plaintext_size", "total_compressed_size"]) {
  test(`authenticated payload with mismatched ${field} cannot publish a database`, async (t) => {
    const fixture = await archive();
    const f = harness(t, fixture);
    fixture.config.payload[field]++;
    await completed(f.decrypt("size"));
    failed(f, "size", /size mismatch/);
    assert.ok(f.arrays.every(zeros));
  });
}

test("an authenticated decompression bomb still fails the per-chunk output bound", async (t) => {
  const fixture = await archive(1, 128, 128);
  fixture.chunks[0] = await fixture.seal(new Uint8Array(1024).fill(65), 0);
  fixture.config.payload.total_compressed_size = fixture.chunks[0].length;
  const f = harness(t, fixture);
  await completed(f.decrypt("bomb"));
  failed(f, "bomb", /Decompressed chunk exceeds/);
  assert.ok(f.databaseBuffers().every(zeros));
});

// Node's worker-thread bridge supplies only the browser worker message API.
// Fetch, AbortController, DEFLATE and buffer transfers are real implementations.
test("real worker and HTTP Fetch abort the superseded archive before serving its replacement", async (t) => {
  const fixture = await archive(3);
  const firstRequest = barrier();
  const firstClosed = barrier();
  const finished = barrier();
  const messages = [];
  let requests = 0;
  const server = createServer((request, response) => {
    requests++;
    if (requests === 1) {
      response.on("close", firstClosed.resolve);
      firstRequest.resolve();
      return; // Deliberately leave the old archive's response headers pending.
    }
    const index = Number(request.url.match(/chunk-(\d+)\.bin$/)?.[1]);
    response.writeHead(200, { "content-type": "application/octet-stream" });
    response.end(Buffer.from(fixture.chunks[index]));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  });
  const base = `http://127.0.0.1:${server.address().port}/`;
  const worker = new Worker(`
    const { parentPort, workerData } = require('node:worker_threads');
    globalThis.self = globalThis;
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (url, init) => originalFetch(new URL(url, workerData.base), init);
    self.postMessage = (message, transfer = []) => parentPort.postMessage(message, transfer);
    parentPort.on('message', (data) => { void self.onmessage({ data }); });
    ${source}
  `, { eval: true, workerData: { base }, stderr: true });
  t.after(() => worker.terminate());
  worker.on("error", (error) => finished.resolve({ error }));
  worker.on("message", (message) => {
    messages.push(message);
    if (message.requestId === "latest" && message.type !== "PROGRESS") finished.resolve(message);
  });
  const send = (id) => worker.postMessage({ type: "DECRYPT_DATABASE", requestId: id,
    config: fixture.config, dek: b64(fixture.dek) });
  send("old");
  await completed(firstRequest.promise);
  send("latest");
  const result = await completed(finished.promise);
  assert.equal(result.type, "DECRYPT_SUCCESS", String(result.error));
  assert.deepEqual(new Uint8Array(result.dbBytes), fixture.plain);
  await completed(firstClosed.promise);
  assert.equal(requests, fixture.chunks.length + 1);
  const old = messages.filter((message) => message.requestId === "old" && message.type !== "PROGRESS");
  assert.equal(old.length, 1);
  assert.equal(old[0].type, "DECRYPT_FAILED");
  assert.match(old[0].error, /superseded/);
});
