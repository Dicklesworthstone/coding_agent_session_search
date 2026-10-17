/** Run: node --experimental-vm-modules --test tests/pages/clipboard-lifecycle.test.mjs
 * Executes shipped share.js with controlled browser clipboard/DOM boundaries.
 * This exercises asynchronous cancellation, not browser permission UI or OS clipboard behavior.
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
