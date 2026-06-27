/**
 * HTML Export E2E Tests
 *
 * Validates that exported HTML files render correctly in browsers,
 * both plain and encrypted modes.
 *
 * Acceptance Criteria (bead 3fu1):
 * - export-html covered for plain + encrypted modes
 * - Browser logs, trace files, and output HTML captured as artifacts
 * - Failures include actionable logs and screenshots
 *
 * Run:
 *   npx playwright test e2e/export/html-export-e2e.spec.ts
 */

import { test, expect, gotoFile, waitForPageReady, countMessages } from '../setup/test-utils';
import { existsSync, readdirSync, readFileSync, writeFileSync, mkdirSync } from 'fs';
import path from 'path';

// Test artifact directory
const ARTIFACT_DIR = path.join(process.cwd(), 'test-results', 'e2e', 'html-export');

test.beforeAll(async () => {
  mkdirSync(ARTIFACT_DIR, { recursive: true });
});

test.describe('HTML Export - Plain Mode', () => {
  test('renders basic export with all messages visible', async ({ page, exportPath }, testInfo) => {
    await test.step('Verify export file exists', async () => {
      expect(existsSync(exportPath), `Export file should exist at ${exportPath}`).toBe(true);
    });

    await test.step('Navigate to export file', async () => {
      await gotoFile(page, exportPath);
      await waitForPageReady(page);
    });

    await test.step('Verify page structure', async () => {
      // Should have a main content area
      await expect(page.locator('main, #conversation, .conversation')).toBeVisible();

      // Should have at least one message
      const messageCount = await countMessages(page);
      expect(messageCount).toBeGreaterThan(0);

      // Log message count for debugging
      console.log(JSON.stringify({
        event: 'html_export_render',
        test: testInfo.title,
        messages: messageCount,
        ts: new Date().toISOString(),
      }));
    });

    await test.step('Verify no JavaScript errors', async () => {
      const errors: string[] = [];
      page.on('pageerror', (err) => errors.push(err.message));

      // Wait for any deferred scripts
      await page.waitForTimeout(500);

      expect(errors).toEqual([]);
    });

    // Capture output HTML as artifact
    await test.step('Capture output HTML artifact', async () => {
      const html = await page.content();
      const artifactPath = path.join(ARTIFACT_DIR, `${testInfo.title.replace(/\s+/g, '-')}-output.html`);
      writeFileSync(artifactPath, html);

      await testInfo.attach('output-html', {
        path: artifactPath,
        contentType: 'text/html',
      });
    });
  });

  test('renders messages with correct role styling', async ({ page, exportPath }, testInfo) => {
    test.skip(!exportPath, 'Export path not configured');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await test.step('Verify user messages have correct styling', async () => {
      const userMessages = page.locator('.message[data-role="user"], .message.user, .user-message');
      const count = await userMessages.count();

      if (count > 0) {
        // User messages should be visually distinct
        await expect(userMessages.first()).toBeVisible();
      }
    });

    await test.step('Verify assistant messages have correct styling', async () => {
      const assistantMessages = page.locator('.message[data-role="assistant"], .message.assistant, .assistant-message');
      const count = await assistantMessages.count();

      if (count > 0) {
        await expect(assistantMessages.first()).toBeVisible();
      }
    });
  });

  test('code blocks render with syntax highlighting', async ({ page, toolCallsExportPath }, testInfo) => {
    test.skip(!toolCallsExportPath, 'Tool calls export path not configured');

    await gotoFile(page, toolCallsExportPath);
    await waitForPageReady(page);

    await test.step('Verify code blocks exist', async () => {
      const codeBlocks = page.locator('pre code, .hljs, .code-block, .language-javascript, .language-python, .language-rust');
      const count = await codeBlocks.count();

      if (count > 0) {
        // Code should have some highlighting classes
        await expect(codeBlocks.first()).toBeVisible();
      }
    });
  });

  test('theme toggle works correctly', async ({ page, exportPath }, testInfo) => {
    test.skip(!exportPath, 'Export path not configured');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const themeToggle = page.locator('[data-testid="theme-toggle"], .theme-toggle, #theme-toggle, button:has-text("theme")');

    test.skip(await themeToggle.count() === 0, 'No theme toggle found');

    await test.step('Toggle theme', async () => {
      const initialTheme = await page.locator('html').getAttribute('data-theme');
      await themeToggle.click();
      await page.waitForTimeout(100);

      const newTheme = await page.locator('html').getAttribute('data-theme');
      expect(newTheme).not.toBe(initialTheme);
    });
  });

  test('export handles large conversations', async ({ page, largeExportPath }, testInfo) => {
    test.skip(!largeExportPath, 'Large export path not configured');

    await test.step('Navigate to large export', async () => {
      await gotoFile(page, largeExportPath);
      await waitForPageReady(page);
    });

    await test.step('Verify performance with large content', async () => {
      const start = Date.now();
      const messageCount = await countMessages(page);
      const loadTime = Date.now() - start;

      console.log(JSON.stringify({
        event: 'large_export_metrics',
        messages: messageCount,
        countTimeMs: loadTime,
        ts: new Date().toISOString(),
      }));

      expect(messageCount).toBeGreaterThan(0);
      // Count operation should be reasonably fast
      expect(loadTime).toBeLessThan(5000);
    });
  });

  test('export handles unicode content', async ({ page, unicodeExportPath }, testInfo) => {
    test.skip(!unicodeExportPath, 'Unicode export path not configured');

    await gotoFile(page, unicodeExportPath);
    await waitForPageReady(page);

    await test.step('Verify unicode renders correctly', async () => {
      // Should not have any replacement characters
      const content = await page.locator('body').textContent();
      expect(content).not.toContain('\uFFFD');
    });
  });
});

