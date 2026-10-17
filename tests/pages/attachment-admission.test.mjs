// node --test tests/pages/attachment-admission.test.mjs
// Real WebCrypto/HKDF/AES-GCM and streamed Fetch Response bodies. A private
// module copy uses small resource ceilings to exercise the production paths.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { webcrypto } from "node:crypto";
import { test } from "node:test";

const source = await readFile(new URL("../../src/pages_assets/attachments.js", import.meta.url), "utf8");
const dek = new Uint8Array(32).fill(7);
const exportId = new Uint8Array(16).fill(9);
const hash = (n) => n.toString(16).padStart(64, "0");
let serial = 0;
const tick = () => new Promise((resolve) => setImmediate(resolve));

async function fixture(t, { timeout = 2000 } = {}) {
  t.mock.method(console, "warn", () => {});
  const module = await import(`data:text/javascript;base64,${Buffer.from(source + `
    CACHE_CONFIG.MAX_SIZE_BYTES = 256;
    LOAD_CONFIG.MAX_MANIFEST_BYTES = 512;
    LOAD_CONFIG.MAX_CONCURRENT = 2;
    LOAD_CONFIG.MAX_PENDING = 2;
    LOAD_CONFIG.TIMEOUT_MS = ${timeout};
    export function loadState() { return [activeLoads.size, pendingLoads.length]; }
    // Unique namespace, without any production-only test export.
    // ${++serial}
  `).toString("base64")}`);
  t.after(() => module.reset());
  return module;
}

async function encrypted(identifier, plaintext) {
  const encoder = new TextEncoder();
  const material = await webcrypto.subtle.importKey("raw", encoder.encode(identifier), "HKDF", false, ["deriveBits"]);
  const nonce = await webcrypto.subtle.deriveBits({
    name: "HKDF", hash: "SHA-256", salt: encoder.encode("cass-blob-nonce-v1"), info: encoder.encode("nonce"),
  }, material, 96);
  const key = await webcrypto.subtle.importKey("raw", dek, "AES-GCM", false, ["encrypt"]);
  const aad = identifier === "manifest" ? exportId : Buffer.concat([exportId, Buffer.from(identifier, "hex")]);
  return new Uint8Array(await webcrypto.subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: aad }, key, plaintext));
}

function streamed(bytes, { fragment = 11, headers, cancel } = {}) {
  let offset = 0;
  return new Response(new ReadableStream({
    pull(controller) {
      if (offset === bytes.length) { controller.close(); return; }
      const end = Math.min(offset + fragment, bytes.length);
      controller.enqueue(bytes.slice(offset, end));
      offset = end;
    },
    cancel,
  }), { headers });
}

function code(expected) { return (error) => error.code === expected; }

test("streamed authenticated blobs round-trip, coalesce and share MIME-specific URLs", async (t) => {
  const m = await fixture(t);
  const plain = new TextEncoder().encode("archived attachment contents");
  const cipher = await encrypted(hash(1), plain);
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async () => { fetches++; return streamed(cipher, { fragment: 1 }); });
  const [a, b] = await Promise.all([m.loadBlob(hash(1), dek, exportId), m.loadBlob(hash(1), dek, exportId)]);
  assert.deepEqual(a, plain);
  assert.strictEqual(a, b);
  assert.equal(fetches, 1);
  const url = await m.loadBlobAsUrl(hash(1), "application/octet-stream", dek, exportId);
  assert.equal(await m.loadBlobAsUrl(hash(1), "application/octet-stream", dek, exportId), url);
  const other = await m.loadBlobAsUrl(hash(1), "text/plain", dek, exportId);
  assert.notEqual(url, other);
  m.reset();
  assert.equal(a.every((x) => x === 0), true, "reset scrubs retained plaintext");
  assert.equal(m.getCacheStats().entries, 0);
});

test("oversized Content-Length is refused before reading or allocating its body", async (t) => {
  const m = await fixture(t);
  let reads = 0;
  let cancels = 0;
  t.mock.method(globalThis, "fetch", async () => ({
    ok: true, headers: new Headers({ "Content-Length": "273" }),
    body: { cancel: async () => { cancels++; }, getReader() { reads++; throw Error("must not read"); } },
  }));
  await assert.rejects(m.loadBlob(hash(1), dek, exportId), code("ATTACHMENT_TOO_LARGE"));
  assert.equal(reads, 0);
  assert.equal(cancels, 1);
  assert.equal(m.getCacheStats().sizeBytes, 0);
});

test("missing and dishonest lengths cannot bypass the streaming byte cap", async (t) => {
  const m = await fixture(t);
  for (const headers of [undefined, { "content-length": "1" }]) {
    let cancellations = 0;
    t.mock.method(globalThis, "fetch", async () => streamed(new Uint8Array(400), {
      fragment: 17, headers, cancel() { cancellations++; },
    }));
    await assert.rejects(m.loadBlob(hash(1), dek, exportId), code("ATTACHMENT_TOO_LARGE"));
    assert.equal(cancellations, 1);
    assert.equal(m.getCacheStats().entries, 0);
  }
});

