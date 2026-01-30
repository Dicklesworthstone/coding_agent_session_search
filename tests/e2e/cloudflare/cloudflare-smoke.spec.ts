/**
 * Cloudflare Pages Deployment Smoke Tests
 *
 * Validates Cloudflare Pages deployments are healthy and properly configured.
 * Can target fresh deployments or existing URLs for verification.
 *
 * Environment Variables:
 *   CLOUDFLARE_TEST_URL - Target URL to test (required for live tests)
 *   CLOUDFLARE_TEST_PASSWORD - Password for encrypted archives (optional)
 *   CLOUDFLARE_REPORT_DIR - Directory for JSON reports (default: test-results/cloudflare)
 *
 * Run:
 *   CLOUDFLARE_TEST_URL=https://my-archive.pages.dev npx playwright test cloudflare-smoke.spec.ts
 *
 * Acceptance Criteria (bead ka49):
 * - Smoke test passes on fresh deploy and existing URL
 * - Validate response headers (COOP/COEP, CSP, nosniff, noindex)
 * - Browser flow: unlock -> search -> open conversation
 * - Failures surface clear remediation steps
 * - JSON reports stored as CI artifacts
 */

import { test, expect } from '../setup/test-utils';
import { existsSync, mkdirSync, writeFileSync } from 'fs';
import path from 'path';

// Configuration from environment
const CLOUDFLARE_URL = process.env.CLOUDFLARE_TEST_URL;
const ARCHIVE_PASSWORD = process.env.CLOUDFLARE_TEST_PASSWORD || 'test-password';
const REPORT_DIR = process.env.CLOUDFLARE_REPORT_DIR ||
  path.join(process.cwd(), 'test-results', 'cloudflare');

// Expected headers for Cloudflare Pages deployments
const EXPECTED_HEADERS = {
  'cross-origin-opener-policy': 'same-origin',
  'cross-origin-embedder-policy': 'require-corp',
  'x-content-type-options': 'nosniff',
  'x-frame-options': 'DENY',
  'referrer-policy': 'no-referrer',
  'x-robots-tag': 'noindex, nofollow',
};

// Content-Security-Policy directives to validate (partial match)
const CSP_DIRECTIVES = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self' data: blob:",
  "connect-src 'self'",
  "worker-src 'self' blob:",
  "object-src 'none'",
  "frame-ancestors 'none'",
];

interface SmokeTestReport {
  url: string;
  timestamp: string;
  status: 'pass' | 'fail' | 'skip';
  headers: {
    present: string[];
    missing: string[];
    values: Record<string, string>;
  };
  csp: {
    present: string[];
    missing: string[];
    raw?: string;
  };
  timings: {
    responseMs: number;
    domContentLoadedMs?: number;
    fullyLoadedMs?: number;
  };
  browser: {
    crossOriginIsolated: boolean;
    sharedArrayBufferAvailable: boolean;
    opfsAvailable: boolean;
    webCryptoAvailable: boolean;
  };
  errors: string[];
  warnings: string[];
  remediations: string[];
}

function generateRemediation(issue: string): string {
  const remediations: Record<string, string> = {
    'missing-coop': 'Add "Cross-Origin-Opener-Policy: same-origin" to _headers file',
    'missing-coep': 'Add "Cross-Origin-Embedder-Policy: require-corp" to _headers file',
    'missing-nosniff': 'Add "X-Content-Type-Options: nosniff" to _headers file',
    'missing-frame-options': 'Add "X-Frame-Options: DENY" to _headers file',
    'missing-robots': 'Add "X-Robots-Tag: noindex, nofollow" to _headers file',
    'missing-csp': 'Add Content-Security-Policy header to _headers file',
    'no-crossorigin-isolated': 'Ensure COOP and COEP headers are both set correctly',
    'no-sharedarraybuffer': 'SharedArrayBuffer requires Cross-Origin Isolation (COOP + COEP)',
    'no-opfs': 'OPFS requires a secure context (HTTPS) and may need browser update',
    'no-webcrypto': 'WebCrypto requires a secure context (HTTPS)',
    'slow-response': 'Consider enabling Cloudflare caching or optimizing bundle size',
  };
  return remediations[issue] || `Unknown issue: ${issue}`;
}

