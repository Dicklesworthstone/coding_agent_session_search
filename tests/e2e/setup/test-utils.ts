import {
  test as base,
  expect,
  Page,
  ConsoleMessage,
  Request,
  BrowserContext,
} from '@playwright/test';
import { createHash } from 'crypto';
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
  text: SanitizedDiagnostic;
  location?: {
    url?: SanitizedReference;
    lineNumber?: number;
    columnNumber?: number;
  };
  time: string;
};

type PageErrorEntry = {
  name?: string;
  message: SanitizedDiagnostic;
  stack?: SanitizedDiagnostic;
  time: string;
};

type RequestFailureEntry = {
  url: SanitizedReference;
  method: string;
  resourceType: string;
  failure?: SanitizedDiagnostic;
  time: string;
};

type SanitizedDiagnostic = {
  category: string;
  bytes: number;
  sha256: string;
  redacted: true;
};

type SanitizedReference = {
  scheme: string;
  scope: 'workspace' | 'external' | 'network' | 'unknown';
  segments?: number;
  port?: string;
  hostSha256?: string;
  pathSha256: string;
  redacted: true;
};

function nowIso(): string {
  return new Date().toISOString();
}

function sanitizeDiagnostic(value: string, category: string): SanitizedDiagnostic {
  return {
    category,
    bytes: Buffer.byteLength(value),
    sha256: createHash('sha256').update(value).digest('hex'),
    redacted: true,
  };
}

function sanitizePathReference(filePath: string): SanitizedReference {
  const resolved = path.resolve(filePath);
  const relative = path.relative(process.cwd(), resolved);
  const inWorkspace =
    relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..');
  return {
    scheme: 'file',
    scope: inWorkspace ? 'workspace' : 'external',
    segments: resolved.split(path.sep).filter(Boolean).length,
    pathSha256: createHash('sha256').update(resolved).digest('hex'),
    redacted: true,
  };
}

function sanitizeUrlReference(value: string): SanitizedReference {
  try {
    const url = new URL(value);
    if (url.protocol === 'file:') {
      return sanitizePathReference(decodeURIComponent(url.pathname));
    }
    return {
      scheme: url.protocol.replace(/:$/, ''),
      scope: 'network',
      port: url.port || undefined,
      hostSha256: createHash('sha256').update(url.hostname).digest('hex'),
      pathSha256: createHash('sha256').update(url.pathname).digest('hex'),
      redacted: true,
    };
  } catch {
    return {
      scheme: 'unknown',
      scope: 'unknown',
      pathSha256: createHash('sha256').update(value).digest('hex'),
      redacted: true,
    };
  }
}

function sanitizeOptionalDiagnostic(
  value: string | undefined,
  category: string
): SanitizedDiagnostic | undefined {
  return value ? sanitizeDiagnostic(value, category) : undefined;
}