test("the exact limit succeeds and eviction never grows the retained cache above it", async (t) => {
  const m = await fixture(t);
  const cipher1 = await encrypted(hash(1), new Uint8Array(256).fill(3));
  const cipher2 = await encrypted(hash(2), new Uint8Array(1).fill(4));
  t.mock.method(globalThis, "fetch", async (url) => streamed(url.includes(hash(1)) ? cipher1 : cipher2));
  assert.equal((await m.loadBlob(hash(1), dek, exportId)).length, 256);
  assert.equal(m.getCacheStats().sizeBytes, 256);
  assert.deepEqual(await m.loadBlob(hash(2), dek, exportId), new Uint8Array([4]));
  assert.equal(m.getCacheStats().entries, 1);
  assert.equal(m.getCacheStats().sizeBytes, 1);
});

test("bad authentication releases admission and remains retryable", async (t) => {
  const m = await fixture(t);
  const cipher = await encrypted(hash(1), new Uint8Array([4, 5, 6]));
  let attempt = 0;
  t.mock.method(globalThis, "fetch", async () => {
    const body = cipher.slice();
    if (attempt++ === 0) body[0] ^= 1;
    return streamed(body);
  });
  await assert.rejects(m.loadBlob(hash(1), dek, exportId), { name: "OperationError" });
  assert.equal(m.getCacheStats().entries, 0);
  assert.deepEqual(await m.loadBlob(hash(1), dek, exportId), new Uint8Array([4, 5, 6]));
  await tick();
  assert.deepEqual(m.loadState(), [0, 0]);
});

test("concurrency and queue admission are bounded; reset cancels active and queued work", async (t) => {
  const m = await fixture(t);
  let fetches = 0;
  let aborts = 0;
  t.mock.method(globalThis, "fetch", (_url, { signal }) => new Promise((_resolve, reject) => {
    fetches++;
    signal.addEventListener("abort", () => { aborts++; reject(signal.reason); }, { once: true });
  }));
  const requests = Array.from({ length: 5 }, (_, i) => m.loadBlob(hash(i + 1), dek, exportId));
  const settled = Promise.allSettled(requests);
  await tick();
  assert.equal(fetches, 2);
  assert.deepEqual(m.loadState(), [2, 2]);
  m.reset();
  const result = await settled;
  assert.equal(aborts, 2);
  assert.equal(fetches, 2, "queued work never fetches after lock/reset");
  for (const r of result.slice(0, 4)) {
    assert.equal(r.status, "rejected");
    assert.equal(r.reason.code, "ATTACHMENT_REQUEST_INVALIDATED");
  }
  assert.equal(result[4].reason.code, "ATTACHMENT_LOAD_QUEUE_FULL");
  await tick();
  assert.deepEqual(m.loadState(), [0, 0]);
  const cipher = await encrypted(hash(8), new Uint8Array([8]));
  t.mock.method(globalThis, "fetch", async () => streamed(cipher));
  assert.deepEqual(await m.loadBlob(hash(8), dek, exportId), new Uint8Array([8]));
});

test("reset cancels a body stalled between chunks, without stale cache publication", async (t) => {
  const m = await fixture(t);
  let cancelled = false;
  t.mock.method(globalThis, "fetch", async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array([1])); },
    cancel() { cancelled = true; },
  })));
  const request = m.loadBlob(hash(1), dek, exportId);
  const rejected = assert.rejects(request, code("ATTACHMENT_REQUEST_INVALIDATED"));
  await tick();
  m.reset();
  await rejected;
  assert.equal(cancelled, true);
  assert.equal(m.getCacheStats().entries, 0);
});

test("a stalled body times out and frees the slot for the next request", async (t) => {
  const m = await fixture(t, { timeout: 20 });
  let cancelled = false;
  t.mock.method(globalThis, "fetch", async () => new Response(new ReadableStream({
    cancel() { cancelled = true; },
  })));
  await assert.rejects(m.loadBlob(hash(1), dek, exportId), code("ATTACHMENT_REQUEST_TIMED_OUT"));
  await tick();
  assert.equal(cancelled, true);
  assert.deepEqual(m.loadState(), [0, 0]);
});

test("manifest decryption shares admission, enforces its own byte cap and retries failures", async (t) => {
  const m = await fixture(t);
  const plain = new TextEncoder().encode(JSON.stringify({ entries: [{
    hash: hash(1), filename: "notes.txt", mime_type: "text/plain", size_bytes: 6, message_id: 1,
  }], total_size_bytes: 6 }));
  const cipher = await encrypted("manifest", plain);
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async () => {
    fetches++;
    return fetches === 1 ? streamed(new Uint8Array(600)) : streamed(cipher);
  });
  await assert.rejects(m.initAttachments(dek, exportId), code("ATTACHMENT_TOO_LARGE"));
  assert.equal(m.hasAttachments(), false);
  const [a, b] = await Promise.all([m.initAttachments(dek, exportId), m.initAttachments(dek, exportId)]);
  assert.strictEqual(a, b);
  assert.equal(fetches, 2);
  assert.equal(m.getMessageAttachments(1)[0].filename, "notes.txt");
});