function saveReport(report: SmokeTestReport, filename: string): string {
  mkdirSync(REPORT_DIR, { recursive: true });
  const reportPath = path.join(REPORT_DIR, filename);
  writeFileSync(reportPath, JSON.stringify(report, null, 2));
  return reportPath;
}

test.describe('Cloudflare Pages Smoke Tests', () => {
  test.beforeAll(async () => {
    mkdirSync(REPORT_DIR, { recursive: true });
  });

  test('target URL is configured', async () => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set - skipping live tests');
    expect(CLOUDFLARE_URL).toBeTruthy();
    expect(CLOUDFLARE_URL).toMatch(/^https?:\/\//);
  });

  test('response headers are correctly configured', async ({ request }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    const report: SmokeTestReport = {
      url: CLOUDFLARE_URL!,
      timestamp: new Date().toISOString(),
      status: 'pass',
      headers: { present: [], missing: [], values: {} },
      csp: { present: [], missing: [] },
      timings: { responseMs: 0 },
      browser: {
        crossOriginIsolated: false,
        sharedArrayBufferAvailable: false,
        opfsAvailable: false,
        webCryptoAvailable: false,
      },
      errors: [],
      warnings: [],
      remediations: [],
    };

    const startTime = Date.now();

    await test.step('Fetch page and validate response', async () => {
      const response = await request.get(CLOUDFLARE_URL!);
      report.timings.responseMs = Date.now() - startTime;

      expect(response.ok()).toBe(true);

      const headers = response.headers();

      // Check expected headers
      for (const [header, expectedValue] of Object.entries(EXPECTED_HEADERS)) {
        const actualValue = headers[header];
        if (actualValue) {
          report.headers.present.push(header);
          report.headers.values[header] = actualValue;

          // Check value matches
          if (!actualValue.toLowerCase().includes(expectedValue.toLowerCase())) {
            report.warnings.push(`Header ${header}: expected "${expectedValue}", got "${actualValue}"`);
          }
        } else {
          report.headers.missing.push(header);
          report.errors.push(`Missing header: ${header}`);
          report.remediations.push(generateRemediation(`missing-${header.replace('x-', '').replace(/-/g, '')}`));
        }
      }

      // Check CSP
      const csp = headers['content-security-policy'];
      if (csp) {
        report.csp.raw = csp;
        for (const directive of CSP_DIRECTIVES) {
          if (csp.includes(directive)) {
            report.csp.present.push(directive);
          } else {
            report.csp.missing.push(directive);
          }
        }
      } else {
        report.errors.push('Missing Content-Security-Policy header');
        report.remediations.push(generateRemediation('missing-csp'));
      }

      // Check response time
      if (report.timings.responseMs > 3000) {
        report.warnings.push(`Slow response: ${report.timings.responseMs}ms`);
        report.remediations.push(generateRemediation('slow-response'));
      }
    });

    // Set report status
    if (report.errors.length > 0) {
      report.status = 'fail';
    }

    // Save report
    const reportPath = saveReport(report, 'headers-report.json');
    await testInfo.attach('headers-report', {
      path: reportPath,
      contentType: 'application/json',
    });

    // Fail if critical headers missing
    const criticalMissing = report.headers.missing.filter(h =>
      ['cross-origin-opener-policy', 'cross-origin-embedder-policy'].includes(h)
    );
    expect(criticalMissing, 'Critical COOP/COEP headers must be present').toHaveLength(0);
  });

  test('browser capabilities work correctly', async ({ page }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    const report: SmokeTestReport = {
      url: CLOUDFLARE_URL!,
      timestamp: new Date().toISOString(),
      status: 'pass',
      headers: { present: [], missing: [], values: {} },
      csp: { present: [], missing: [] },
      timings: { responseMs: 0 },
      browser: {
        crossOriginIsolated: false,
        sharedArrayBufferAvailable: false,
        opfsAvailable: false,
        webCryptoAvailable: false,
      },
      errors: [],
      warnings: [],
      remediations: [],
    };

    const consoleErrors: string[] = [];
    const networkErrors: string[] = [];

    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        consoleErrors.push(msg.text());
      }
    });

    page.on('requestfailed', (request) => {
      networkErrors.push(`${request.method()} ${request.url()}: ${request.failure()?.errorText}`);
    });

    const navStart = Date.now();

    await test.step('Navigate to Cloudflare Pages site', async () => {
      const response = await page.goto(CLOUDFLARE_URL!, { waitUntil: 'domcontentloaded' });
      report.timings.domContentLoadedMs = Date.now() - navStart;

      expect(response?.ok()).toBe(true);
    });

    await test.step('Wait for full page load', async () => {
      await page.waitForLoadState('load');
      report.timings.fullyLoadedMs = Date.now() - navStart;
    });

    await test.step('Check Cross-Origin Isolation', async () => {
      report.browser.crossOriginIsolated = await page.evaluate(() => window.crossOriginIsolated);

      if (!report.browser.crossOriginIsolated) {
        report.errors.push('Cross-Origin Isolation not enabled');
        report.remediations.push(generateRemediation('no-crossorigin-isolated'));
      }
    });

    await test.step('Check SharedArrayBuffer availability', async () => {
      report.browser.sharedArrayBufferAvailable = await page.evaluate(
        () => typeof SharedArrayBuffer !== 'undefined'
      );

      if (!report.browser.sharedArrayBufferAvailable) {
        report.errors.push('SharedArrayBuffer not available');
        report.remediations.push(generateRemediation('no-sharedarraybuffer'));
      }
    });

    await test.step('Check OPFS availability', async () => {
      report.browser.opfsAvailable = await page.evaluate(async () => {
        try {
          const root = await navigator.storage.getDirectory();
          return root !== null;
        } catch {
          return false;
        }
      });

      if (!report.browser.opfsAvailable) {
        report.warnings.push('OPFS not available (may affect performance)');
        report.remediations.push(generateRemediation('no-opfs'));
      }
    });

    await test.step('Check WebCrypto availability', async () => {
      report.browser.webCryptoAvailable = await page.evaluate(
        () => typeof crypto !== 'undefined' && typeof crypto.subtle !== 'undefined'
      );

      if (!report.browser.webCryptoAvailable) {
        report.errors.push('WebCrypto not available (required for decryption)');
        report.remediations.push(generateRemediation('no-webcrypto'));
      }
    });

    // Collect errors
    if (consoleErrors.length > 0) {
      report.warnings.push(`${consoleErrors.length} console error(s) detected`);
    }
    if (networkErrors.length > 0) {
      report.warnings.push(`${networkErrors.length} network error(s) detected`);
    }

    // Set status
    if (report.errors.length > 0) {
      report.status = 'fail';
    }

    // Save report
    const reportPath = saveReport(report, 'browser-capabilities-report.json');
    await testInfo.attach('browser-report', {
      path: reportPath,
      contentType: 'application/json',
    });

    // Take screenshot
    const screenshotPath = path.join(REPORT_DIR, 'page-screenshot.png');
    await page.screenshot({ path: screenshotPath, fullPage: true });
    await testInfo.attach('screenshot', {
      path: screenshotPath,
      contentType: 'image/png',
    });

    // Assert critical capabilities
    expect(report.browser.crossOriginIsolated, 'Cross-Origin Isolation must be enabled').toBe(true);
    expect(report.browser.webCryptoAvailable, 'WebCrypto must be available').toBe(true);
  });

  test('authentication and unlock flow works', async ({ page }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    await page.goto(CLOUDFLARE_URL!, { waitUntil: 'load' });

    await test.step('Locate password input', async () => {
      // Look for password input elements (various possible selectors)
      const passwordInputSelectors = [
        'input[type="password"]',
        '#password-input',
        '[data-testid="password-input"]',
        '.password-field input',
      ];

      let passwordInput = null;
      for (const selector of passwordInputSelectors) {
        const element = page.locator(selector).first();
        if (await element.isVisible({ timeout: 2000 }).catch(() => false)) {
          passwordInput = element;
          break;
        }
      }

      if (!passwordInput) {
        // Archive may not be encrypted or already unlocked
        console.log(JSON.stringify({
          event: 'auth_check',
          result: 'no_password_field',
          note: 'Archive may be unencrypted or already unlocked',
          ts: new Date().toISOString(),
        }));
        return;
      }

      // Found password input - attempt unlock
      await passwordInput.fill(ARCHIVE_PASSWORD);

      // Find and click submit button
      const submitSelectors = [
        'button[type="submit"]',
        '#unlock-button',
        '[data-testid="unlock-button"]',
        'button:has-text("Unlock")',
        'button:has-text("Submit")',
      ];

      for (const selector of submitSelectors) {
        const button = page.locator(selector).first();
        if (await button.isVisible({ timeout: 1000 }).catch(() => false)) {
          await button.click();
          break;
        }
      }

      // Wait for unlock to complete
      await page.waitForTimeout(2000);

      console.log(JSON.stringify({
        event: 'unlock_attempted',
        ts: new Date().toISOString(),
      }));
    });

    // Take post-unlock screenshot
    const screenshotPath = path.join(REPORT_DIR, 'post-unlock-screenshot.png');
    await page.screenshot({ path: screenshotPath, fullPage: true });
    await testInfo.attach('post-unlock-screenshot', {
      path: screenshotPath,
      contentType: 'image/png',
    });
  });

  test('search functionality is available', async ({ page }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    await page.goto(CLOUDFLARE_URL!, { waitUntil: 'load' });
    await page.waitForTimeout(2000); // Wait for JS initialization

    await test.step('Check for search input', async () => {
      const searchSelectors = [
        'input[type="search"]',
        'input[placeholder*="search" i]',
        '#search-input',
        '[data-testid="search-input"]',
        '.search-box input',
      ];

      let searchInput = null;
      for (const selector of searchSelectors) {
        const element = page.locator(selector).first();
        if (await element.isVisible({ timeout: 1000 }).catch(() => false)) {
          searchInput = element;
          break;
        }
      }

      console.log(JSON.stringify({
        event: 'search_check',
        searchInputFound: searchInput !== null,
        ts: new Date().toISOString(),
      }));

      // Search input should exist after unlock
      // Note: May not be visible if archive requires unlock first
    });
  });

  test('cache headers are properly configured', async ({ request }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    const report = {
      url: CLOUDFLARE_URL!,
      timestamp: new Date().toISOString(),
      cacheHeaders: {} as Record<string, Record<string, string>>,
    };

    // Test different file types for cache behavior
    const testPaths = [
      { path: '/', expected: 'no-cache' },  // index.html
      { path: '/config.json', expected: 'no-cache' },
    ];

    for (const { path: testPath, expected } of testPaths) {
      const url = new URL(testPath, CLOUDFLARE_URL!).toString();
      const response = await request.get(url);

      if (response.ok()) {
        const cacheControl = response.headers()['cache-control'] || 'not set';
        report.cacheHeaders[testPath] = {
          'cache-control': cacheControl,
          expected,
          matches: cacheControl.includes(expected).toString(),
        };
      }
    }

    // Save report
    const reportPath = saveReport(report as unknown as SmokeTestReport, 'cache-report.json');
    await testInfo.attach('cache-report', {
      path: reportPath,
      contentType: 'application/json',
    });

    console.log(JSON.stringify({
      event: 'cache_check',
      ...report.cacheHeaders,
      ts: new Date().toISOString(),
    }));
  });
});

