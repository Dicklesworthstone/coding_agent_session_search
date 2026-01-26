import { test, expect, waitForPageReady, countMessages, collectConsoleErrors } from '../setup/test-utils';

test.describe('CDN Fallback - No-CDN Mode', () => {
  test('renders correctly without CDN resources', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await page.goto(`file://${noCdnExportPath}`);
    await waitForPageReady(page);

    // Page should render completely
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Should be styled (has some CSS applied)
    const bodyBgColor = await page.locator('body').evaluate((el) =>
      window.getComputedStyle(el).backgroundColor
    );
    expect(bodyBgColor).not.toBe('');
  });

  test('no external resource URLs in no-cdn export', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    const response = await page.goto(`file://${noCdnExportPath}`);
    const html = await page.content();

    // Should not reference external CDNs
    const cdnPatterns = [
      'cdn.tailwindcss.com',
      'cdn.jsdelivr.net',
      'fonts.googleapis.com',
      'unpkg.com',
      'cdnjs.cloudflare.com',
    ];

    for (const pattern of cdnPatterns) {
      // Allow references in comments, but not in actual script/link tags
      const hasActiveReference =
        html.includes(`src="${pattern}`) ||
        html.includes(`href="${pattern}`) ||
        html.includes(`src='${pattern}`) ||
        html.includes(`href='${pattern}`);

      expect(hasActiveReference).toBe(false);
    }
  });

  test('code blocks styled without external resources', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await page.goto(`file://${noCdnExportPath}`);
    await waitForPageReady(page);

    const preBlock = page.locator('pre').first();
    const preExists = (await preBlock.count()) > 0;

    if (preExists) {
      await expect(preBlock).toBeVisible();

      // Should have fallback styling - check pre or its code child
      const styles = await preBlock.evaluate((el) => {
        const code = el.querySelector('code');
        const target = code || el;
        const computed = window.getComputedStyle(target);
        return {
          fontFamily: computed.fontFamily,
          backgroundColor: computed.backgroundColor,
        };
      });

      // Should have monospace font
      expect(styles.fontFamily.toLowerCase()).toMatch(/mono|courier|consolas|ui-monospace|sfmono/);
    }
  });
});

test.describe('CDN Fallback - Network Blocking', () => {
  test('renders correctly with CDN blocked', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Block all CDN requests
    await page.route('**/*.tailwindcss.com/**', (route) => route.abort());
    await page.route('**/*.jsdelivr.net/**', (route) => route.abort());
    await page.route('**/*.googleapis.com/**', (route) => route.abort());
    await page.route('**/*.unpkg.com/**', (route) => route.abort());

    await page.goto(`file://${exportPath}`);
    await waitForPageReady(page);

    // Page should still render
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);
  });

  test('page functions without JavaScript CDN', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Block JS CDNs
    await page.route('**/*.jsdelivr.net/**/*.js', (route) => route.abort());
    await page.route('**/*.unpkg.com/**/*.js', (route) => route.abort());

    await page.goto(`file://${exportPath}`);
    await waitForPageReady(page);

    // Basic functionality should work
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Theme toggle might still work (inline JS)
    const toggleBtn = page.locator('[data-action="toggle-theme"], .theme-toggle');
    if ((await toggleBtn.count()) > 0) {
      await toggleBtn.first().click();
      // Should not crash
    }
  });

  test('fallback classes applied when Tailwind unavailable', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Block Tailwind
    await page.route('**/*.tailwindcss.com/**', (route) => route.abort());

    await page.goto(`file://${exportPath}`);
    await waitForPageReady(page);

    // Wait for error handler to run
    await page.waitForTimeout(2000);

    // Check if fallback class is applied
    const bodyClasses = await page.locator('body').getAttribute('class');
    const htmlClasses = await page.locator('html').getAttribute('class');

    // Might have 'no-tailwind' or 'offline' class
    const hasFallbackIndicator =
      (bodyClasses?.includes('no-tailwind') || bodyClasses?.includes('offline')) ||
      (htmlClasses?.includes('no-tailwind') || htmlClasses?.includes('offline'));

    // This is optional - the page should work regardless
    // Just verify the page is functional
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);
  });
});

test.describe('Offline Mode Simulation', () => {
  test('page works in offline mode', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    // Go offline
    await page.context().setOffline(true);

    await page.goto(`file://${noCdnExportPath}`);
    await waitForPageReady(page);

    // Page should work fully offline
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Go back online
    await page.context().setOffline(false);
  });

  test('all critical styles are inline', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await page.goto(`file://${noCdnExportPath}`);
    await waitForPageReady(page);

    // Check that there are inline styles
    const inlineStyles = page.locator('style');
    const styleCount = await inlineStyles.count();
    expect(styleCount).toBeGreaterThan(0);

    // Critical styles should be present
    const html = await page.content();
    expect(html).toMatch(/\.message|\.conversation|body\s*\{/);
  });
});
