import { defineConfig, devices } from '@playwright/test';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * Playwright configuration for HTML export E2E tests.
 * Tests verify that exported HTML files render correctly in real browsers.
 */
export default defineConfig({
  testDir: './e2e',
  // Explicitly ignore test files outside e2e/ that use Playwright imports
  // but are meant to be run separately or are legacy tests
  // Note: accessibility tests previously ignored are now enabled (T4.5)
  testIgnore: ['**/html_export/**', '**/performance/**'],
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: [
    ['list'],
    ['json', { outputFile: 'e2e-results.json' }],
    ['html', { outputFolder: 'e2e-report', open: 'never' }],
    ['./e2e/reporters/jsonl-reporter.ts'],
  ],

  timeout: 60000,
  expect: {
    timeout: 10000,
  },

  use: {
    baseURL: 'file://',
    trace: 'on-first-retry',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
    // Use domcontentloaded for faster file:// URL navigation
    navigationTimeout: 30000,
    actionTimeout: 10000,
  },

  projects: [
    // Desktop browsers
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'firefox',
      use: { ...devices['Desktop Firefox'] },
    },
    {
      name: 'webkit',
      use: { ...devices['Desktop Safari'] },
    },
    // Mobile devices - iOS (various screen sizes)
    {
      name: 'iphone-12',
      use: { ...devices['iPhone 12'] },
    },
    {
      name: 'iphone-13',
      use: { ...devices['iPhone 13'] },
    },
    {
      name: 'iphone-14',
      use: { ...devices['iPhone 14'] },
    },
    // Mobile devices - Android
    {
      name: 'pixel-5',
      use: { ...devices['Pixel 5'] },
    },
    {
      name: 'pixel-7',
      use: { ...devices['Pixel 7'] },
    },
    {
      name: 'galaxy-s9',
      use: { ...devices['Galaxy S9+'] },
    },
    // Low-end Android (320px width for edge case testing)
    {
      name: 'low-end-android',
      use: {
        viewport: { width: 320, height: 568 },
        userAgent: 'Mozilla/5.0 (Linux; Android 8.0; SM-G930F) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/80.0.3987.149 Mobile Safari/537.36',
        deviceScaleFactor: 2,
        isMobile: true,
        hasTouch: true,
      },
    },
    // Mobile-specific test projects (for targeted mobile testing)
    {
      name: 'mobile-chrome',
      use: { ...devices['Pixel 5'] },
      testMatch: /mobile\/.*.spec.ts/,
    },
    {
      name: 'mobile-safari',
      use: { ...devices['iPhone 12'] },
      testMatch: /mobile\/.*.spec.ts/,
    },
  ],

  globalSetup: path.join(__dirname, 'e2e/setup/global-setup.ts'),
  globalTeardown: path.join(__dirname, 'e2e/setup/global-teardown.ts'),
});
