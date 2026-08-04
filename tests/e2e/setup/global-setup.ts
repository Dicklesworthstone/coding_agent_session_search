import { execSync, spawn, spawnSync } from 'child_process';
import { createHash } from 'crypto';
import {
  createWriteStream,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

type SanitizedPathReference = {
  scope: 'workspace' | 'external';
  segments: number;
  sha256: string;
  redacted: true;
};

type DiagnosticArtifact = {
  artifact: string;
  bytes: number;
  sha256: string;
  redacted: true;
};

function sha256(value: string | Buffer): string {
  return createHash('sha256').update(value).digest('hex');
}

function sanitizePathReference(filePath: string, projectRoot: string): SanitizedPathReference {
  const resolved = path.resolve(filePath);
  const relative = path.relative(projectRoot, resolved);
  const inWorkspace =
    relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..');
  return {
    scope: inWorkspace ? 'workspace' : 'external',
    segments: resolved.split(path.sep).filter(Boolean).length,
    sha256: sha256(resolved),
    redacted: true,
  };
}

function writeDiagnosticArtifact(
  exportDir: string,
  exportName: string,
  stream: 'stdout' | 'stderr',
  raw: string
): DiagnosticArtifact {
  const summary = {
    stream,
    bytes: Buffer.byteLength(raw),
    sha256: sha256(raw),
    redacted: true as const,
    rawStored: false,
  };
  const artifact = `${exportName}-${stream}-summary.json`;
  writeFileSync(path.join(exportDir, artifact), JSON.stringify(summary, null, 2));
  return {
    artifact,
    bytes: summary.bytes,
    sha256: summary.sha256,
    redacted: true,
  };
}

function encryptedMetadataSummary(html: string): {
  encryptedContentPresent: boolean;
  metadataValid: boolean;
  iterations?: number;
  saltBytes?: number;
  ivBytes?: number;
  ciphertextBytes?: number;
} {
  const match = html.match(/<div id="encrypted-content" hidden>([\s\S]*?)<\/div>/);
  if (!match) {
    return { encryptedContentPresent: false, metadataValid: false };
  }
  try {
    const metadata = JSON.parse(match[1]) as {
      iterations?: unknown;
      salt?: unknown;
      iv?: unknown;
      ciphertext?: unknown;
    };
    const iterations =
      typeof metadata.iterations === 'number' && Number.isInteger(metadata.iterations)
        ? metadata.iterations
        : undefined;
    const saltBytes =
      typeof metadata.salt === 'string'
        ? Buffer.from(metadata.salt, 'base64').byteLength
        : undefined;
    const ivBytes =
      typeof metadata.iv === 'string'
        ? Buffer.from(metadata.iv, 'base64').byteLength
        : undefined;
    const ciphertextBytes =
      typeof metadata.ciphertext === 'string'
        ? Buffer.from(metadata.ciphertext, 'base64').byteLength
        : undefined;
    return {
      encryptedContentPresent: true,
      metadataValid:
        iterations !== undefined &&
        saltBytes !== undefined &&
        ivBytes !== undefined &&
        ciphertextBytes !== undefined,
      iterations,
      saltBytes,
      ivBytes,
      ciphertextBytes,
    };
  } catch {
    return { encryptedContentPresent: true, metadataValid: false };
  }
}

/**
 * Global setup for HTML export E2E tests.
 * Generates test HTML exports from fixture JSONL files before tests run.
 */
async function globalSetup() {
  const startedAt = new Date();
  const projectRoot = path.resolve(__dirname, '../../..');
  const exportDir = path.resolve(__dirname, '../exports');
  const pagesPreviewDir = path.resolve(__dirname, '../pages_preview');
  const fixturesDir = path.resolve(projectRoot, 'tests/fixtures/html_export/real_sessions');

  // Ensure export directories exist
  if (!existsSync(exportDir)) {
    mkdirSync(exportDir, { recursive: true });
  }
  if (!existsSync(pagesPreviewDir)) {
    mkdirSync(pagesPreviewDir, { recursive: true });
  }

  // Check if we can skip regeneration - if all exports exist and are recent
  const requiredExports = ['test-basic.html', 'test-encrypted.html', 'test-tool-calls.html',
                           'test-large.html', 'test-unicode.html', 'test-no-cdn.html'];
  const allExportsExist = requiredExports.every(name => {
    const exportPath = path.join(exportDir, name);
    if (!existsSync(exportPath)) return false;
    // Check file size > 1KB to ensure it's not a placeholder
    try {
      const stats = statSync(exportPath);
      return stats.size > 1024;
    } catch {
      return false;
    }
  });

  const forceRegenerate =
    process.env.CI === 'true' || process.env.E2E_SKIP_REGENERATE === '0';
  const skipExportRegenerate = allExportsExist && !forceRegenerate;
  if (skipExportRegenerate) {
    console.log('All exports exist, skipping regeneration. Set E2E_SKIP_REGENERATE=0 to force regeneration.');
  }

  // Find the cass binary - check CARGO_TARGET_DIR or common locations
  const possiblePaths = [
    process.env.CARGO_TARGET_DIR ? path.join(process.env.CARGO_TARGET_DIR, 'release/cass') : null,
    '/data/tmp/cargo-target/release/cass',
    path.join(projectRoot, 'target/release/cass'),
  ].filter(Boolean) as string[];

  let cassPath = '';
  for (const p of possiblePaths) {
    if (existsSync(p)) {
      cassPath = p;
      break;
    }
  }

  // Browser CI downloads the release binary from the setup job. Rebuilding it
  // inside every Playwright shard wastes most of the job timeout.
  if (!cassPath) {
    console.log('Building cass CLI...');
    try {
      execSync('cargo build --release', { cwd: projectRoot, stdio: 'inherit', timeout: 600000 });
    } catch {
      console.warn('Cargo build failed or timed out, trying with existing binary...');
    }

    for (const p of possiblePaths) {
      if (existsSync(p)) {
        cassPath = p;
        break;
      }
    }
  }

  if (!cassPath) {
    throw new Error(`Could not find cass binary. Checked: ${possiblePaths.join(', ')}`);
  }

  console.log(`Using cass binary: ${cassPath}`);

  // Generate test exports
  const exports = [
    {
      name: 'test-basic',
      fixture: 'claude_code_auth_fix.jsonl',
      args: [],
    },
    {
      name: 'test-encrypted',
      fixture: 'claude_code_auth_fix.jsonl',
      args: ['--encrypt', '--password-stdin'],
      stdin: 'test-password-123\n',
    },
    {
      name: 'test-tool-calls',
      fixture: 'cursor_refactoring.jsonl',
      args: [],
    },
    {
      name: 'test-large',
      fixture: '../edge_cases/large_session.jsonl',
      args: [],
    },
    {
      name: 'test-unicode',
      fixture: '../edge_cases/unicode_heavy.jsonl',
      args: [],
    },
    {
      name: 'test-no-cdn',
      fixture: 'claude_code_auth_fix.jsonl',
      args: ['--no-cdns'],
    },
  ];

  const exportResults: Array<{
    name: string;
    fixtureName: string;
    output: {
      artifact: string;
      sizeBytes: number;
      sha256: string;
    };
    flags: string[];
    command: {
      program: 'cass';
      argumentCount: number;
      passwordViaStdin: boolean;
      redacted: true;
    };
    success: boolean;
    reused: boolean;
    durationMs: number;
    error?: {
      bytes: number;
      sha256: string;
      redacted: true;
    };
    diagnostics: {
      stdout: DiagnosticArtifact;
      stderr: DiagnosticArtifact;
    };
    encryption: ReturnType<typeof encryptedMetadataSummary>;
    redaction: {
      passwordChecked: boolean;
      passwordFoundInHtml: boolean;
      passwordFoundInStdout: boolean;
      passwordFoundInStderr: boolean;
      rawDiagnosticsStored: false;
    };
  }> = [];
  let redactionViolation = false;
  let setupProofViolation = false;

  // Write environment file for tests
  const envContent: Record<string, string> = {
    TEST_EXPORTS_DIR: exportDir,
    TEST_EXPORT_PASSWORD: 'test-password-123',
  };

  for (const { name, fixture, args, stdin } of exports) {
    const fixturePath = path.join(fixturesDir, fixture);
    const outputPath = path.join(exportDir, `${name}.html`);
    const envKey = `TEST_EXPORT_${name.toUpperCase().replace(/-/g, '_')}`;

    // Always set the env path so tests can fail loudly if exports are missing.
    envContent[envKey] = outputPath;

    const cmdArgs = [
      'export-html',
      fixturePath,
      '--output-dir', path.dirname(outputPath),
      '--filename', path.basename(outputPath),
      ...args,
    ];

    const started = Date.now();
    let success = true;
    let errorText = '';
    let stdout = '';
    let stderr = '';

    if (!skipExportRegenerate) {
      console.log(`Generating ${name}.html from ${fixture}...`);
      const output = spawnSync(cassPath, cmdArgs, {
        cwd: projectRoot,
        input: stdin,
        encoding: 'utf-8',
        timeout: 600_000,
      });
      stdout = output.stdout ?? '';
      stderr = output.stderr ?? '';
      if (!output.error && output.status === 0) {
        console.log(`  -> ${outputPath}`);
      } else {
        success = false;
        errorText =
          output.error?.message ??
          `cass export exited with status ${output.status ?? 'unknown'}`;
        console.error(`Failed to generate ${name}; see sanitized setup diagnostics.`);
        // Create a placeholder file so tests can check for its existence
        writeFileSync(outputPath, `<!-- Export generation failed for ${name} -->`);
      }
    }

    const durationMs = Date.now() - started;
    const html = existsSync(outputPath) ? readFileSync(outputPath, 'utf-8') : '';
    const password = stdin?.trim() ?? '';
    const passwordFoundInHtml = password.length > 0 && html.includes(password);
    const passwordFoundInStdout = password.length > 0 && stdout.includes(password);
    const passwordFoundInStderr = password.length > 0 && stderr.includes(password);
    if (passwordFoundInHtml || passwordFoundInStdout || passwordFoundInStderr) {
      success = false;
      redactionViolation = true;
    }
    const encryption = encryptedMetadataSummary(html);
    const expectsEncryption = args.includes('--encrypt');
    if (
      (expectsEncryption && (!encryption.encryptedContentPresent || !encryption.metadataValid)) ||
      (!expectsEncryption && encryption.encryptedContentPresent) ||
      Buffer.byteLength(html) <= 1024
    ) {
      success = false;
      setupProofViolation = true;
    }
    const stdoutArtifact = writeDiagnosticArtifact(exportDir, name, 'stdout', stdout);
    const stderrArtifact = writeDiagnosticArtifact(exportDir, name, 'stderr', stderr);
    exportResults.push({
      name,
      fixtureName: path.basename(fixture),
      output: {
        artifact: path.basename(outputPath),
        sizeBytes: Buffer.byteLength(html),
        sha256: sha256(html),
      },
      flags: args,
      command: {
        program: 'cass',
        argumentCount: cmdArgs.length,
        passwordViaStdin: Boolean(stdin),
        redacted: true,
      },
      success,
      reused: skipExportRegenerate,
      durationMs,
      error: errorText
        ? {
            bytes: Buffer.byteLength(errorText),
            sha256: sha256(errorText),
            redacted: true,
          }
        : undefined,
      diagnostics: {
        stdout: stdoutArtifact,
        stderr: stderrArtifact,
      },
      encryption,
      redaction: {
        passwordChecked: password.length > 0,
        passwordFoundInHtml,
        passwordFoundInStdout,
        passwordFoundInStderr,
        rawDiagnosticsStored: false,
      },
    });
  }

  // -----------------------------------------------------------------------------
  // Pages preview server (for OPFS / Service Worker tests)
  // -----------------------------------------------------------------------------
  const previewPort = parseInt(process.env.TEST_PAGES_PREVIEW_PORT || '8090', 10);
  const previewPassword = process.env.TEST_PAGES_PREVIEW_PASSWORD || 'test-password-123';
  const pagesBundleDir = path.join(pagesPreviewDir, 'bundle');

  const possibleBundlePaths = [
    path.join(path.dirname(cassPath), 'cass-pages-perf-bundle'),
    process.env.CARGO_TARGET_DIR ? path.join(process.env.CARGO_TARGET_DIR, 'release/cass-pages-perf-bundle') : null,
    path.join(projectRoot, 'target/release/cass-pages-perf-bundle'),
  ].filter(Boolean) as string[];

  let bundleBinPath = '';
  let bundleGenerationDiagnostics: {
    stdout: DiagnosticArtifact;
    stderr: DiagnosticArtifact;
  } | null = null;
  for (const p of possibleBundlePaths) {
    if (existsSync(p)) {
      bundleBinPath = p;
      break;
    }
  }

  if (!bundleBinPath) {
    console.warn(`Could not find cass-pages-perf-bundle binary. Checked: ${possibleBundlePaths.join(', ')}`);
  } else {
    console.log(`Using perf bundle binary: ${bundleBinPath}`);
    const bundleOutput = spawnSync(
      bundleBinPath,
      [
        '--output',
        pagesPreviewDir,
        '--preset',
        'small',
        '--password',
        previewPassword,
      ],
      { cwd: projectRoot, encoding: 'utf-8', timeout: 600_000 }
    );
    bundleGenerationDiagnostics = {
      stdout: writeDiagnosticArtifact(
        exportDir,
        'pages-preview',
        'stdout',
        bundleOutput.stdout ?? ''
      ),
      stderr: writeDiagnosticArtifact(
        exportDir,
        'pages-preview',
        'stderr',
        bundleOutput.stderr ?? ''
      ),
    };
    if (
      (bundleOutput.stdout ?? '').includes(previewPassword) ||
      (bundleOutput.stderr ?? '').includes(previewPassword)
    ) {
      redactionViolation = true;
    }
    if (!bundleOutput.error && bundleOutput.status === 0) {
      console.log(`Pages bundle ready: ${pagesBundleDir}`);
    } else {
      console.warn('Failed to generate pages preview bundle; raw process errors are redacted.');
    }
  }

  let previewUrl = '';
  let previewPid = '';
  let previewLog = path.join(pagesPreviewDir, 'preview-server.log');

  if (bundleBinPath && existsSync(pagesBundleDir)) {
    const previewArgs = [
      'pages',
      '--preview', pagesBundleDir,
      '--port', String(previewPort),
      '--no-open',
    ];

    console.log(`Starting preview server on port ${previewPort}...`);
    const previewProc = spawn(cassPath, previewArgs, { cwd: projectRoot, stdio: ['ignore', 'pipe', 'pipe'] });

    if (previewProc.stdout && previewProc.stderr) {
      const logStream = createWriteStream(previewLog, { flags: 'a' });
      previewProc.stdout.pipe(logStream);
      previewProc.stderr.pipe(logStream);
    }

    previewPid = String(previewProc.pid ?? '');

    const ready = await waitForUrl(`http://127.0.0.1:${previewPort}/index.html`, 8000);
    if (ready) {
      previewUrl = `http://127.0.0.1:${previewPort}/index.html`;
      console.log(`Preview server ready at ${previewUrl}`);
    } else {
      console.warn('Preview server failed to respond in time. Tests will skip preview checks.');
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
    paths: {
      projectRoot: sanitizePathReference(projectRoot, projectRoot),
      exportDir: sanitizePathReference(exportDir, projectRoot),
      fixturesDir: sanitizePathReference(fixturesDir, projectRoot),
      cassBinary: sanitizePathReference(cassPath, projectRoot),
    },
    exports: exportResults,
    pagesPreview: {
      port: previewPort,
      available: previewUrl.length > 0,
      pidRecorded: previewPid.length > 0,
      siteDir: sanitizePathReference(pagesBundleDir, projectRoot),
      log: sanitizePathReference(previewLog, projectRoot),
      bundleGenerationDiagnostics,
    },
    redaction: {
      rawExportProcessDiagnosticsStored: false,
      passwordLeakDetected: redactionViolation,
      exportProofViolationDetected: setupProofViolation,
    },
  };

  const metadataPath = path.join(exportDir, 'setup-metadata.json');
  writeFileSync(metadataPath, JSON.stringify(setupMetadata, null, 2));
  envContent.TEST_EXPORT_SETUP_LOG = metadataPath;

  // Write environment file
  const envPath = path.join(__dirname, '../.env.test');
  writeFileSync(
    envPath,
    Object.entries(envContent)
      .map(([k, v]) => `${k}=${v}`)
      .join('\n')
  );

  console.log('\nE2E test setup complete!');
  console.log(`Exports directory: ${exportDir}`);
  console.log(`Environment file: ${envPath}`);

  if (redactionViolation || setupProofViolation) {
    throw new Error('Export setup proof check failed; inspect sanitized setup metadata.');
  }
}

export default globalSetup;

async function waitForUrl(url: string, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await fetch(url, { method: 'GET' });
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