test("manifest absence is cached, transient HTTP failure is not", async (t) => {
  const m = await fixture(t);
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async () => new Response(null, { status: ++fetches === 1 ? 503 : 404 }));
  await assert.rejects(m.initAttachments(dek, exportId), code("ATTACHMENT_MANIFEST_FETCH_FAILED"));
  assert.equal(await m.initAttachments(dek, exportId), null);
  assert.equal(await m.initAttachments(dek, exportId), null);
  assert.equal(fetches, 2);
});

test("non-streaming transport compatibility still checks the materialized length", async (t) => {
  const m = await fixture(t);
  t.mock.method(globalThis, "fetch", async () => ({ ok: true, arrayBuffer: async () => new Uint8Array(273).buffer }));
  await assert.rejects(m.loadBlob(hash(1), dek, exportId), code("ATTACHMENT_TOO_LARGE"));
});

test("reset retains admission until non-cancellable crypto settles and scrubs its result", { timeout: 5000 }, async (t) => {
  const m = await fixture(t);
  const cipher = new Map(await Promise.all([1, 2, 3].map(async (i) => [hash(i), await encrypted(hash(i), new Uint8Array([i]))])));
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async (url) => { fetches++; return streamed(cipher.get(url.slice(8, -4))); });
  const decrypt = globalThis.crypto.subtle.decrypt.bind(globalThis.crypto.subtle);
  const held = [];
  let reachedBoth;
  const bothDecrypting = new Promise((resolve) => { reachedBoth = resolve; });
  t.mock.method(globalThis.crypto.subtle, "decrypt", async (...args) => {
    const plaintext = await decrypt(...args);
    if (held.length < 2) {
      await new Promise((release) => {
        held.push({ plaintext, release });
        if (held.length === 2) reachedBoth();
      });
    }
    return plaintext;
  });
  const old = [m.loadBlob(hash(1), dek, exportId), m.loadBlob(hash(2), dek, exportId)];
  const oldSettled = Promise.allSettled(old);
  await bothDecrypting;
  assert.equal(held.length, 2, "both admitted jobs reached real AES-GCM decryption");
  m.reset();
  const fresh = m.loadBlob(hash(3), dek, exportId);
  await tick();
  assert.deepEqual(m.loadState(), [2, 1]);
  assert.equal(fetches, 2, "new epoch cannot bypass occupied crypto slots");
  for (const item of held) item.release();
  for (const result of await oldSettled) assert.equal(result.reason.code, "ATTACHMENT_REQUEST_INVALIDATED");
  for (const item of held) assert.equal(new Uint8Array(item.plaintext).every((x) => x === 0), true);
  assert.deepEqual(await fresh, new Uint8Array([3]));
});

test("reset disconnects lazy observers and old DOM callbacks cannot restart downloads", async (t) => {
  const m = await fixture(t);
  const observers = [];
  const elements = [];
  const documentBefore = globalThis.document;
  const observerBefore = globalThis.IntersectionObserver;
  t.after(() => { globalThis.document = documentBefore; globalThis.IntersectionObserver = observerBefore; });
  globalThis.document = {
    createElement() {
      const element = {
        dataset: {}, children: [], handlers: {},
        appendChild(child) { this.children.push(child); },
        addEventListener(event, callback) { this.handlers[event] = callback; },
        querySelector() { return this.button ??= this; },
      };
      elements.push(element);
      return element;
    },
  };
  globalThis.IntersectionObserver = class {
    constructor(callback) { this.callback = callback; this.disconnected = false; observers.push(this); }
    observe(target) { this.target = target; }
    disconnect() { this.disconnected = true; }
  };
  let fetches = 0;
  t.mock.method(globalThis, "fetch", async () => { fetches++; throw Error("stale DOM must not fetch"); });
  const entry = { hash: hash(1), filename: "image.png", mime_type: "image/png", size_bytes: 1 };
  const image = m.createAttachmentElement(entry, dek, exportId);
  const pdf = m.createAttachmentElement({ ...entry, mime_type: "application/pdf" }, dek, exportId);
  const file = m.createAttachmentElement({ ...entry, mime_type: "text/plain" }, dek, exportId);
  m.reset();
  assert.equal(observers[0].disconnected, true);
  await observers[0].callback([{ isIntersecting: true, target: image }]);
  await image.children[0].handlers.click();
  await pdf.handlers.click();
  await file.handlers.click();
  assert.equal(fetches, 0);
});
