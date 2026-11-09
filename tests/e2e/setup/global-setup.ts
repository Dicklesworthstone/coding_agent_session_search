import { execFileSync, spawn } from "child_process";
import { randomUUID } from "crypto";
import { once } from "events";
import { createWriteStream, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "fs";
import path from "path";
import { finished } from "stream/promises";
import { fileURLToPath } from "url";
import { stripVTControlCharacters } from "util";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
let previewProcess: ReturnType<typeof spawn> | undefined;
let previewLogStream: ReturnType<typeof createWriteStream> | undefined;

/**
 * Global setup for HTML export E2E tests.
 * Generates test HTML exports from fixture JSONL files before tests run.
 */
async function globalSetup() {
  await cleanupPreview();
  try {
    await generateFixtures();
  } catch (error) {
    try {
      await cleanupPreview();
    } catch (cleanupError) {
      if (cleanupError !== error) {
        throw new AggregateError([error, cleanupError], "Preview setup and cleanup failed");
      }
    }
    throw error;
  }
}

async function generateFixtures() {
  const startedAt = new Date();
  const projectRoot = path.resolve(__dirname, "../../..");
  const exportDir = path.resolve(__dirname, "../exports");
  const pagesPreviewDir = path.resolve(__dirname, "../pages_preview");
  const fixturesDir = path.resolve(projectRoot, "tests/fixtures/html_export/real_sessions");

  // Ensure export directories exist
  if (!existsSync(exportDir)) {
    mkdirSync(exportDir, { recursive: true });
  }
  if (!existsSync(pagesPreviewDir)) {
    mkdirSync(pagesPreviewDir, { recursive: true });
  }

  // Each regeneration writes into a fresh run directory: `cass export-html`
  // never overwrites (it writes `<name>_1.html` beside an existing file), so a
  // reused directory left the tests reading the previous run's exports while
  // this binary's sat unused. latest-exports.json maps each export to the file
  // the newest run wrote, for the skip-regeneration path.
  const latestExportsFile = path.join(exportDir, "latest-exports.json");
  let latestExports: Record<string, string> = {};
  try {
    latestExports = JSON.parse(readFileSync(latestExportsFile, "utf8"));
  } catch {
    latestExports = {};
  }

  // Check if we can skip regeneration - if all exports exist and are recent
  const requiredExports = [
    "test-basic",
    "test-encrypted",
    "test-tool-calls",
    "test-large",
    "test-unicode",
    "test-no-cdn",
  ];
  const allExportsExist = requiredExports.every((name) => {
    const exportPath = latestExports[name];
    if (!exportPath || !existsSync(exportPath)) return false;
    // Check file size > 1KB to ensure it's not a placeholder
    try {
      const stats = statSync(exportPath);
      return stats.size > 1024;
    } catch {
      return false;
    }
  });

  const forceRegenerate = process.env.CI === "true" || process.env.E2E_SKIP_REGENERATE === "0";
  const skipExportRegenerate = allExportsExist && !forceRegenerate;
  if (skipExportRegenerate) {
    console.log(
      "All exports exist, skipping regeneration. Set E2E_SKIP_REGENERATE=0 to force regeneration.",
    );
  }

  // Find the cass binary - check CARGO_TARGET_DIR or common locations.
  // Cargo names Windows executables with `.exe`.
  const exe = process.platform === "win32" ? ".exe" : "";
  const cargoTargetDir = process.env.CARGO_TARGET_DIR
    ? path.resolve(process.env.CARGO_TARGET_DIR)
    : undefined;
  const possiblePaths = [
    cargoTargetDir ? path.join(cargoTargetDir, `release/cass${exe}`) : null,
    `/data/tmp/cargo-target/release/cass${exe}`,
    path.join(projectRoot, `target/release/cass${exe}`),
  ].filter(Boolean) as string[];

  let cassPath = "";
  for (const p of possiblePaths) {
    if (existsSync(p)) {
      cassPath = p;
      break;
    }
  }

  // The browser lane builds both binaries. Standalone setup uses the same
  // shared build helper when no existing cass executable is available.
  if (!cassPath) {
    console.log("Building cass CLI...");
    try {
      execFileSync(
        "bash",
        [
          "-c",
          'set -euo pipefail; source "$1"; run_cargo build --locked --release --bin cass --bin cass-pages-perf-bundle',
          "cass-e2e-build",
          path.join(projectRoot, "scripts/lib/run_cargo.sh"),
        ],
        {
          cwd: projectRoot,
          stdio: "inherit",
          timeout: 600000,
          env: {
            ...process.env,
            RCH_TARGET_DIR: cargoTargetDir ?? path.join(projectRoot, "target"),
          },
        },
      );
    } catch {
      console.warn("Cargo build failed or timed out, trying with existing binary...");
    }

    for (const p of possiblePaths) {
      if (existsSync(p)) {
        cassPath = p;
        break;
      }
    }
  }

  if (!cassPath) {
    throw new Error(`Could not find cass binary. Checked: ${possiblePaths.join(", ")}`);
  }

  console.log(`Using cass binary: ${cassPath}`);

  // Generate test exports
  const exports = [
    {
      name: "test-basic",
      fixture: "claude_code_auth_fix.jsonl",
      args: [],
    },
    {
      name: "test-encrypted",
      fixture: "claude_code_auth_fix.jsonl",
      args: ["--encrypt", "--password-stdin"],
      stdin: "test-password-123\n",
    },
    {
      name: "test-tool-calls",
      fixture: "cursor_refactoring.jsonl",
      args: [],
    },
    {
      name: "test-large",
      fixture: "../edge_cases/large_session.jsonl",
      args: [],
    },
    {
      name: "test-unicode",
      fixture: "../edge_cases/unicode_heavy.jsonl",
      args: [],
    },
    {
      name: "test-no-cdn",
      fixture: "claude_code_auth_fix.jsonl",
      args: ["--no-cdns"],
    },
  ];

  const exportResults: Array<{
    name: string;
    fixture: string;
    outputPath: string;
    args: string[];
    stdin?: boolean;
    command: string;
    success: boolean;
    durationMs: number;
    error?: string;
    stdout?: string;
    stderr?: string;
  }> = [];

  // Write environment file for tests
  const envContent: Record<string, string> = {
    TEST_EXPORTS_DIR: exportDir,
    TEST_EXPORT_PASSWORD: "test-password-123",
  };

  const runDir = path.join(exportDir, `run-${new Date().toISOString().replace(/[:.]/g, "-")}`);
  if (!skipExportRegenerate) {
    mkdirSync(runDir, { recursive: true });
  }
  const writtenExports: Record<string, string> = {};

  for (const { name, fixture, args, stdin } of exports) {
    const fixturePath = path.join(fixturesDir, fixture);
    const envKey = `TEST_EXPORT_${name.toUpperCase().replace(/-/g, "_")}`;

    if (skipExportRegenerate) {
      envContent[envKey] = latestExports[name];
      continue;
    }

    // Always set the env path so tests can fail loudly if exports are missing.
    let outputPath = path.join(runDir, `${name}.html`);
    envContent[envKey] = outputPath;

    console.log(`Generating ${name}.html from ${fixture}...`);

    const cmdArgs = [
      "export-html",
      fixturePath,
      "--output-dir",
      runDir,
      "--filename",
      `${name}.html`,
      "--json",
      ...args,
    ];
    const cmd = [cassPath, ...cmdArgs].join(" ");

    const started = Date.now();
    let success = true;
    let errorText = "";
    let stdout = "";
    let stderr = "";

    try {
      // Use the CLI to generate export
      const output = execFileSync(cassPath, cmdArgs, {
        cwd: projectRoot,
        input: stdin,
        stdio: "pipe",
      });
      stdout = output ? output.toString() : "";
      // Test the file cass reports writing, not the name requested.
      try {
        const reported = JSON.parse(stdout)?.exported?.output_path;
        if (typeof reported === "string" && reported) {
          outputPath = reported;
          envContent[envKey] = outputPath;
        }
      } catch {
        console.warn(`  export-html --json printed no JSON document for ${name}`);
      }
      console.log(`  -> ${outputPath}`);
    } catch (err) {
      success = false;
      const execErr = err as {
        message?: string;
        stdout?: Buffer | string;
        stderr?: Buffer | string;
      };
      stdout = execErr?.stdout ? execErr.stdout.toString() : "";
      stderr = execErr?.stderr ? execErr.stderr.toString() : "";
      errorText = execErr?.message ?? String(err);
      console.error(`Failed to generate ${name}:`, err);
      // Create a placeholder file so tests can check for its existence
      writeFileSync(outputPath, `<!-- Export generation failed for ${name} -->`);
    }

    const durationMs = Date.now() - started;
    exportResults.push({
      name,
      fixture,
      outputPath,
      args,
      stdin: stdin ? true : undefined,
      command: cmd,
      success,
      durationMs,
      error: errorText || undefined,
      stdout: stdout ? stdout.slice(-8000) : undefined,
      stderr: stderr ? stderr.slice(-8000) : undefined,
    });
    writtenExports[name] = outputPath;
  }
  if (!skipExportRegenerate) {
    writeFileSync(latestExportsFile, JSON.stringify(writtenExports, null, 2) + "\n");
  }

  // -----------------------------------------------------------------------------
  // Pages preview server (for OPFS / Service Worker tests)
  // -----------------------------------------------------------------------------
  const previewPort = parseInt(process.env.TEST_PAGES_PREVIEW_PORT || "8090", 10);
  const previewPassword = process.env.TEST_PAGES_PREVIEW_PASSWORD || "test-password-123";
  const pagesBundleDir = path.join(pagesPreviewDir, "bundle");

  const possibleBundlePaths = [
    path.join(path.dirname(cassPath), `cass-pages-perf-bundle${exe}`),
    cargoTargetDir
      ? path.join(cargoTargetDir, `release/cass-pages-perf-bundle${exe}`)
      : null,
    path.join(projectRoot, `target/release/cass-pages-perf-bundle${exe}`),
  ].filter(Boolean) as string[];

  let bundleBinPath = "";
  for (const p of possibleBundlePaths) {
    if (existsSync(p)) {
      bundleBinPath = p;
      break;
    }
  }

  if (!bundleBinPath) {
    throw new Error(
      `Could not find cass-pages-perf-bundle binary. Checked: ${possibleBundlePaths.join(", ")}`,
    );
  } else {
    console.log(`Using perf bundle binary: ${bundleBinPath}`);
    try {
      execFileSync(
        bundleBinPath,
        [
          "--output",
          pagesPreviewDir,
          "--preset",
          "small",
          "--password",
          previewPassword,
        ],
        { cwd: projectRoot, stdio: "pipe" },
      );
      console.log(`Pages bundle ready: ${pagesBundleDir}`);
    } catch (err) {
      throw new Error("Failed to generate a fresh pages preview bundle", { cause: err });
    }
  }

  // Key slots for the unlock specs (bead browser-e2e-lane-bnueb): a recovery
  // slot, and a second password that is then revoked, so the viewer must
  // refuse it. A value is exported only once its slot operation succeeded;
  // otherwise the reason goes to TEST_PAGES_KEY_SLOT_ERROR and the specs fail.
  let recoveryKey = "";
  let revokedPassword = "";
  let keySlotError = "";
  if (bundleBinPath && existsSync(pagesBundleDir)) {
    const keyVerb = (verb: string, stdin: string, extra: string[] = []) => {
      const stdout = execFileSync(
        cassPath,
        ["pages", "key", verb, "--archive", pagesBundleDir, "--password-stdin", "--json", ...extra],
        { cwd: projectRoot, input: stdin, stdio: "pipe" },
      ).toString();
      try {
        return JSON.parse(stdout);
      } catch {
        throw new Error(`cass pages key ${verb} printed no JSON document: ${stdout.slice(0, 200)}`);
      }
    };
    try {
      recoveryKey = keyVerb("add-recovery", `${previewPassword}\n`).recovery_secret ?? "";
      // A fresh value per run: the bundle is regenerated, and no fixed
      // credential lands in the tree.
      const secondPassword = `revoked-${randomUUID()}`;
      const added = keyVerb("add-password", `${previewPassword}\n${secondPassword}\n`);
      // The refusal spec proves nothing unless the second password opened
      // its slot before the revoke. revoke refuses to remove the slot that
      // authenticated the call, so revoking it with its own password must fail.
      let selfRevoke = "";
      try {
        keyVerb("revoke", `${secondPassword}\n`, ["--slot", String(added.slot_id)]);
      } catch (err) {
        selfRevoke = String((err as { stderr?: Buffer }).stderr ?? err);
      }
      if (!selfRevoke.includes("used for authentication")) {
        throw new Error(
          `the added password did not authenticate slot ${added.slot_id}: ${selfRevoke || "its own revoke succeeded"}`,
        );
      }
      keyVerb("revoke", `${previewPassword}\n`, ["--slot", String(added.slot_id)]);
      revokedPassword = secondPassword;
    } catch (err) {
      keySlotError = String((err as { stderr?: Buffer }).stderr ?? err);
      console.warn("Failed to prepare pages key slots:", err);
    }
  }
  envContent.TEST_PAGES_PREVIEW_RECOVERY_KEY = recoveryKey;
  envContent.TEST_PAGES_PREVIEW_REVOKED_PASSWORD = revokedPassword;
  envContent.TEST_PAGES_KEY_SLOT_ERROR = keySlotError.replace(/\s+/g, " ").trim();

  let previewUrl = "";
  let previewPid = "";
  const previewLog = path.join(pagesPreviewDir, "preview-server.log");

  if (bundleBinPath && existsSync(pagesBundleDir)) {
    const previewArgs = [
      "pages",
      "--preview",
      pagesBundleDir,
      "--port",
      String(previewPort),
      "--no-open",
    ];

    console.log(`Starting preview server on port ${previewPort}...`);
    const logStream = createWriteStream(previewLog, { flags: "a" });
    previewLogStream = logStream;
    // An unusable destination must reject before there is a child to orphan.
    await once(logStream, "open");
    const previewProc = spawn(cassPath, previewArgs, {
      cwd: projectRoot,
      stdio: ["ignore", "pipe", "pipe"],
    });
    previewProcess = previewProc;
    const logFailure = new Promise<never>((_, reject) => logStream.once("error", reject));

    if (previewProc.stdout && previewProc.stderr) {
      previewProc.stdout.pipe(logStream, { end: false });
      previewProc.stderr.pipe(logStream, { end: false });
      previewProc.once("close", () => logStream.end());
    }

    previewPid = String(previewProc.pid ?? "");

    const ready = await Promise.race([
      waitForUrl(`http://127.0.0.1:${previewPort}/index.html`, 8000, previewProc),
      logFailure,
    ]);
    if (ready) {
      // Drain writes queued by the startup acknowledgement before declaring
      // readiness: the file may open successfully but fail its first write.
      await new Promise<void>((resolve, reject) => {
        logStream.write("", error => error ? reject(error) : resolve());
      });
      previewUrl = `http://127.0.0.1:${previewPort}/index.html`;
      console.log(`Preview server ready at ${previewUrl}`);
    } else {
      throw new Error(`The preview child did not become ready on port ${previewPort}; see ${previewLog}`);
    }
  }

  envContent.TEST_PAGES_PREVIEW_URL = previewUrl;
  envContent.TEST_PAGES_PREVIEW_PORT = String(previewPort);
  envContent.TEST_PAGES_PREVIEW_SITE_DIR = pagesBundleDir;
  envContent.TEST_PAGES_PREVIEW_PID = previewPid;
  envContent.TEST_PAGES_PREVIEW_PASSWORD = previewPassword;
  envContent.TEST_PAGES_PREVIEW_LOG = previewLog;

  const finishedAt = new Date();
  const setupMetadata = {
    startedAt: startedAt.toISOString(),
    finishedAt: finishedAt.toISOString(),
    durationMs: finishedAt.getTime() - startedAt.getTime(),
    node: process.version,
    platform: process.platform,
    arch: process.arch,
    projectRoot,
    exportDir,
    fixturesDir,
    cassPath,
    exports: exportResults,
    pagesPreview: {
      port: previewPort,
      siteDir: pagesBundleDir,
      url: previewUrl,
      pid: previewPid,
      log: previewLog,
    },
  };

  const metadataPath = path.join(exportDir, "setup-metadata.json");
  writeFileSync(metadataPath, JSON.stringify(setupMetadata, null, 2));
  envContent.TEST_EXPORT_SETUP_LOG = metadataPath;

  // Write environment file
  const envPath = path.join(__dirname, "../.env.test");
  writeFileSync(
    envPath,
    Object.entries(envContent)
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
  );

  console.log("\nE2E test setup complete!");
  console.log(`Exports directory: ${exportDir}`);
  console.log(`Environment file: ${envPath}`);
}

export default globalSetup;

// Keep the ChildProcess handle: a PID persisted by a previous run may now
// identify an unrelated process. Playwright calls this through global teardown.
export async function cleanupPreview(): Promise<void> {
  const child = previewProcess;
  const logStream = previewLogStream;
  previewProcess = undefined;
  previewLogStream = undefined;
  try {
    if (child?.pid && child.exitCode === null && child.signalCode === null) {
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => child.kill("SIGKILL"), 2000);
        const onClose = () => {
          clearTimeout(timer);
          child.off("error", onError);
          resolve();
        };
        const onError = (error: Error) => {
          clearTimeout(timer);
          child.off("close", onClose);
          reject(error);
        };
        child.once("close", onClose);
        child.once("error", onError);
        child.kill("SIGTERM");
      });
    }
  } finally {
    if (logStream) {
      if (!logStream.writableEnded) logStream.end();
      await finished(logStream, { cleanup: true });
    }
  }
}

