// Production classic worker, real WebCrypto AES-GCM/HKDF and native raw DEFLATE.
// VM tests control only transport scheduling and optional native-call pauses.
// They do not establish browser RSS, SQLite-WASM startup, or Argon2 performance.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { webcrypto } from 'node:crypto';
import { deflateRawSync } from 'node:zlib';
import vm from 'node:vm';
import test from 'node:test';
import { setImmediate as turn } from 'node:timers/promises';

const sourcePath = process.env.CASS_CRYPTO_WORKER_SOURCE ||
  new URL('../src/pages_assets/crypto_worker.js', import.meta.url);
const source = await readFile(sourcePath, 'utf8');
const b64 = bytes => Buffer.from(bytes).toString('base64');
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
async function waitFor(predicate, message = 'condition did not become true') {
  const started = Date.now();
  while (!predicate()) {
    if (Date.now() - started > 3000) assert.fail(message);
    await turn();
  }
}
async function bounded(promise) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('worker failed to settle')), 3000);
    })]);
  } finally { clearTimeout(timer); }
}

async function archive(count = 9, size = 1024) {
  const keyBytes = Uint8Array.from({ length: 32 }, (_, i) => i + 1);
  const nonce = Uint8Array.from({ length: 12 }, (_, i) => i + 11);
  const exportId = Uint8Array.from({ length: 16 }, (_, i) => i + 42);
  const total = count ? (count - 1) * size + Math.ceil(size / 2) : 0;
  const plaintext = Uint8Array.from({ length: total }, (_, i) => (i * 31 + Math.floor(i / 97)) % 251);
  const key = await webcrypto.subtle.importKey('raw', keyBytes, 'AES-GCM', false, ['encrypt']);
  const chunks = [];
  for (let i = 0; i < count; i++) {
    const iv = new Uint8Array(nonce);
    new DataView(iv.buffer).setUint32(8, i, false);
    const aad = new Uint8Array(21);
    aad.set(exportId);
    new DataView(aad.buffer).setUint32(16, i, false);
    aad[20] = 2;
    chunks.push(new Uint8Array(await webcrypto.subtle.encrypt({
      name: 'AES-GCM', iv, additionalData: aad,
    }, key, deflateRawSync(plaintext.subarray(i * size, (i + 1) * size)))));
  }
  const secret = new Uint8Array(32).fill(73);
  const salt = new Uint8Array(16).fill(81);
  const hkdf = await webcrypto.subtle.importKey('raw', secret, 'HKDF', false, ['deriveBits']);
  const kekBytes = await webcrypto.subtle.deriveBits({
    name: 'HKDF', hash: 'SHA-256', salt, info: new TextEncoder().encode('cass-pages-kek-v2'),
  }, hkdf, 256);
  const kek = await webcrypto.subtle.importKey('raw', kekBytes, 'AES-GCM', false, ['encrypt']);
  const slotAad = new Uint8Array(17); slotAad.set(exportId);
  const wrapped = await webcrypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: slotAad }, kek, keyBytes);
  const config = {
    version: 2, compression: 'deflate', export_id: b64(exportId), base_nonce: b64(nonce),
    kdf_defaults: { memory_kb: 65536, iterations: 3, parallelism: 4 },
    key_slots: [{ id: 0, slot_type: 'recovery', kdf: 'hkdf-sha256', salt: b64(salt), wrapped_dek: b64(wrapped), nonce: b64(nonce) }],
    payload: { chunk_count: count, chunk_size: size, total_plaintext_size: total,
      total_compressed_size: chunks.reduce((n, bytes) => n + bytes.length, 0),
      files: chunks.map((_, i) => `payload/chunk-${String(i).padStart(5, '0')}.bin`) },
  };
  return { config, key: b64(keyBytes), secret: b64(secret), chunks, plaintext };
}
function response(bytes, headers = {}) {
  return new Response(new Uint8Array(bytes), { headers });
}
function indexOf(url) {
  const match = /\/chunk-(\d+)\.bin$/.exec(String(url));
  assert.ok(match, `unexpected URL ${url}`);
  return Number(match[1]);
}
function worker(text = source, options = {}) {
  const messages = [], requests = [], buffers = [], timers = new Map();
  let decryptCalls = 0;
  const subtle = new Proxy(webcrypto.subtle, {
    get(target, key) {
      if (key === 'decrypt') return async (...args) => {
        const call = ++decryptCalls;
        if (options.beforeDecrypt) await options.beforeDecrypt(call, args);
        return target.decrypt(...args);
      };
      const value = target[key];
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  const self = {
    DecompressionStream,
    location: { href: 'https://archive.invalid/crypto_worker.js' },
    postMessage(message, transfer = []) {
      messages.push(structuredClone(message, { transfer }));
      options.onMessage?.(message);
    },
  };
  const context = vm.createContext({
    self, crypto: { subtle }, Uint8Array, ArrayBuffer, DataView, TextEncoder,
    AbortController, atob, btoa, URL, Promise,
    console: { debug() {}, error() {}, warn() {} },
    setTimeout(callback, ms) {
      if (options.manualTimeout && ms === 120000) {
        const token = {}; timers.set(token, callback); return token;
      }
      return setTimeout(callback, ms);
    },
    clearTimeout(token) {
      if (!timers.delete(token)) clearTimeout(token);
    },
    async fetch(url, init) {
      const call = { index: indexOf(url), signal: init?.signal };
      requests.push(call);
      return options.fetch(call, url, init);
    },
    __buffers: buffers,
  });
  vm.runInContext(text, context, { filename: String(sourcePath) });
  // Observe actual allocated response buffers, without substituting their data.
  vm.runInContext(`const testRead = readResponseBytesBounded;
    readResponseBytesBounded = async (...args) => {
      const bytes = await testRead(...args); __buffers.push(bytes); return bytes;
    };`, context);
  return {
    messages, requests, buffers, timers,
    evaluate: expression => vm.runInContext(expression, context),
    get decryptCalls() { return decryptCalls; },
    send: data => self.onmessage({ data }),
    decrypt: (fixture, requestId = 'decrypt') => self.onmessage({ data: {
      type: 'DECRYPT_DATABASE', requestId, dek: fixture.key, config: fixture.config,
    } }),
    clear: () => self.onmessage({ data: { type: 'CLEAR_KEYS' } }),
  };
}
function success(runtime, fixture, id = 'decrypt') {
  const terminal = runtime.messages.filter(m => m.requestId === id && m.type.startsWith('DECRYPT_'));
  assert.equal(terminal.length, 1);
  assert.equal(terminal[0].type, 'DECRYPT_SUCCESS', JSON.stringify(terminal[0]));
  assert.deepEqual(new Uint8Array(terminal[0].dbBytes), fixture.plaintext);
  assert.equal(terminal[0].dbSize, fixture.plaintext.length);
  assert.equal(runtime.evaluate('activeDatabaseJob'), null);
  assert.equal(runtime.evaluate('pendingDatabaseJob'), null);
  assert.ok(runtime.buffers.every(bytes => bytes.every(byte => byte === 0)));
}
function failure(runtime, pattern, id = 'decrypt') {
  const terminal = runtime.messages.filter(m => m.requestId === id && m.type.startsWith('DECRYPT_'));
  assert.equal(terminal.length, 1);
  assert.equal(terminal[0].type, 'DECRYPT_FAILED');
  assert.match(terminal[0].error, pattern);
  assert.ok(runtime.buffers.every(bytes => bytes.every(byte => byte === 0)));
}

async function initialWindow(text, fixture) {
  const waiting = [];
  let released = false;
  const runtime = worker(text, { fetch(call) {
    if (released) return response(fixture.chunks[call.index]);
    const gate = deferred(); waiting.push({ gate, call }); return gate.promise;
  } });
  const done = runtime.decrypt(fixture);
  await waitFor(() => waiting.length > 0);
  // Let all microtasks from the same admission settle, without releasing bytes.
  for (let i = 0; i < 4; i++) await turn();
  const width = waiting.length;
  released = true;
  // Later headers/body finish first. Decryption must nevertheless stay ordered.
  for (const { gate, call } of waiting.toReversed()) gate.resolve(response(fixture.chunks[call.index]));
  await bounded(done);
  success(runtime, fixture);
  assert.equal(runtime.decryptCalls, fixture.chunks.length);
  assert.deepEqual(runtime.requests.map(r => r.index), fixture.chunks.map((_, i) => i));
  return { width, runtime };
}

test('starts four bounded transfers before the first body finishes; live serial control agrees on bytes', async t => {
  const fixture = await archive(13);
  const actual = await initialWindow(source, fixture);
  assert.equal(actual.width, 4, 'serial network round trips still block archive startup');
  const incumbentPath = process.env.CASS_CRYPTO_INCUMBENT;
  if (incumbentPath) {
    const incumbent = await initialWindow(await readFile(incumbentPath, 'utf8'), fixture);
    assert.equal(incumbent.width, 1, 'control is not the retained serial implementation');
    assert.equal(actual.runtime.decryptCalls, incumbent.runtime.decryptCalls);
    t.diagnostic('same invocation, same encrypted fixture: initial outstanding downloads 1 -> 4; output byte-identical');
  }
});

test('completed ciphertext behind a held native call keeps its credit, then is scrubbed on CLEAR_KEYS', async () => {
  const fixture = await archive(40);
  const held = deferred();
  const runtime = worker(source, {
    fetch: call => response(fixture.chunks[call.index]),
    beforeDecrypt: call => call === 1 ? held.promise : undefined,
  });
  const done = runtime.decrypt(fixture);
  await waitFor(() => runtime.decryptCalls === 1 && runtime.buffers.length >= 4);
  for (let i = 0; i < 8; i++) await turn();
  assert.equal(runtime.requests.length, 4, 'completed transfers released credit before decryption');
  const database = runtime.evaluate('activeDatabaseJob.dbBytes');
  await runtime.clear();
  assert.ok(database.every(byte => byte === 0));
  assert.ok(runtime.buffers.every(bytes => bytes.every(byte => byte === 0)));
  assert.notEqual(runtime.evaluate('activeDatabaseJob'), null, 'native work must keep its admission');
  held.resolve();
  await bounded(done);
  failure(runtime, /superseded/);
  assert.equal(runtime.evaluate('activeDatabaseJob'), null);
  assert.equal(runtime.requests.length, 4);
});

test('large ciphertext upper bounds reduce concurrency instead of multiplying maximum-size buffers', async () => {
  // Directly exercise scheduling without allocating a 512 MiB dummy database.
  const runtime = worker(source, { manualTimeout: true, fetch: () => new Promise(() => {}) });
  const size = 32 * 1024 * 1024;
  const payload = { chunk_count: 4, chunk_size: size, total_compressed_size: size * 4,
    files: Array.from({ length: 4 }, (_, i) => `payload/chunk-${String(i).padStart(5, '0')}.bin`) };
  runtime.evaluate(`globalThis.testController = new AbortController(); globalThis.testPayload = ${JSON.stringify(payload)};`);
  const done = runtime.evaluate('(async () => { try { for await (const ignored of prefetchDatabaseChunks(testPayload, testController)) {} } catch(error) { return error.message; } })()');
  await waitFor(() => runtime.requests.length > 0);
  for (let i = 0; i < 4; i++) await turn();
  assert.equal(runtime.requests.length, 1);
  runtime.evaluate('testController.abort(new Error("test end"))');
  assert.equal(await bounded(done), 'test end');
  assert.equal(runtime.timers.size, 0);
});

test('a later HTTP failure cancels an earlier stalled response without waiting for its deadline', async () => {
  const fixture = await archive();
  const runtime = worker(source, { manualTimeout: true, fetch: call =>
    call.index === 2 ? new Response(null, { status: 503 }) : new Promise(() => {}) });
  await bounded(runtime.decrypt(fixture));
  failure(runtime, /503/);
  assert.ok(runtime.requests.every(call => call.signal.aborted));
  assert.equal(runtime.timers.size, 0);
  assert.equal(runtime.evaluate('activeDatabaseJob'), null);
});

test('tampering or swapping later ciphertext cannot publish a partially decoded database', async () => {
  const fixture = await archive(7);
  for (const corruption of ['tamper', 'swap']) {
    const runtime = worker(source, { fetch(call) {
      const bytes = new Uint8Array(fixture.chunks[call.index === 2 && corruption === 'swap' ? 3 : call.index]);
      if (call.index === 2 && corruption === 'tamper') bytes[0] ^= 1;
      return response(bytes);
    } });
    await bounded(runtime.decrypt(fixture));
    failure(runtime, /Failed to decrypt chunk 2/);
    assert.equal(runtime.evaluate('activeDatabaseJob'), null);
    assert.ok(runtime.requests.length <= 6, 'failed authentication kept scheduling the archive');
  }
});

test('a failed future transfer scrubs accumulated plaintext even while native decryption is held', async () => {
  const fixture = await archive(12);
  const hold = deferred(), later = deferred();
  const runtime = worker(source, {
    fetch: call => call.index === 4 ? later.promise : response(fixture.chunks[call.index]),
    beforeDecrypt: call => call === 2 ? hold.promise : undefined,
  });
  const done = runtime.decrypt(fixture);
  await waitFor(() => runtime.decryptCalls === 2 && runtime.requests.length >= 5);
  const buffer = runtime.evaluate('activeDatabaseJob.dbBytes');
  assert.deepEqual(buffer.subarray(0, 1024), fixture.plaintext.subarray(0, 1024));
  later.reject(new Error('injected transport failure'));
  await waitFor(() => runtime.evaluate('activeDatabaseJob.controller.signal.aborted'));
  assert.ok(buffer.every(byte => byte === 0));
  hold.resolve();
  await bounded(done);
  failure(runtime, /injected transport failure/);
});

test('supersession starts only the latest pending request after the native call drains', async () => {
  const fixture = await archive(10), hold = deferred();
  const runtime = worker(source, {
    fetch: call => response(fixture.chunks[call.index]),
    beforeDecrypt: call => call === 1 ? hold.promise : undefined,
  });
  const jobs = [runtime.decrypt(fixture, 'first')];
  await waitFor(() => runtime.decryptCalls === 1);
  for (let i = 0; i < 30; i++) jobs.push(runtime.decrypt(fixture, `next-${i}`));
  for (let i = 0; i < 4; i++) await turn();
  assert.equal(runtime.requests.length, 4);
  assert.equal(runtime.evaluate('pendingDatabaseJob.requestId'), 'next-29');
  hold.resolve();
  await bounded(Promise.all(jobs));
  success(runtime, fixture, 'next-29');
  assert.equal(runtime.messages.filter(m => m.type === 'DECRYPT_SUCCESS').length, 1);
  assert.equal(runtime.messages.filter(m => m.type === 'DECRYPT_FAILED').length, 30);
  assert.equal(runtime.requests.length, fixture.chunks.length + 4);
});

test('per-chunk timeout aborts every prefetched sibling and drops all timer registrations', async () => {
  const fixture = await archive();
  const runtime = worker(source, { manualTimeout: true, fetch: () => new Promise(() => {}) });
  const done = runtime.decrypt(fixture);
  await waitFor(() => runtime.requests.length === 4);
  assert.equal(runtime.timers.size, 4);
  [...runtime.timers.values()][2]();
  await bounded(done);
  failure(runtime, /download timed out/);
  assert.equal(runtime.timers.size, 0);
  assert.ok(runtime.requests.every(call => call.signal.aborted));
});

test('aggregate ciphertext declarations are enforced even when downloads complete out of order', async () => {
  const fixture = await archive(8);
  for (const adjustment of [-1, 1]) {
    const changed = { ...fixture, config: structuredClone(fixture.config) };
    changed.config.payload.total_compressed_size += adjustment;
    const runtime = worker(source, { fetch: call => response(fixture.chunks[call.index]) });
    await bounded(runtime.decrypt(changed));
    failure(runtime, /Encrypted payload (exceeds|size mismatch)/);
    assert.equal(runtime.evaluate('activeDatabaseJob'), null);
  }
});

test('empty archives and recovery-unlocked archives retain the existing wire and cryptographic contract', async () => {
  for (const count of [0, 1, 5]) {
    const fixture = await archive(count);
    const runtime = worker(source, { fetch: call => response(fixture.chunks[call.index]) });
    await bounded(runtime.send({ type: 'UNLOCK_RECOVERY', requestId: 'key',
      recoverySecret: fixture.secret, config: fixture.config }));
    const unlocked = runtime.messages.find(m => m.type === 'UNLOCK_SUCCESS');
    assert.equal(unlocked?.dek, fixture.key);
    await bounded(runtime.decrypt({ ...fixture, key: unlocked.dek }));
    success(runtime, fixture);
    assert.equal(runtime.requests.length, count);
    assert.equal(runtime.decryptCalls, count + 1);
  }
});