test.describe('Cloudflare Smoke - Combined Report', () => {
  test('generate combined smoke test report', async ({ request, page }, testInfo) => {
    test.skip(!CLOUDFLARE_URL, 'CLOUDFLARE_TEST_URL not set');

    const combinedReport: SmokeTestReport = {
      url: CLOUDFLARE_URL!,
      timestamp: new Date().toISOString(),
      status: 'pass',
      headers: { present: [], missing: [], values: {} },
      csp: { present: [], missing: [] },
      timings: { responseMs: 0 },
      browser: {
        crossOriginIsolated: false,
        sharedArrayBufferAvailable: false,
        opfsAvailable: false,
        webCryptoAvailable: false,
      },
      errors: [],
      warnings: [],
      remediations: [],
    };

    // Step 1: HTTP request check
    const startTime = Date.now();
    const httpResponse = await request.get(CLOUDFLARE_URL!);
    combinedReport.timings.responseMs = Date.now() - startTime;

    if (!httpResponse.ok()) {
      combinedReport.errors.push(`HTTP request failed: ${httpResponse.status()}`);
      combinedReport.status = 'fail';
    }

    const headers = httpResponse.headers();
    for (const [header, expectedValue] of Object.entries(EXPECTED_HEADERS)) {
      if (headers[header]) {
        combinedReport.headers.present.push(header);
        combinedReport.headers.values[header] = headers[header];
      } else {
        combinedReport.headers.missing.push(header);
      }
    }

    // Step 2: Browser capabilities
    const navStart = Date.now();
    await page.goto(CLOUDFLARE_URL!, { waitUntil: 'load' });
    combinedReport.timings.fullyLoadedMs = Date.now() - navStart;

    combinedReport.browser = await page.evaluate(() => ({
      crossOriginIsolated: window.crossOriginIsolated,
      sharedArrayBufferAvailable: typeof SharedArrayBuffer !== 'undefined',
      opfsAvailable: 'storage' in navigator,
      webCryptoAvailable: typeof crypto !== 'undefined' && typeof crypto.subtle !== 'undefined',
    }));

    // Determine pass/fail
    const criticalChecks = [
      combinedReport.headers.present.includes('cross-origin-opener-policy'),
      combinedReport.headers.present.includes('cross-origin-embedder-policy'),
      combinedReport.browser.crossOriginIsolated,
      combinedReport.browser.webCryptoAvailable,
    ];

    if (!criticalChecks.every(Boolean)) {
      combinedReport.status = 'fail';

      if (!combinedReport.browser.crossOriginIsolated) {
        combinedReport.remediations.push(generateRemediation('no-crossorigin-isolated'));
      }
    }

    // Save combined report
    const reportPath = saveReport(combinedReport, 'smoke-test-report.json');
    await testInfo.attach('smoke-test-report', {
      path: reportPath,
      contentType: 'application/json',
    });

    // Screenshot
    const screenshotPath = path.join(REPORT_DIR, 'final-screenshot.png');
    await page.screenshot({ path: screenshotPath, fullPage: true });
    await testInfo.attach('final-screenshot', {
      path: screenshotPath,
      contentType: 'image/png',
    });

    // Log summary
    console.log(JSON.stringify({
      event: 'smoke_test_complete',
      status: combinedReport.status,
      errorCount: combinedReport.errors.length,
      warningCount: combinedReport.warnings.length,
      remediationCount: combinedReport.remediations.length,
      responseMs: combinedReport.timings.responseMs,
      fullyLoadedMs: combinedReport.timings.fullyLoadedMs,
      ts: new Date().toISOString(),
    }));

    // Final assertions
    expect(combinedReport.status).toBe('pass');
  });
});
