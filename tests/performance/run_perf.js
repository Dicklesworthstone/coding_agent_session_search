const fs = require("fs");
const path = require("path");
const http = require("http");
const { chromium } = require("playwright");
const { runDecryptTiming } = require("./decrypt-timing.test");
const { runSearchLatency } = require("./search-latency.test");
const { runMemoryProfile } = require("./memory-profiler.test");
const { runScrollPerformance } = require("./scroll-performance.test");
const { assertAll, TARGETS } = require("./assertions");

const DEFAULT_QUERIES = [
  "authentication",
  "error handling",
  "async await promise",
  "react useState hook",
  "fix bug",
  "AuthController.ts",
  "sha256",
  "xyzzy123nonexistent",
];

function parseArgs() {
  const args = process.argv.slice(2);
  const out = {
    bundle: null,
    password: "test-password",
    out: null,
    lighthouse: false,
  };

  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--bundle") {
      out.bundle = args[i + 1];
      i += 1;
    } else if (arg === "--password") {
      out.password = args[i + 1];
      i += 1;
    } else if (arg === "--out") {
      out.out = args[i + 1];
      i += 1;
    } else if (arg === "--lighthouse") {
      out.lighthouse = true;
    }
  }

  if (!out.bundle) {
    throw new Error("Missing --bundle <path>");
  }

  return out;
}

function contentType(filePath) {
  const ext = path.extname(filePath).toLowerCase();
  switch (ext) {
    case ".html":
      return "text/html";
    case ".js":
      return "text/javascript";
    case ".css":
      return "text/css";
    case ".json":
      return "application/json";
    case ".wasm":
      return "application/wasm";
    case ".bin":
      return "application/octet-stream";
    case ".svg":
      return "image/svg+xml";
    default:
      return "application/octet-stream";
  }
}

function startServer(rootDir) {
  const server = http.createServer((req, res) => {
    const urlPath = decodeURIComponent(req.url || "/");
    const safePath = urlPath.split("?")[0].replace(/\.{2,}/g, ".");
    const resolved = safePath === "/" ? "/index.html" : safePath;
    const filePath = path.join(rootDir, resolved);

    fs.readFile(filePath, (err, data) => {
      if (err) {
        res.writeHead(404, { "Content-Type": "text/plain" });
        res.end("Not found");
        return;
      }
      res.writeHead(200, { "Content-Type": contentType(filePath) });
      res.end(data);
    });
  });

  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      resolve({ server, port });
    });
  });
}

async function runLighthouse(url) {
  try {
    const lighthouse = require("lighthouse");
    const chromeLauncher = require("chrome-launcher");
    const config = require("./lighthouse.config");

    const chrome = await chromeLauncher.launch({ chromeFlags: ["--headless"] });
    const options = {
      logLevel: "info",
      output: "json",
      port: chrome.port,
    };

    const result = await lighthouse(url, options, config);
    await chrome.kill();

    const lhr = result.lhr;
    return {
      performanceScore: lhr?.categories?.performance?.score
        ? lhr.categories.performance.score * 100
        : 0,
      fcp: lhr?.audits?.["first-contentful-paint"]?.numericValue || 0,
      lcp: lhr?.audits?.["largest-contentful-paint"]?.numericValue || 0,
      tti: lhr?.audits?.["interactive"]?.numericValue || 0,
      tbt: lhr?.audits?.["total-blocking-time"]?.numericValue || 0,
    };
  } catch (error) {
    return { error: error.message || String(error) };
  }
}

async function main() {
  const args = parseArgs();
  const bundleDir = path.resolve(args.bundle);

  if (!fs.existsSync(bundleDir)) {
    throw new Error(`Bundle directory not found: ${bundleDir}`);
  }

  const { server, port } = await startServer(bundleDir);
  const baseUrl = `http://127.0.0.1:${port}/index.html`;

  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();

  const navigationStart = Date.now();
  await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#password", { timeout: 15_000 });

  const navMetrics = await page.evaluate(() => {
    const nav = performance.getEntriesByType("navigation")[0];
    const fcpEntry = performance.getEntriesByName("first-contentful-paint")[0];
    return {
      domContentLoaded: nav ? nav.domContentLoadedEventEnd : null,
      loadEvent: nav ? nav.loadEventEnd : null,
      fcp: fcpEntry ? fcpEntry.startTime : null,
    };
  });

  const decryptMetrics = await runDecryptTiming(page, args.password);
  await page.waitForSelector("#search-input", { timeout: 30_000 });

  const searchMetrics = await runSearchLatency(page, DEFAULT_QUERIES);
  const memoryMetrics = await runMemoryProfile(page, 100);

  // Run scroll performance test
  let scrollMetrics = null;
  try {
    scrollMetrics = await runScrollPerformance(page, {
      scrollContainer: "#results-container",
      scrollSteps: 100,
      stepDelay: 16,
    });
  } catch (e) {
    scrollMetrics = { error: e.message || String(e) };
  }

  let lighthouseMetrics = null;
  if (args.lighthouse) {
    lighthouseMetrics = await runLighthouse(baseUrl);
  }

  await browser.close();
  server.close();

  // Compile all metrics
  const rawMetrics = {
    navigation: navMetrics,
    decrypt: decryptMetrics,
    search: searchMetrics,
    memory: memoryMetrics,
    scroll: scrollMetrics,
    lighthouse: lighthouseMetrics,
  };

  // Run assertions
  const assertions = assertAll(rawMetrics);

  const summary = {
    bundle: bundleDir,
    baseUrl,
    elapsed_ms: Date.now() - navigationStart,
    targets: TARGETS,
    ...rawMetrics,
    assertions: {
      pass: assertions.pass,
      summary: assertions.summary,
      failures: assertions.failures,
    },
  };

  const payload = JSON.stringify(summary, null, 2);
  if (args.out) {
    fs.writeFileSync(args.out, payload);
  } else {
    console.log(payload);
  }

  // Print assertion summary to stderr
  if (!assertions.pass) {
    console.error("\n[perf] ASSERTIONS FAILED:");
    for (const failure of assertions.failures) {
      console.error(`  - ${failure}`);
    }
    return 1;
  }

  console.error(`\n[perf] All ${assertions.summary.passed} assertions passed`);
  return 0;
}

main()
  .then((exitCode) => {
    process.exit(exitCode || 0);
  })
  .catch((err) => {
    console.error("[perf] failed:", err);
    process.exit(1);
  });
