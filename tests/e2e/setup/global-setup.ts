import { execSync } from 'child_process';
import { existsSync, mkdirSync, writeFileSync } from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * Global setup for HTML export E2E tests.
 * Generates test HTML exports from fixture JSONL files before tests run.
 */
async function globalSetup() {
  const startedAt = new Date();
  const projectRoot = path.resolve(__dirname, '../../..');
  const exportDir = path.resolve(__dirname, '../exports');
  const fixturesDir = path.resolve(projectRoot, 'tests/fixtures/html_export/real_sessions');

  // Ensure export directory exists
  if (!existsSync(exportDir)) {
    mkdirSync(exportDir, { recursive: true });
  }

  // Check if we can skip regeneration - if all exports exist and are recent
  const requiredExports = ['test-basic.html', 'test-encrypted.html', 'test-tool-calls.html',
                           'test-large.html', 'test-unicode.html', 'test-no-cdn.html'];
  const allExportsExist = requiredExports.every(name => {
    const exportPath = path.join(exportDir, name);
    if (!existsSync(exportPath)) return false;
    // Check file size > 1KB to ensure it's not a placeholder
    try {
      const stats = require('fs').statSync(exportPath);
      return stats.size > 1024;
    } catch {
      return false;
    }
  });

  if (allExportsExist && process.env.E2E_SKIP_REGENERATE !== '0') {
    console.log('All exports exist, skipping regeneration. Set E2E_SKIP_REGENERATE=0 to force regeneration.');
    // Write environment file with existing exports
    const envContent: Record<string, string> = {
      TEST_EXPORTS_DIR: exportDir,
      TEST_EXPORT_PASSWORD: 'test-password-123',
    };
    for (const name of requiredExports) {
      const envKey = `TEST_EXPORT_${name.replace('.html', '').toUpperCase().replace(/-/g, '_')}`;
      envContent[envKey] = path.join(exportDir, name);
    }
    const envPath = path.join(__dirname, '../.env.test');
    writeFileSync(
      envPath,
      Object.entries(envContent)
        .map(([k, v]) => `${k}=${v}`)
        .join('\n')
    );
    console.log(`Environment file: ${envPath}`);
    return;
  }

  // Build the Rust CLI if needed (with timeout to avoid blocking)
  console.log('Building cass CLI...');
  try {
    execSync('cargo build --release', { cwd: projectRoot, stdio: 'inherit', timeout: 60000 });
  } catch {
    console.warn('Cargo build failed or timed out, trying with existing binary...');
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
      args: ['--encrypt', '--password', 'test-password-123'],
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
    fixture: string;
    outputPath: string;
    args: string[];
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
    TEST_EXPORT_PASSWORD: 'test-password-123',
  };

  for (const { name, fixture, args } of exports) {
    const fixturePath = path.join(fixturesDir, fixture);
    const outputPath = path.join(exportDir, `${name}.html`);
    const envKey = `TEST_EXPORT_${name.toUpperCase().replace(/-/g, '_')}`;

    console.log(`Generating ${name}.html from ${fixture}...`);

    // Always set the env path so tests can fail loudly if exports are missing.
    envContent[envKey] = outputPath;

    const cmd = [
      cassPath,
      'export-html',
      fixturePath,
      '--output-dir', path.dirname(outputPath),
      '--filename', path.basename(outputPath),
      ...args,
    ].join(' ');

    const started = Date.now();
    let success = true;
    let errorText = '';
    let stdout = '';
    let stderr = '';

    try {
      // Use the CLI to generate export
      const output = execSync(cmd, { cwd: projectRoot, stdio: 'pipe' });
      stdout = output ? output.toString() : '';
      console.log(`  -> ${outputPath}`);
    } catch (err) {
      success = false;
      const execErr = err as {
        message?: string;
        stdout?: Buffer | string;
        stderr?: Buffer | string;
      };
      stdout = execErr?.stdout ? execErr.stdout.toString() : '';
      stderr = execErr?.stderr ? execErr.stderr.toString() : '';
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
      command: cmd,
      success,
      durationMs,
      error: errorText || undefined,
      stdout: stdout ? stdout.slice(-8000) : undefined,
      stderr: stderr ? stderr.slice(-8000) : undefined,
    });
  }

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
}

export default globalSetup;