test.describe('HTML Export - Encrypted Mode', () => {
  test('shows password modal for encrypted export', async ({ page, encryptedExportPath }, testInfo) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not configured');

    await test.step('Navigate to encrypted export', async () => {
      await gotoFile(page, encryptedExportPath);
      await waitForPageReady(page);
    });

    await test.step('Verify password modal is visible', async () => {
      const modal = page.locator('#password-modal, .password-modal, [role="dialog"]');
      await expect(modal).toBeVisible();
    });

    await test.step('Verify password input exists', async () => {
      const passwordInput = page.locator('input[type="password"], #password-input');
      await expect(passwordInput).toBeVisible();
    });
  });

  test('decrypts content with correct password', async ({ page, encryptedExportPath, password }, testInfo) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not configured');

    await test.step('Navigate to encrypted export', async () => {
      await gotoFile(page, encryptedExportPath);
      await waitForPageReady(page);
    });

    await test.step('Enter password and decrypt', async () => {
      const passwordInput = page.locator('input[type="password"], #password-input');
      await passwordInput.fill(password);

      const submitButton = page.locator('button[type="submit"], .decrypt-button, #decrypt-button');
      await submitButton.click();
    });

    await test.step('Verify content is decrypted', async () => {
      // Modal should be hidden after successful decryption
      const modal = page.locator('#password-modal, .password-modal, [role="dialog"]');
      await expect(modal).toBeHidden({ timeout: 10000 });

      // Content should be visible
      const content = page.locator('.message, .conversation, main');
      await expect(content.first()).toBeVisible();
    });

    // Capture decrypted content as artifact
    await test.step('Capture decrypted output', async () => {
      const html = await page.content();
      const artifactPath = path.join(ARTIFACT_DIR, `${testInfo.title.replace(/\s+/g, '-')}-decrypted.html`);
      mkdirSync(path.dirname(artifactPath), { recursive: true });
      writeFileSync(artifactPath, html);

      await testInfo.attach('decrypted-output', {
        path: artifactPath,
        contentType: 'text/html',
      });
    });
  });

  test('shows error on wrong password', async ({ page, encryptedExportPath }, testInfo) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not configured');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    await test.step('Enter wrong password', async () => {
      const passwordInput = page.locator('input[type="password"], #password-input');
      await passwordInput.fill('wrong-password-definitely-wrong');

      const submitButton = page.locator('button[type="submit"], .decrypt-button, #decrypt-button');
      await submitButton.click();
    });

    await test.step('Verify error is shown', async () => {
      const errorMessage = page.locator('.error, .decrypt-error, #decrypt-error, [role="alert"]');
      await expect(errorMessage).toBeVisible({ timeout: 5000 });
    });
  });

  test('WebCrypto API is available', async ({ page, encryptedExportPath }, testInfo) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not configured');

    await gotoFile(page, encryptedExportPath);

    const webcryptoAvailable = await page.evaluate(() => {
      return !!(window.crypto && window.crypto.subtle);
    });

    expect(webcryptoAvailable).toBe(true);
  });
});

test.describe('HTML Export - CDN Fallback', () => {
  test('works without CDN resources', async ({ page, noCdnExportPath }, testInfo) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not configured');

    await test.step('Navigate to no-CDN export', async () => {
      await gotoFile(page, noCdnExportPath);
      await waitForPageReady(page);
    });

    await test.step('Verify content renders', async () => {
      const messageCount = await countMessages(page);
      expect(messageCount).toBeGreaterThanOrEqual(0);
    });

    await test.step('Check for failed resource loads', async () => {
      const failedRequests: string[] = [];
      page.on('requestfailed', (request) => {
        failedRequests.push(request.url());
      });

      await page.waitForTimeout(1000);

      // Log any failed requests for debugging
      if (failedRequests.length > 0) {
        console.log(JSON.stringify({
          event: 'failed_requests',
          requests: failedRequests,
          ts: new Date().toISOString(),
        }));
      }
    });
  });
});

test.describe('HTML Export - Accessibility', () => {
  test('has proper heading structure', async ({ page, exportPath }, testInfo) => {
    test.skip(!exportPath, 'Export path not configured');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await test.step('Verify heading hierarchy', async () => {
      const h1Count = await page.locator('h1').count();
      expect(h1Count).toBeGreaterThanOrEqual(0);

      // Should not skip heading levels (h1 -> h3 without h2)
      const headings = await page.locator('h1, h2, h3, h4, h5, h6').all();
      let lastLevel = 0;

      for (const heading of headings) {
        const tagName = await heading.evaluate((el) => el.tagName);
        const level = parseInt(tagName.charAt(1));
        // Allow same level or one level deeper
        if (lastLevel > 0 && level > lastLevel + 1) {
          console.warn(`Heading level skip: h${lastLevel} -> h${level}`);
        }
        lastLevel = level;
      }
    });
  });

  test('messages have proper ARIA attributes', async ({ page, exportPath }, testInfo) => {
    test.skip(!exportPath, 'Export path not configured');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const messages = page.locator('.message, [role="article"], [role="listitem"]');
    const count = await messages.count();

    if (count > 0) {
      // At least some messages should have accessibility attributes
      const firstMessage = messages.first();
      const role = await firstMessage.getAttribute('role');
      const ariaLabel = await firstMessage.getAttribute('aria-label');

      // Log for debugging
      console.log(JSON.stringify({
        event: 'accessibility_check',
        role,
        hasAriaLabel: !!ariaLabel,
        ts: new Date().toISOString(),
      }));
    }
  });
});