async function waitForUrl(
  url: string,
  timeoutMs: number,
  previewProc?: ReturnType<typeof spawn>,
): Promise<boolean> {
  const start = Date.now();
  const controller = new AbortController();
  let bound = !previewProc;
  let pendingLine = "";
  let wake: () => void = () => {};
  const startup = new Promise<void>(resolve => { wake = resolve; });
  const onStopped = () => controller.abort();
  const target = new URL(url);
  const port = target.port || (target.protocol === "https:" ? "443" : "80");
  const expected = `Preview server running at http://localhost:${port}`;
  const onOutput = (chunk: Buffer) => {
    const lines = (pendingLine + chunk.toString()).split("\n");
    pendingLine = (lines.pop() ?? "").slice(-65536);
    if (lines.some(line => stripVTControlCharacters(line).trimEnd().endsWith(expected))) {
      bound = true;
      wake();
    }
  };
  controller.signal.addEventListener("abort", wake, { once: true });
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    if (previewProc) {
      previewProc.once("error", onStopped);
      previewProc.once("exit", onStopped);
      previewProc.stderr?.on("data", onOutput);
      if (!previewProc.pid || previewProc.exitCode !== null || previewProc.signalCode !== null) {
        controller.abort();
      }
      if (!controller.signal.aborted) {
        await startup;
      }
    }
    while (bound && !controller.signal.aborted && Date.now() - start < timeoutMs) {
      try {
        const res = await fetch(url, { method: "GET", signal: controller.signal });
        if (res.ok && !controller.signal.aborted) {
          return true;
        }
      } catch {
        if (controller.signal.aborted) {
          return false;
        }
      }
      const remaining = timeoutMs - (Date.now() - start);
      if (remaining > 0) {
        await new Promise((resolve) => setTimeout(resolve, Math.min(200, remaining)));
      }
    }
    return false;
  } finally {
    clearTimeout(timer);
    controller.abort();
    controller.signal.removeEventListener("abort", wake);
    previewProc?.off("error", onStopped);
    previewProc?.off("exit", onStopped);
    previewProc?.stderr?.off("data", onOutput);
  }
}
