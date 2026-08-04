/**
 * Playwright JSONL Reporter
 *
 * Emits structured JSONL events following the schema in docs/reference/E2E_LOGGING_SCHEMA.md.
 * Output: test-results/e2e/playwright_<timestamp>.jsonl
 *
 * Usage in playwright.config.ts:
 *   reporter: [
 *     ['./reporters/jsonl-reporter.ts'],
 *     // ... other reporters
 *   ],
 */

import {
  Reporter,
  TestCase,
  TestResult,
  TestStep,
  FullConfig,
  Suite,
  FullResult,
} from '@playwright/test/reporter';
import { execSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

interface E2eEnvironment {
  git_sha: string | null;
  git_branch: string | null;
  os: string;
  arch: string;
  rust_version?: string;
  node_version: string;
  cass_version?: string;
  ci: boolean;
}

interface E2eTestInfo {
  name: string;
  suite: string;
  file?: string;
  line?: number;
}

interface E2eTestResult {
  status: string;
  duration_ms: number;
  retries?: number;
}

interface E2ePhase {
  name: string;
  description?: string;
}

interface E2eError {
  message: string;
  type?: string;
  stack?: string;
}

interface E2eMetrics {
  duration_ms?: number;
  memory_bytes?: number;
  throughput_per_sec?: number;
  [key: string]: number | string | boolean | undefined;
}

interface E2eRunSummary {
  total: number;
  passed: number;
  failed: number;
  skipped: number;
  flaky?: number;
  duration_ms: number;
}

type E2eEvent =
  | { event: 'run_start'; env: E2eEnvironment; config?: Record<string, unknown> }
  | { event: 'test_start'; test: E2eTestInfo }
  | { event: 'test_end'; test: E2eTestInfo; result: E2eTestResult; error?: E2eError }
  | { event: 'phase_start'; test: E2eTestInfo; phase: E2ePhase }
  | { event: 'phase_end'; test: E2eTestInfo; phase: E2ePhase; duration_ms: number }
  | { event: 'metrics'; test: E2eTestInfo; name: string; metrics: E2eMetrics }
  | { event: 'run_end'; summary: E2eRunSummary; exit_code: number };

function nowIso(): string {
  return new Date().toISOString();
}

function timestampId(): string {
  const now = new Date();
  const pad = (n: number, len = 2) => n.toString().padStart(len, '0');
  return `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}_${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
}

function randomSuffix(): string {
  return Math.random().toString(16).slice(2, 8);
}

function execOrNull(cmd: string): string | null {
  try {
    return execSync(cmd, { encoding: 'utf-8', stdio: ['pipe', 'pipe', 'pipe'] }).trim();
  } catch {
    return null;
  }
}

function slugify(value: string): string {
  const slug = value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return slug.length > 0 ? slug : 'phase';
}

function captureEnvironment(): E2eEnvironment {
  return {
    git_sha: execOrNull('git rev-parse --short HEAD'),
    git_branch: execOrNull('git rev-parse --abbrev-ref HEAD'),
    os: process.platform,
    arch: process.arch,
    rust_version: execOrNull('rustc --version')?.split(' ')[1] ?? undefined,
    node_version: process.version,
    cass_version: process.env.CARGO_PKG_VERSION ?? execOrNull('cass --version')?.split(' ')[1] ?? undefined,
    ci: Boolean(process.env.CI || process.env.GITHUB_ACTIONS),
  };
}

function testStatusToE2e(status: TestResult['status']): string {
  switch (status) {
    case 'passed':
      return 'pass';
    case 'failed':
    case 'timedOut':
    case 'interrupted':
      return 'fail';
    case 'skipped':
      return 'skip';
    default:
      return 'fail';
  }
}

class JsonlReporter implements Reporter {
  private runId: string;
  private outputPath: string;
  private stream: fs.WriteStream | null = null;
  private env: E2eEnvironment;
  private startTime: number = 0;
  private stats = { total: 0, passed: 0, failed: 0, skipped: 0, flaky: 0 };

  constructor() {
    const ts = timestampId();
    this.runId = `${ts}_${randomSuffix()}`;
    this.env = captureEnvironment();

    // Determine output path (directory created in onBegin to survive Playwright's
    // outputDir cleanup which runs between constructor and onBegin)
    const projectRoot = process.cwd();
    this.outputPath = path.join(projectRoot, 'test-results', 'e2e', `playwright_${ts}.jsonl`);
  }

  private writeEvent(eventData: E2eEvent): void {
    if (!this.stream) return;

    const fullEvent = {
      ts: nowIso(),
      run_id: this.runId,
      runner: 'playwright',
      ...eventData,
    };

    this.stream.write(JSON.stringify(fullEvent) + '\n');
  }

  private getTestInfo(test: TestCase): E2eTestInfo {
    // Get file path relative to project root
    const file = test.location?.file
      ? path.relative(process.cwd(), test.location.file)
      : undefined;

    // Build suite name from parent titles
    const suiteParts: string[] = [];
    let parent: Suite | undefined = test.parent;
    while (parent) {
      if (parent.title) {
        suiteParts.unshift(parent.title);
      }
      parent = parent.parent;
    }

    return {
      name: test.title,
      suite: suiteParts.join(' > ') || 'default',
      file,
      line: test.location?.line,
    };
  }

  onBegin(config: FullConfig, _suite: Suite): void {
    this.startTime = Date.now();
    // Create output directory here (not in constructor) because Playwright cleans
    // its default outputDir (test-results/) between reporter construction and onBegin
    fs.mkdirSync(path.dirname(this.outputPath), { recursive: true });
    this.stream = fs.createWriteStream(this.outputPath, { flags: 'a' });

    this.writeEvent({
      event: 'run_start',
      env: this.env,
      config: {
        projects: config.projects.map(p => p.name),
        workers: config.workers,
        retries: config.projects[0]?.retries ?? 0,
        timeout: config.projects[0]?.timeout ?? 30000,
      },
    });
  }

  onTestBegin(test: TestCase, _result: TestResult): void {
    this.writeEvent({
      event: 'test_start',
      test: this.getTestInfo(test),
    });
  }

  onTestEnd(test: TestCase, result: TestResult): void {
    this.stats.total++;

    const e2eStatus = testStatusToE2e(result.status);

    // Track stats
    if (result.status === 'passed') {
      this.stats.passed++;
    } else if (result.status === 'skipped') {
      this.stats.skipped++;
    } else {
      this.stats.failed++;
    }

    // Check for flaky (passed on retry)
    if (result.status === 'passed' && result.retry > 0) {
      this.stats.flaky++;
    }

    const testResult: E2eTestResult = {
      status: e2eStatus,
      duration_ms: result.duration,
      retries: result.retry,
    };

    let error: E2eError | undefined;
    if (result.error) {
      error = {
        message: result.error.message || 'Unknown error',
        type: 'TestError',
        stack: result.error.stack,
      };
    }

    this.writeEvent({
      event: 'test_end',
      test: this.getTestInfo(test),
      result: testResult,
      ...(error && { error }),
    });

    // Emit metrics events from attachments
    this.emitMetricsFromAttachments(test, result);
  }

  private emitMetricsFromAttachments(test: TestCase, result: TestResult): void {
    for (const attachment of result.attachments) {
      if (attachment.name !== 'metrics' || !attachment.body) {
        continue;
      }

      let raw: string | null = null;
      if (typeof attachment.body === 'string') {
        raw = attachment.body;
      } else if (Buffer.isBuffer(attachment.body)) {
        raw = attachment.body.toString('utf-8');
      }

      if (!raw) {
        continue;
      }

      try {
        const metricsData = JSON.parse(raw) as Record<string, unknown>;
        const metricName =
          typeof metricsData.name === 'string' && metricsData.name.length > 0
            ? metricsData.name
            : test.title;

        let metrics: E2eMetrics = {};
        if (
          metricsData.metrics &&
          typeof metricsData.metrics === 'object' &&
          !Array.isArray(metricsData.metrics) &&
          metricsData.metrics !== null
        ) {
          metrics = metricsData.metrics as E2eMetrics;
        } else {
          const { name, metrics: _metrics, ...rest } = metricsData;
          metrics = rest as E2eMetrics;
        }

        this.writeEvent({
          event: 'metrics',
          test: this.getTestInfo(test),
          name: metricName,
          metrics,
        });
      } catch {
        // Silently ignore malformed metrics attachments
      }
    }
  }

  onStepBegin(test: TestCase, _result: TestResult, step: TestStep): void {
    if (step.category !== 'test.step') return;

    this.writeEvent({
      event: 'phase_start',
      test: this.getTestInfo(test),
      phase: {
        name: slugify(step.title || 'phase'),
        description: step.title || undefined,
      },
    });
  }

  onStepEnd(test: TestCase, _result: TestResult, step: TestStep): void {
    if (step.category !== 'test.step') return;

    const duration = Number.isFinite(step.duration) ? Math.max(0, Math.round(step.duration)) : 0;

    this.writeEvent({
      event: 'phase_end',
      test: this.getTestInfo(test),
      phase: {
        name: slugify(step.title || 'phase'),
        description: step.title || undefined,
      },
      duration_ms: duration,
    });
  }

  onEnd(result: FullResult): void {
    const duration = Date.now() - this.startTime;

    this.writeEvent({
      event: 'run_end',
      summary: {
        total: this.stats.total,
        passed: this.stats.passed,
        failed: this.stats.failed,
        skipped: this.stats.skipped,
        flaky: this.stats.flaky,
        duration_ms: duration,
      },
      exit_code: result.status === 'passed' ? 0 : 1,
    });

    if (this.stream) {
      this.stream.end();
      this.stream = null;
    }

    // Log output path for visibility
    console.log(`\nJSONL E2E log written to: ${this.outputPath}`);
  }
}

export default JsonlReporter;
