import { test, expect, collectConsoleErrors, waitForPageReady, countMessages } from '../setup/test-utils';

test.describe('Basic HTML Rendering', () => {
  test('renders complete HTML document structure', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Document structure
    await expect(page.locator('html')).toBeAttached();
    await expect(page.locator('head')).toBeAttached();
    await expect(page.locator('body')).toBeAttached();

    // Essential elements present - use more specific selector for main header
    await expect(page.locator('header[role="banner"], .conversation-header, [data-testid="header"]').first()).toBeVisible();
    await expect(page.locator('main, .conversation, [data-testid="conversation"]').first()).toBeVisible();
  });

  test('page loads without JavaScript errors', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    const errors = await collectConsoleErrors(page);
    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Filter out expected warnings (like CDN failures in offline mode or MIME type issues)
    const criticalErrors = errors.filter(
      (err) =>
        !err.includes('net::ERR') &&
        !err.includes('Failed to load resource') &&
        !err.includes('MIME type') &&
        !err.includes('Refused to apply style')
    );

    expect(criticalErrors).toHaveLength(0);
  });

  test('displays messages in correct order', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const messages = page.locator('.message');
    const count = await messages.count();
    expect(count).toBeGreaterThan(0);

    // Verify order by checking data-idx attributes if present
    const indices = await messages.evaluateAll((els) =>
      els
        .map((el) => el.getAttribute('data-idx'))
        .filter((idx) => idx !== null)
        .map((idx) => parseInt(idx!, 10))
    );

    if (indices.length > 0) {
      const sorted = [...indices].sort((a, b) => a - b);
      expect(indices).toEqual(sorted);
    }
  });

  test('renders code blocks with syntax highlighting', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Wait a bit for Prism.js to process
    await page.waitForTimeout(1000);

    const codeBlocks = page.locator('pre code');
    const count = await codeBlocks.count();

    if (count > 0) {
      // Check that code blocks have some styling (either Prism or fallback)
      const hasHighlighting = await page.evaluate(() => {
        const code = document.querySelector('pre code');
        if (!code) return false;

        // Prism adds token spans
        const hasTokens = code.querySelectorAll('.token').length > 0;
        // Or we have language class
        const hasLanguageClass = code.className.includes('language-');
        // Or fallback background
        const hasBgColor =
          window.getComputedStyle(code).backgroundColor !== 'rgba(0, 0, 0, 0)';

        return hasTokens || hasLanguageClass || hasBgColor;
      });

      expect(hasHighlighting).toBe(true);
    }
  });

  test('displays timestamps in valid format', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const timestamps = page.locator('time[datetime], .timestamp, [data-timestamp]');
    const count = await timestamps.count();

    if (count > 0) {
      // Verify at least one timestamp is visible
      await expect(timestamps.first()).toBeVisible();
    }
  });

  test('renders all message roles correctly', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Check for user messages
    const userMessages = page.locator('.message-user, [data-role="user"]');
    const userCount = await userMessages.count();

    // Check for agent/assistant messages
    const agentMessages = page.locator('.message-agent, .message-assistant, [data-role="assistant"]');
    const agentCount = await agentMessages.count();

    // At least some messages should exist
    expect(userCount + agentCount).toBeGreaterThan(0);
  });
});

test.describe('Large Session Rendering', () => {
  test('renders large session without timeout', async ({ page, largeExportPath }) => {
    test.skip(!largeExportPath, 'Large export path not available');

    const start = Date.now();
    await page.goto(`file://${largeExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);
    const elapsed = Date.now() - start;

    // Should load within reasonable time (30 seconds)
    expect(elapsed).toBeLessThan(30000);

    // Should have many messages
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(100);
  });

  test('page remains responsive with large content', async ({ page, largeExportPath }) => {
    test.skip(!largeExportPath, 'Large export path not available');

    await page.goto(`file://${largeExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Test that we can interact with the page
    const messages = page.locator('.message');
    const messageCount = await messages.count();
    expect(messageCount).toBeGreaterThan(100);

    // Page should have scrollable content (body height > viewport)
    const pageHeight = await page.evaluate(() => document.body.scrollHeight);
    const viewportHeight = await page.evaluate(() => window.innerHeight);

    // Large content should make page scrollable
    expect(pageHeight).toBeGreaterThan(viewportHeight);
  });
});

test.describe('Unicode Content Rendering', () => {
  test('renders unicode content correctly', async ({ page, unicodeExportPath }) => {
    test.skip(!unicodeExportPath, 'Unicode export path not available');

    await page.goto(`file://${unicodeExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Page should load without errors
    const errors = await collectConsoleErrors(page);
    const criticalErrors = errors.filter((err) => !err.includes('net::ERR'));
    expect(criticalErrors).toHaveLength(0);

    // Content should be visible
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);
  });

  test('emoji and special characters display properly', async ({ page, unicodeExportPath }) => {
    test.skip(!unicodeExportPath, 'Unicode export path not available');

    await page.goto(`file://${unicodeExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Check that unicode content is present in the page
    const pageContent = await page.content();

    // Should contain some unicode characters (not escaped)
    const hasUnicode =
      pageContent.includes('日本語') ||
      pageContent.includes('中文') ||
      pageContent.includes('🎉') ||
      pageContent.includes('مرحبا') ||
      /[\u4e00-\u9fff]/.test(pageContent) || // CJK
      /[\u{1F300}-\u{1F9FF}]/u.test(pageContent); // Emoji

    expect(hasUnicode).toBe(true);
  });
});
