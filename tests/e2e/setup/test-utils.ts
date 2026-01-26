import { test as base, expect, Page, ConsoleMessage, Request } from '@playwright/test';
import { readFileSync, existsSync } from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// Load environment variables from .env.test
const envPath = path.resolve(__dirname, '../.env.test');
if (existsSync(envPath)) {
  const envContent = readFileSync(envPath, 'utf-8');
  for (const rawLine of envContent.split('\n')) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) {
      continue;
    }
    const [key, ...valueParts] = line.split('=');
    if (key && valueParts.length > 0) {
      process.env[key] = valueParts.join('=');
    }
  }
}

type ConsoleEntry = {
  type: string;
  text: string;
  location?: { url?: string; lineNumber?: number; columnNumber?: number };
  time: string;
};

type PageErrorEntry = {
  name?: string;
  message: string;
  stack?: string;
  time: string;
};

type RequestFailureEntry = {
  url: string;
  method: string;
  resourceType: string;
  failure?: string;
  time: string;
};

function nowIso(): string {
  return new Date().toISOString();
}

function readJsonIfExists(filePath?: string): unknown | null {
  if (!filePath || !existsSync(filePath)) {
    return null;
  }
  try {
    return JSON.parse(readFileSync(filePath, 'utf-8'));
  } catch (err) {
    return { error: 'Failed to parse JSON log', details: String(err) };
  }
}

function shouldAttachLogs(status: string | undefined, expected: string | undefined): boolean {
  const always = process.env.E2E_LOG_ALWAYS === '1' || process.env.E2E_LOG_ALWAYS === 'true';
  return always || status !== expected;
}

/**
 * Test fixtures for HTML export tests.
 */
export interface TestFixtures {
  exportPath: string;
  encryptedExportPath: string;
  toolCallsExportPath: string;
  largeExportPath: string;
  unicodeExportPath: string;
  noCdnExportPath: string;
  password: string;
}

/**
 * Extended test with HTML export fixtures.
 */
export const test = base.extend<TestFixtures>({
  page: async ({ page }, use, testInfo) => {
    const consoleEntries: ConsoleEntry[] = [];
    const pageErrors: PageErrorEntry[] = [];
    const requestFailures: RequestFailureEntry[] = [];

    const onConsole = (msg: ConsoleMessage) => {
      consoleEntries.push({
        type: msg.type(),
        text: msg.text(),
        location: msg.location(),
        time: nowIso(),
      });
    };

    const onPageError = (error: Error) => {
      pageErrors.push({
        name: error.name,
        message: error.message,
        stack: error.stack,
        time: nowIso(),
      });
    };

    const onRequestFailed = (request: Request) => {
      requestFailures.push({
        url: request.url(),
        method: request.method(),
        resourceType: request.resourceType(),
        failure: request.failure()?.errorText,
        time: nowIso(),
      });
    };

    page.on('console', onConsole);
    page.on('pageerror', onPageError);
    page.on('requestfailed', onRequestFailed);

    await use(page);

    page.off('console', onConsole);
    page.off('pageerror', onPageError);
    page.off('requestfailed', onRequestFailed);

    if (shouldAttachLogs(testInfo.status, testInfo.expectedStatus)) {
      let pageUrl: string | null = null;
      try {
        pageUrl = page.url();
      } catch {
        pageUrl = null;
      }
      const setupLog = readJsonIfExists(process.env.TEST_EXPORT_SETUP_LOG);
      const logPayload = {
        test: {
          title: testInfo.title,
          file: testInfo.file,
          project: testInfo.project?.name,
          status: testInfo.status,
          expectedStatus: testInfo.expectedStatus,
          retry: testInfo.retry,
        },
        runtime: {
          workerIndex: testInfo.workerIndex,
          parallelIndex: testInfo.parallelIndex,
          startTime: testInfo.startTime?.toISOString?.() ?? testInfo.startTime,
          durationMs: testInfo.duration,
        },
        environment: {
          node: process.version,
          platform: process.platform,
          arch: process.arch,
          exportsDir: process.env.TEST_EXPORTS_DIR,
          exportPaths: {
            basic: process.env.TEST_EXPORT_TEST_BASIC,
            encrypted: process.env.TEST_EXPORT_TEST_ENCRYPTED,
            toolCalls: process.env.TEST_EXPORT_TEST_TOOL_CALLS,
            large: process.env.TEST_EXPORT_TEST_LARGE,
            unicode: process.env.TEST_EXPORT_TEST_UNICODE,
            noCdn: process.env.TEST_EXPORT_TEST_NO_CDN,
          },
        },
        page: {
          url: pageUrl,
        },
        setup: setupLog,
        logs: {
          console: consoleEntries,
          pageErrors,
          requestFailures,
        },
      };

      await testInfo.attach(`browser-logs-${testInfo.project?.name ?? 'default'}`, {
        body: Buffer.from(JSON.stringify(logPayload, null, 2)),
        contentType: 'application/json',
      });
    }
  },

  exportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_BASIC || '';
    await use(exportPath);
  },

  encryptedExportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_ENCRYPTED || '';
    await use(exportPath);
  },

  toolCallsExportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_TOOL_CALLS || '';
    await use(exportPath);
  },

  largeExportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_LARGE || '';
    await use(exportPath);
  },

  unicodeExportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_UNICODE || '';
    await use(exportPath);
  },

  noCdnExportPath: async ({}, use) => {
    const exportPath = process.env.TEST_EXPORT_TEST_NO_CDN || '';
    await use(exportPath);
  },

  password: async ({}, use) => {
    await use(process.env.TEST_EXPORT_PASSWORD || 'test-password-123');
  },
});

export { expect };

/**
 * Navigate to a local file with appropriate options for file:// URLs.
 * Uses domcontentloaded for faster, more reliable navigation.
 */
export async function gotoFile(page: Page, filePath: string): Promise<void> {
  await page.goto(`file://${filePath}`, { waitUntil: 'domcontentloaded' });
}

/**
 * Utility to collect console errors during test.
 */
export async function collectConsoleErrors(page: Page): Promise<string[]> {
  const errors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      errors.push(msg.text());
    }
  });
  return errors;
}

/**
 * Utility to wait for page to be fully loaded including lazy resources.
 * For file:// URLs, we use domcontentloaded which is faster and more reliable.
 */
export async function waitForPageReady(page: Page): Promise<void> {
  // For local file URLs, domcontentloaded is sufficient and more reliable
  await page.waitForLoadState('domcontentloaded');
  // Stabilize animations/transitions to avoid flake from entrance effects
  await page.addStyleTag({
    content: `
*,
*::before,
*::after {
  animation-duration: 0s !important;
  animation-delay: 0s !important;
  transition-duration: 0s !important;
  transition-delay: 0s !important;
  scroll-behavior: auto !important;
}
.message {
  opacity: 1 !important;
  transform: none !important;
}
`,
  });
  // Short wait for any immediate scripts to run
  await page.waitForTimeout(150);
}

/**
 * Count messages in the rendered HTML.
 */
export async function countMessages(page: Page): Promise<number> {
  return page.locator('.message').count();
}

/**
 * Get the current theme from the page.
 */
export async function getCurrentTheme(page: Page): Promise<string> {
  return page.locator('html').getAttribute('data-theme') || 'unknown';
}
