import { execFileSync, execSync, spawn } from "child_process";
import { randomUUID } from "crypto";
import { createWriteStream, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * Global setup for HTML export E2E tests.
 * Generates test HTML exports from fixture JSONL files before tests run.
 */
async function globalSetup() {
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
  const possiblePaths = [
    process.env.CARGO_TARGET_DIR
      ? path.join(process.env.CARGO_TARGET_DIR, `release/cass${exe}`)
      : null,
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

  // Browser CI downloads the release binary from the setup job. Rebuilding it
  // inside every Playwright shard wastes most of the job timeout.
  if (!cassPath) {
    console.log("Building cass CLI...");
    try {
      execSync("cargo build --release", { cwd: projectRoot, stdio: "inherit", timeout: 600000 });
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
    process.env.CARGO_TARGET_DIR
      ? path.join(process.env.CARGO_TARGET_DIR, `release/cass-pages-perf-bundle${exe}`)
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
    console.warn(
      `Could not find cass-pages-perf-bundle binary. Checked: ${possibleBundlePaths.join(", ")}`,
    );
  } else {
    console.log(`Using perf bundle binary: ${bundleBinPath}`);
    try {
      execSync(
        [
          bundleBinPath,
          "--output",
          pagesPreviewDir,
          "--preset",
          "small",
          "--password",
          previewPassword,
        ].join(" "),
        { cwd: projectRoot, stdio: "pipe" },
      );
      console.log(`Pages bundle ready: ${pagesBundleDir}`);
    } catch (err) {
      console.warn("Failed to generate pages preview bundle:", err);
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
    const previewProc = spawn(cassPath, previewArgs, {
      cwd: projectRoot,
      stdio: ["ignore", "pipe", "pipe"],
    });

    if (previewProc.stdout && previewProc.stderr) {
      const logStream = createWriteStream(previewLog, { flags: "a" });
      previewProc.stdout.pipe(logStream);
      previewProc.stderr.pipe(logStream);
    }

    previewPid = String(previewProc.pid ?? "");

    const ready = await waitForUrl(`http://127.0.0.1:${previewPort}/index.html`, 8000);
    if (ready) {
      previewUrl = `http://127.0.0.1:${previewPort}/index.html`;
      console.log(`Preview server ready at ${previewUrl}`);
    } else {
      console.warn("Preview server failed to respond in time. Tests will skip preview checks.");
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

async function waitForUrl(url: string, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await fetch(url, { method: "GET" });
      if (res.ok) {
        return true;
      }
    } catch {
      // ignore
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  return false;
}
