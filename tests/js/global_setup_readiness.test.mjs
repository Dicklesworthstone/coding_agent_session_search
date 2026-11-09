// Run with Node 22.6+: node --experimental-strip-types --test tests/js/global_setup_readiness.test.mjs
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

// Expose the existing private function in a temporary module. Its body stays
// unchanged, and importing it does not invoke setup or launch any browsers.
const source = readFileSync(new URL("../e2e/setup/global-setup.ts", import.meta.url), "utf8");
const modulePath = path.join(mkdtempSync(path.join(tmpdir(), "cass-readiness-")), "global-setup.ts");
writeFileSync(modulePath, `${source}\nexport { waitForUrl };\nexport function trackPreviewForTest(child) { previewProcess = child; }\n`);
const { waitForUrl, cleanupPreview, trackPreviewForTest } = await import(pathToFileURL(modulePath).href);

async function listen(server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return `http://127.0.0.1:${server.address().port}/index.html`;
}

async function close(server) {
  const closed = new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
  server.closeAllConnections();
  await closed;
}

async function within(promise, timeoutMs) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("readiness probe exceeded its deadline")), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

test("preview readiness accepts a real successful HTTP response", async () => {
  const server = createServer((_, response) => response.end("ready"));
  const url = await listen(server);
  try {
    assert.equal(await within(waitForUrl(url, 1000), 2000), true);
  } finally {
    await close(server);
  }
});

test("preview readiness bounds a server that accepts but never responds", async () => {
  const server = createServer(() => {});
  const url = await listen(server);
  const pending = waitForUrl(url, 75);
  try {
    assert.equal(await within(pending, 400), false);
  } finally {
    await close(server);
    await pending.catch(() => {});
  }
});

test("preview readiness bounds connection refusal", async () => {
  const server = createServer((_, response) => response.end("ready"));
  const url = await listen(server);
  await close(server);
  assert.equal(await within(waitForUrl(url, 75), 400), false);
});

test("a zero readiness budget makes no HTTP request", async () => {
  let requests = 0;
  const server = createServer((_, response) => {
    requests += 1;
    response.end("ready");
  });
  const url = await listen(server);
  try {
    assert.equal(await waitForUrl(url, 0), false);
    assert.equal(requests, 0);
  } finally {
    await close(server);
  }
});

// A real child owns the listener only after listen succeeds. The startup line
// matches cass's bind-success message; no fetch or process API is replaced.
function previewChild(url, mode = "respond") {
  const child = spawn(process.execPath, ["-e", `
    const { createServer } = require("node:http");
    const port = Number(process.argv[1]);
    if (process.argv[2] === "ignore-term") process.on("SIGTERM", () => {});
    const server = createServer((_, response) => {
      console.error("request-received");
      if (process.argv[2] === "exit-request") process.exit(2);
      if (process.argv[2] !== "hang") response.end("owned preview");
    });
    server.on("error", error => { console.error(error.code); process.exitCode = 1; });
    server.listen(port, "127.0.0.1", () => {
      console.error("\\x1b[1;32m🌐\\x1b[0m Preview server running at \\x1b[1;36mhttp://localhost:" + port + "\\x1b[0m");
    });
  `, new URL(url).port, mode], { stdio: ["ignore", "ignore", "pipe"] });
  child.on("error", () => {});
  return child;
}

async function stopChild(child) {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  const closed = new Promise(resolve => child.once("close", resolve));
  child.kill("SIGTERM");
  await within(closed, 2000);
}

async function unusedUrl() {
  const server = createServer();
  const url = await listen(server);
  await close(server);
  return url;
}

test("preview readiness accepts the child that actually bound its port", async () => {
  const url = await unusedUrl();
  const child = previewChild(url);
  try {
    assert.equal(await within(waitForUrl(url, 1000, child), 2000), true);
  } finally {
    await stopChild(child);
  }
});

test("an unrelated HTTP 200 cannot hide the preview child's bind failure", async () => {
  const foreign = createServer((_, response) => response.end("unrelated server"));
  const url = await listen(foreign);
  const child = previewChild(url);
  try {
    assert.equal(await within(waitForUrl(url, 1000, child), 2000), false);
    assert.equal(await (await fetch(url, { signal: AbortSignal.timeout(1000) })).text(), "unrelated server");
  } finally {
    await stopChild(child);
    await close(foreign);
  }
});

test("an unrelated HTTP 200 cannot hide a preview spawn error", async () => {
  const foreign = createServer((_, response) => response.end("unrelated server"));
  const url = await listen(foreign);
  const child = spawn(`${process.execPath}-missing-cass-preview`, [], { stdio: "ignore" });
  child.on("error", () => {});
  try {
    assert.equal(await within(waitForUrl(url, 1000, child), 2000), false);
  } finally {
    await stopChild(child);
    await close(foreign);
  }
});

test("an owned preview that never responds still shares the readiness deadline", async () => {
  const url = await unusedUrl();
  const child = previewChild(url, "hang");
  let output = "";
  child.stderr.on("data", chunk => { output += chunk; });
  try {
    assert.equal(await within(waitForUrl(url, 1000, child), 2000), false);
    assert.match(output, /request-received/);
  } finally {
    await stopChild(child);
  }
});

test("cleanup waits for the owned preview to close and is repeatable", async () => {
  const url = await unusedUrl();
  const child = previewChild(url);
  try {
    assert.equal(await waitForUrl(url, 1000, child), true);
    trackPreviewForTest(child);
    await within(cleanupPreview(), 3000);
    assert.notEqual(child.signalCode ?? child.exitCode, null);
    await cleanupPreview();
  } finally {
    await stopChild(child);
  }
});

test("cleanup of an exited preview preserves another live child", async () => {
  const oldChild = spawn(process.execPath, ["-e", "process.exit(0)"], { stdio: "ignore" });
  await once(oldChild, "close");
  const url = await unusedUrl();
  const unrelated = previewChild(url);
  try {
    assert.equal(await waitForUrl(url, 1000, unrelated), true);
    trackPreviewForTest(oldChild);
    await cleanupPreview();
    assert.equal(await (await fetch(url, { signal: AbortSignal.timeout(1000) })).text(), "owned preview");
  } finally {
    await stopChild(unrelated);
  }
});

test("cleanup closes an owned child that ignores graceful termination", async () => {
  const url = await unusedUrl();
  const child = previewChild(url, "ignore-term");
  try {
    assert.equal(await waitForUrl(url, 1000, child), true);
    trackPreviewForTest(child);
    await within(cleanupPreview(), 4000);
    assert.notEqual(child.signalCode ?? child.exitCode, null);
    if (process.platform !== "win32") assert.equal(child.signalCode, "SIGKILL");
  } finally {
    await stopChild(child);
  }
});

test("preview exit aborts an HTTP request already in progress", async () => {
  const url = await unusedUrl();
  const child = previewChild(url, "exit-request");
  try {
    assert.equal(await within(waitForUrl(url, 1000, child), 2000), false);
    assert.equal(child.exitCode, 2);
  } finally {
    await stopChild(child);
  }
});
