/** Production worker admission with real AES-GCM/HKDF and a controlled Argon2
 * callback. The callback makes initialization/native interleavings observable;
 * these tests do not qualify actual Argon2-WASM or browser memory usage.
 * Run: node --test tests/pages/crypto-worker-unlock.test.mjs
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import vm from 'node:vm';
import { readFileSync } from 'node:fs';
import { webcrypto } from 'node:crypto';
import { setImmediate as turn } from 'node:timers/promises';

const source = readFileSync(process.env.CASS_CRYPTO_WORKER_SOURCE ||
  new URL('../../src/pages_assets/crypto_worker.js', import.meta.url), 'utf8');
const b64 = bytes => Buffer.from(bytes).toString('base64');
const zero = bytes => bytes.every(value => value === 0);
const KDF_KEY = new Uint8Array(32).fill(41);
const params = { memory_kb: 65536, iterations: 3, parallelism: 4 };
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((a, b) => { resolve = a; reject = b; });
  return { promise, resolve, reject };
};
async function until(predicate) {
  for (let i = 0; i < 1000; i++) {
    if (predicate()) return;
    await turn();
  }
  assert.ok(predicate(), 'expected asynchronous milestone');
}
async function settled(promise) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('request failed to settle')), 3000);
    })]);
  } finally { clearTimeout(timer); }
}
async function fixture(seed = 1) {
  const dek = new Uint8Array(32).fill(seed);
  const exportId = new Uint8Array(16).fill(seed + 1);
  const recovery = new Uint8Array(32).fill(seed + 2);
  const salt = new Uint8Array(16).fill(seed + 3);
  async function slot(id, type, rawKey) {
    const nonce = new Uint8Array(12).fill(id + seed);
    const key = await webcrypto.subtle.importKey('raw', rawKey, 'AES-GCM', false, ['encrypt']);
    const aad = new Uint8Array(17); aad.set(exportId); aad[16] = id;
    const wrapped = await webcrypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad }, key, dek);
    return { id, slot_type: type, kdf: type === 'password' ? 'argon2id' : 'hkdf-sha256',
      ...(type === 'password' ? { argon2_params: params } : {}),
      salt: b64(salt), nonce: b64(nonce), wrapped_dek: b64(wrapped) };
  }
  const key = await webcrypto.subtle.importKey('raw', recovery, 'HKDF', false, ['deriveBits']);
  const recoveryKey = await webcrypto.subtle.deriveBits({ name: 'HKDF', hash: 'SHA-256', salt,
    info: new TextEncoder().encode('cass-pages-kek-v2') }, key, 256);
  return { dek, recovery, config: { version: 2, compression: 'deflate', export_id: b64(exportId),
    base_nonce: b64(new Uint8Array(12)), kdf_defaults: params,
    key_slots: [await slot(1, 'password', KDF_KEY), await slot(2, 'recovery', recoveryKey)],
    payload: { chunk_count: 0, chunk_size: 128, files: [], total_compressed_size: 0, total_plaintext_size: 0 } } };
}
function harness(options = {}) {
  const messages = [], hashes = [], calls = [], bytes = [];
  let active = 0, peak = 0;
  const Uint8 = new Proxy(Uint8Array, { construct(target, args) {
    const value = Reflect.construct(target, args); bytes.push(value); return value;
  } });
  const subtle = {};
  for (const method of ['importKey', 'deriveBits', 'decrypt']) {
    subtle[method] = async (...args) => {
      calls.push(method);
      const result = await webcrypto.subtle[method](...args);
      await options[method]?.(args, result);
      return result;
    };
  }
  const sandbox = { Uint8Array: Uint8, ArrayBuffer, DataView, TextEncoder, TextDecoder, URL,
    AbortController, atob, btoa, setTimeout, clearTimeout, crypto: { subtle },
    console: { error() {}, warn() {}, debug() {} },
    postMessage(message) { messages.push(structuredClone(message)); },
    argon2: { ArgonType: { Argon2id: 2 }, async hash(input) {
      active++; peak = Math.max(peak, active);
      const call = { input, text: new TextDecoder().decode(input.pass), result: null };
      hashes.push(call);
      try {
        await options.hash?.(call, hashes.length);
        const result = { hash: new Uint8Array(KDF_KEY), hashHex: 'fixture', encoded: 'fixture' };
        call.result = result;
        return result;
      } finally { active--; }
    } },
  };
  sandbox.self = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox);
  const dispatch = (type, id, archive, extra = {}) => sandbox.onmessage({ data: {
    type, requestId: id, config: structuredClone(archive?.config), ...extra,
  } });
  return { messages, hashes, calls, bytes, sandbox, dispatch,
    peak: () => peak,
    password: (id, archive, password = id) => dispatch('UNLOCK_PASSWORD', id, archive, { password }),
    recovery: (id, archive) => dispatch('UNLOCK_RECOVERY', id, archive, { recoverySecret: b64(archive.recovery) }),
    clear: () => dispatch('CLEAR_KEYS', 'clear'),
    terminal: id => messages.filter(message => message.requestId === id && message.type !== 'PROGRESS'),
    evaluate: text => vm.runInContext(text, sandbox),
  };
}
function failed(f, id, pattern = /superseded/) {
  const messages = f.terminal(id); assert.equal(messages.length, 1, id);
  assert.equal(messages[0].type, 'UNLOCK_FAILED', id); assert.match(messages[0].error, pattern);
}
function success(f, id, archive) {
  assert.deepEqual(f.terminal(id), [{ type: 'UNLOCK_SUCCESS', dek: b64(archive.dek), requestId: id }]);
}

test('password burst admits one active derivation and only its latest waiter', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: (_, count) => count === 1 ? gate.promise : undefined });
  const first = f.password('first', archive);
  await until(() => f.hashes.length === 1);
  const pending = Array.from({ length: 80 }, (_, i) => f.password(`queued-${i}`, archive));
  await turn();
  const observed = { count: f.hashes.length, peak: f.peak() };
  gate.resolve(); await settled(Promise.all([first, ...pending]));
  assert.deepEqual(observed, { count: 1, peak: 1 }, 'superseded messages queued expensive KDF work');
  assert.deepEqual(f.hashes.map(call => call.text), ['first', 'queued-79']);
  failed(f, 'first');
  for (let i = 0; i < 79; i++) failed(f, `queued-${i}`);
  success(f, 'queued-79', archive);
  assert.ok(f.hashes.every(call => zero(call.input.pass) && zero(call.input.salt) && zero(call.result.hash)));
});

test('replaced waiting credentials are scrubbed before active crypto finishes', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: (_, count) => count === 1 ? gate.promise : undefined });
  const first = f.password('first', archive);
  await until(() => f.hashes.length === 1);
  const waiting = f.recovery('waiting', archive);
  const borrowed = f.evaluate('pendingUnlockJob.secret');
  const latest = f.password('latest', archive);
  await settled(waiting); failed(f, 'waiting');
  assert.ok(zero(borrowed), 'replaced credential bytes remain readable');
  assert.equal(f.hashes.length, 1);
  gate.resolve(); await settled(Promise.all([first, latest]));
  success(f, 'latest', archive);
  assert.equal(f.evaluate('activeUnlockJob === null && pendingUnlockJob === null'), true);
});

test('CLEAR_KEYS cancels the waiter without releasing an in-flight KDF slot', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: (_, count) => count === 1 ? gate.promise : undefined });
  const first = f.password('first', archive);
  await until(() => f.hashes.length === 1);
  const waiting = f.password('waiting', archive);
  await f.clear(); await settled(waiting); failed(f, 'waiting');
  const later = f.recovery('later', archive);
  await turn(); assert.equal(f.calls.length, 0, 'recovery crossed the retained active unlock slot');
  gate.resolve(); await settled(Promise.all([first, later]));
  failed(f, 'first'); success(f, 'later', archive);
  assert.equal(f.hashes.length, 1);
});

test('password and recovery share admission and cannot mix archive contexts', async t => {
  const a = await fixture(3), b = await fixture(9), gate = deferred(); t.after(gate.resolve);
  let held = false;
  const f = harness({ deriveBits: async () => { if (!held) { held = true; await gate.promise; } } });
  const first = f.recovery('recovery', a); await until(() => held);
  const discarded = f.password('discarded', a), latest = f.password('latest', b);
  await settled(discarded); failed(f, 'discarded');
  assert.equal(f.hashes.length, 0, 'password started while recovery derivation was active');
  gate.resolve(); await settled(Promise.all([first, latest]));
  failed(f, 'recovery'); success(f, 'latest', b);
});

test('late unwrapped key after reset is wiped and never published', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  let result;
  const f = harness({ decrypt: async (_, value) => { if (!result) { result = value; await gate.promise; } } });
  const first = f.password('first', archive); await until(() => result);
  await f.clear(); gate.resolve(); await settled(first);
  failed(f, 'first'); assert.ok(zero(new Uint8Array(result)));
  await settled(f.recovery('fresh', archive)); success(f, 'fresh', archive);
});

test('a decrypt request invalidates active and queued unlocks without stale success', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: (_, count) => count === 1 ? gate.promise : undefined });
  const first = f.password('first', archive); await until(() => f.hashes.length === 1);
  const queued = f.recovery('queued', archive);
  await settled(f.dispatch('DECRYPT_DATABASE', 'decrypt', archive, { dek: b64(archive.dek) }));
  await settled(queued); failed(f, 'queued');
  assert.equal(f.terminal('decrypt')[0].type, 'DECRYPT_SUCCESS');
  gate.resolve(); await settled(first); failed(f, 'first');
  assert.equal(f.evaluate('currentDek'), null);
});

test('invalid superseding input rejects the waiter and does not trigger derivation', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: (_, count) => count === 1 ? gate.promise : undefined });
  const first = f.password('first', archive); await until(() => f.hashes.length === 1);
  const queued = f.password('queued', archive);
  await settled(f.password('invalid', archive, ' '.repeat(3)));
  await settled(queued); failed(f, 'queued'); failed(f, 'invalid', /password/);
  gate.resolve(); await settled(first); failed(f, 'first');
  assert.equal(f.hashes.length, 1); assert.equal(f.calls.length, 0);
});

test('backend failure releases admission and the latest pending request can recover', async t => {
  const archive = await fixture(), gate = deferred(); t.after(gate.resolve);
  const f = harness({ hash: async (_, count) => { if (count === 1) { await gate.promise; throw Error('KDF fixture unavailable'); } } });
  const first = f.password('first', archive); await until(() => f.hashes.length === 1);
  const latest = f.password('latest', archive);
  gate.resolve(); await settled(Promise.all([first, latest]));
  failed(f, 'first', /KDF fixture unavailable|superseded/); success(f, 'latest', archive);
  assert.equal(f.peak(), 1); assert.ok(f.hashes.every(call => zero(call.input.pass)));
});

test('password slot fallback retains the original bytes and fixed Argon2 parameters', async () => {
  const archive = await fixture();
  const invalid = { ...archive.config.key_slots[0], id: 7 }; // wrong AES AAD, not invalid metadata
  archive.config.key_slots.unshift(invalid);
  const f = harness();
  await settled(f.password('fallback', archive, '  literal\u{1f511} password  '));
  success(f, 'fallback', archive);
  assert.deepEqual(f.hashes.map(call => call.text), ['  literal🔑 password  ', '  literal🔑 password  ']);
  for (const { input } of f.hashes) {
    assert.equal(input.mem, 65536); assert.equal(input.time, 3); assert.equal(input.parallelism, 4);
    assert.equal(input.type, 2); assert.equal(input.hashLen, 32);
  }
});

test('unlock credential byte limits refuse oversized inputs before native work', async () => {
  const archive = await fixture(), f = harness();
  for (const [id, password] of [['ascii', 'x'.repeat(65537)], ['unicode', '🔑'.repeat(16385)]]) {
    await settled(f.password(id, archive, password)); failed(f, id, /limit|64 KiB/);
  }
  await settled(f.dispatch('UNLOCK_RECOVERY', 'recovery-limit', archive,
    { recoverySecret: new Uint8Array(65537).fill(1) }));
  failed(f, 'recovery-limit', /limit|64 KiB/);
  assert.equal(f.hashes.length, 0); assert.equal(f.calls.length, 0);
  await settled(f.password('boundary', archive, '🔑'.repeat(16384)));
  success(f, 'boundary', archive);
});

test('wrong password and malformed KDF output remain retryable without leaked keys', async () => {
  const archive = await fixture(), f = harness();
  f.sandbox.argon2.hash = async () => ({ hash: new Uint8Array(32).fill(19) });
  await settled(f.password('wrong', archive)); failed(f, 'wrong', /Incorrect password/);
  const short = new Uint8Array(31).fill(3);
  f.sandbox.argon2.hash = async () => ({ hash: short });
  await settled(f.password('short', archive)); failed(f, 'short', /invalid key/);
  assert.ok(zero(short));
  await settled(f.recovery('recover', archive)); success(f, 'recover', archive);
});

// Model the vendor's lazy binary promise (including its rejected-promise cache).
// Actual download/stream/cancellation code is production; no WASM algorithm is
// replaced in the shipping worker. The injected hash is the same control above.
function lazyRuntime(f, fetch) {
  const hash = f.sandbox.argon2.hash;
  const deadlines = new Map(); let scriptLoads = 0, id = 0;
  f.sandbox.argon2 = null;
  f.sandbox.location = { href: 'https://example.test/archive/crypto_worker.js' };
  f.sandbox.fetch = fetch;
  f.sandbox.setTimeout = (callback, delay) => {
    assert.equal(delay, 120_000); deadlines.set(++id, callback); return id;
  };
  f.sandbox.clearTimeout = id => deadlines.delete(id);
  f.sandbox.importScripts = path => {
    assert.equal(path, './vendor/argon2.js'); scriptLoads++;
    let modulePromise;
    f.sandbox.argon2 = { ArgonType: { Argon2id: 2 }, hash: input => {
      modulePromise ||= f.sandbox.loadArgon2WasmBinary();
      return modulePromise.then(() => hash(input));
    } };
  };
  return { deadlines, loads: () => scriptLoads };
}

test('superseding recovery aborts stalled WASM fetch and escapes its cancelled password job', async () => {
  const archive = await fixture(), f = harness(); let signal;
  const runtime = lazyRuntime(f, (_, init) => { signal = init?.signal; return new Promise(() => {}); });
  const password = f.password('loading', archive);
  await until(() => f.sandbox.loadArgon2WasmBinary);
  await turn();
  const recovery = f.recovery('recovery', archive);
  await settled(Promise.all([password, recovery]));
  failed(f, 'loading'); success(f, 'recovery', archive);
  assert.ok(signal.aborted); assert.equal(f.hashes.length, 0); assert.equal(runtime.deadlines.size, 0);
});

for (const stalled of ['headers', 'body']) {
  test(`WASM ${stalled} timeout retires the failed cached runtime and a later password retries`, async () => {
    const archive = await fixture(), f = harness(); let requests = 0, cancelled = 0;
    const runtime = lazyRuntime(f, () => {
      requests++;
      if (requests > 1) return new Response(new Uint8Array([0, 97, 115, 109]));
      return stalled === 'headers' ? new Promise(() => {}) : new Response(new ReadableStream({
        cancel() { cancelled++; return new Promise(() => {}); },
      }));
    });
    const first = f.password('timeout', archive);
    await until(() => requests === 1); await turn();
    assert.equal(runtime.deadlines.size, 1);
    [...runtime.deadlines.values()][0]();
    await settled(first); failed(f, 'timeout', /Argon2 WASM runtime.*timed out/);
    if (stalled === 'body') assert.equal(cancelled, 1);
    assert.equal(runtime.deadlines.size, 0); assert.equal(f.hashes.length, 0);
    await settled(f.password('retry', archive)); success(f, 'retry', archive);
    assert.equal(runtime.loads(), 2, 'rejected vendor module promise was reused');
    assert.equal(requests, 2); assert.equal(f.hashes.length, 1);
  });
}

test('failed runtime HTTP response is closed and retry does not reuse rejection', async () => {
  const archive = await fixture(), f = harness(); let requests = 0, cancelled = 0;
  const runtime = lazyRuntime(f, () => ++requests === 1
    ? new Response(new ReadableStream({ cancel() { cancelled++; } }), { status: 503 })
    : new Response(new Uint8Array([0, 97, 115, 109])));
  await settled(f.password('offline', archive)); failed(f, 'offline', /503/);
  assert.equal(f.hashes.length, 0);
  await settled(f.password('online', archive)); success(f, 'online', archive);
  assert.equal(cancelled, 1);
  assert.equal(runtime.loads(), 2); assert.equal(runtime.deadlines.size, 0);
});

test('late cancelled runtime response is disposed without any stale derivation', async () => {
  const archive = await fixture(), f = harness(), headers = deferred(); let requests = 0, cancelled = 0;
  lazyRuntime(f, () => { requests++; return headers.promise; });
  const first = f.password('loading', archive); await until(() => requests === 1);
  await f.clear(); await settled(first); failed(f, 'loading');
  headers.resolve(new Response(new ReadableStream({ cancel() { cancelled++; } })));
  await until(() => cancelled === 1); assert.equal(f.hashes.length, 0);
});

test('oversized runtime stream is refused before KDF and every reader is released', async () => {
  const archive = await fixture(), f = harness(); let cancelled = 0, released = 0;
  const huge = new Uint8Array(1024 * 1024 + 1).fill(3);
  const runtime = lazyRuntime(f, () => ({ ok: true, body: { getReader() { return {
    read: async () => ({ done: false, value: huge }),
    cancel() { cancelled++; return new Promise(() => {}); },
    releaseLock() { released++; },
  }; } } }));
  await settled(f.password('oversize', archive)); failed(f, 'oversize', /download limit/);
  assert.ok(zero(huge)); assert.equal(cancelled, 1); assert.equal(released, 1);
  assert.equal(f.hashes.length, 0); assert.equal(runtime.deadlines.size, 0);
});


test('successful runtime is reused but each new request derives its own key', async () => {
  const archive = await fixture(), f = harness(); let requests = 0;
  const runtime = lazyRuntime(f, () => { requests++; return new Response(new Uint8Array([0, 97, 115, 109])); });
  for (const id of ['first', 'second', 'third']) {
    await settled(f.password(id, archive)); success(f, id, archive);
  }
  assert.equal(requests, 1); assert.equal(runtime.loads(), 1);
  assert.deepEqual(f.hashes.map(call => call.text), ['first', 'second', 'third']);
  assert.equal(runtime.deadlines.size, 0);
});