function readJsonIfExists(filePath?: string): unknown | null {
  if (!filePath || !existsSync(filePath)) {
    return null;
  }
  try {
    return JSON.parse(readFileSync(filePath, 'utf-8'));
  } catch (err) {
    return {
      error: 'Failed to parse JSON log',
      details: sanitizeDiagnostic(String(err), 'setup-log-parse-error'),
    };
  }
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
  previewUrl: string;
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
      const location = msg.location();
      consoleEntries.push({
        type: msg.type(),
        text: sanitizeDiagnostic(msg.text(), `console-${msg.type()}`),
        location: {
          url: location.url ? sanitizeUrlReference(location.url) : undefined,
          lineNumber: location.lineNumber,
          columnNumber: location.columnNumber,
        },
        time: nowIso(),
      });
    };

    const onPageError = (error: Error) => {
      pageErrors.push({
        name: error.name,
        message: sanitizeDiagnostic(error.message, 'page-error-message'),
        stack: sanitizeOptionalDiagnostic(error.stack, 'page-error-stack'),
        time: nowIso(),
      });
    };

    const onRequestFailed = (request: Request) => {
      requestFailures.push({
        url: sanitizeUrlReference(request.url()),
        method: request.method(),
        resourceType: request.resourceType(),
        failure: sanitizeOptionalDiagnostic(
          request.failure()?.errorText,
          'request-failure'
        ),
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

    let pageUrl: SanitizedReference | null = null;
    try {
      pageUrl = sanitizeUrlReference(page.url());
    } catch {
      pageUrl = null;
    }
    const setupLog = readJsonIfExists(process.env.TEST_EXPORT_SETUP_LOG);
    const startTime = (testInfo as typeof testInfo & { startTime?: Date }).startTime;
    const exportPath = (value?: string): SanitizedReference | null =>
      value ? sanitizePathReference(value) : null;
    const logPayload = {
      test: {
        title: testInfo.title,
        file: exportPath(testInfo.file),
        project: testInfo.project?.name,
        status: testInfo.status,
        expectedStatus: testInfo.expectedStatus,
        retry: testInfo.retry,
      },
      runtime: {
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        startTime: startTime?.toISOString(),
        durationMs: testInfo.duration,
      },
      environment: {
        node: process.version,
        platform: process.platform,
        arch: process.arch,
        exportsDir: exportPath(process.env.TEST_EXPORTS_DIR),
        exportPaths: {
          basic: exportPath(process.env.TEST_EXPORT_TEST_BASIC),
          encrypted: exportPath(process.env.TEST_EXPORT_TEST_ENCRYPTED),
          toolCalls: exportPath(process.env.TEST_EXPORT_TEST_TOOL_CALLS),
          large: exportPath(process.env.TEST_EXPORT_TEST_LARGE),
          unicode: exportPath(process.env.TEST_EXPORT_TEST_UNICODE),
          noCdn: exportPath(process.env.TEST_EXPORT_TEST_NO_CDN),
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
      redaction: {
        rawConsoleTextStored: false,
        rawPageErrorsStored: false,
        rawRequestUrlsStored: false,
      },
    };

    await testInfo.attach(`browser-logs-${testInfo.project?.name ?? 'default'}`, {
      body: Buffer.from(JSON.stringify(logPayload, null, 2)),
      contentType: 'application/json',
    });
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

  previewUrl: async ({}, use) => {
    const previewUrl = process.env.TEST_PAGES_PREVIEW_URL || '';
    await use(previewUrl);
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

export async function grantClipboardPermissionsIfSupported(
  context: BrowserContext,
  browserName: string,
  permissions: Array<'clipboard-read' | 'clipboard-write'> = ['clipboard-read', 'clipboard-write']
): Promise<boolean> {
  if (browserName !== 'chromium') {
    return false;
  }

  try {
    await context.grantPermissions(permissions);
    return true;
  } catch (err) {
    const summary = sanitizeDiagnostic(String(err), 'clipboard-permission-error');
    console.log(
      `[browser-capability] Clipboard permission grant unavailable: bytes=${summary.bytes} sha256=${summary.sha256}`
    );
    return false;
  }
}

export async function focusFirstKeyboardReachableElement(
  page: Page,
  maxTabs = 8
): Promise<boolean> {
  for (let i = 0; i < maxTabs; i++) {
    await page.keyboard.press('Tab');
    const hasFocus = await page.evaluate(() => {
      const el = document.activeElement;
      return !!el && el !== document.body && el !== document.documentElement;
    });
    if (hasFocus) {
      return true;
    }
  }
  return false;
}

/**
 * Utility to collect console errors during test.
 */
export async function collectConsoleErrors(page: Page): Promise<string[]> {
  const errors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      const summary = sanitizeDiagnostic(msg.text(), 'console-error');
      errors.push(
        `${summary.category}:bytes=${summary.bytes}:sha256=${summary.sha256}`
      );
    }
  });
  return errors;
}

/**
 * Start collecting browser failures before navigation. Load-time failures are
 * the most important ones for self-contained exports, so callers must create
 * this collector before `gotoFile`/`page.goto` and assert both arrays after
 * deferred scripts have settled.
 */
export function collectBrowserErrors(page: Page): {
  consoleErrors: string[];
  pageErrors: string[];
} {
  const consoleErrors: string[] = [];
  const pageErrors: string[] = [];
  page.on('console', (message) => {
    if (message.type() === 'error') {
      const summary = sanitizeDiagnostic(message.text(), 'console-error');
      consoleErrors.push(
        `${summary.category}:bytes=${summary.bytes}:sha256=${summary.sha256}`
      );
    }
  });
  page.on('pageerror', (error) => {
    const summary = sanitizeDiagnostic(error.message, 'page-error');
    pageErrors.push(
      `${summary.category}:bytes=${summary.bytes}:sha256=${summary.sha256}`
    );
  });
  return { consoleErrors, pageErrors };
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
  return (await page.locator('html').getAttribute('data-theme')) || 'unknown';
}
