import {
  test,
  expect,
  gotoFile,
  waitForPageReady,
  countMessages,
  collectBrowserErrors,
} from '../setup/test-utils';

test.describe('CDN Fallback - No-CDN Mode', () => {
  test('renders correctly without CDN resources', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');
    const browserErrors = collectBrowserErrors(page);

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    // Page should render completely
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Should be styled (has some CSS applied)
    const bodyBgColor = await page.locator('body').evaluate((el) =>
      window.getComputedStyle(el).backgroundColor
    );
    expect(bodyBgColor).not.toBe('');
    await page.waitForTimeout(500);
    expect(browserErrors.pageErrors).toEqual([]);
    expect(browserErrors.consoleErrors).toEqual([]);
  });

  test('no external resource URLs in no-cdn export', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await gotoFile(page, noCdnExportPath);
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

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    const preBlock = page.locator('pre').first();
    const preExists = (await preBlock.count()) > 0;

    if (preExists) {
      await preBlock.scrollIntoViewIfNeeded();
      await expect(preBlock).toBeAttached();

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

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
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

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Basic functionality should work
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Theme toggle might still work (inline JS)
    const toggleBtn = page.locator('#theme-toggle, [data-action="toggle-theme"], .theme-toggle');
    if ((await toggleBtn.count()) > 0) {
      // Use JS scroll (instant) to avoid stability check timeout
      await toggleBtn.first().evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
      await toggleBtn.first().click({ force: true });
      // Should not crash
    }
  });

  test(
    'fallback classes and legible content survive CDN failure',
    async ({ page, exportPath }, testInfo) => {
      test.skip(!exportPath, 'Export path not available');

      const browserErrors = collectBrowserErrors(page);
      const failedRequests: string[] = [];
      page.on('requestfailed', (request) => failedRequests.push(request.url()));

      // Block both current jsDelivr assets and the legacy Tailwind host before
      // navigation so stylesheet/script onerror handlers exercise real fallback.
      await page.route('https://cdn.jsdelivr.net/**', (route) => route.abort());
      await page.route('https://cdn.tailwindcss.com/**', (route) => route.abort());

      await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
      await waitForPageReady(page);

      // Wait for error handlers to run.
      await page.waitForTimeout(2000);

      const bodyClasses = await page.locator('body').getAttribute('class');
      const htmlClasses = await page.locator('html').getAttribute('class');

      // A failed CDN must become an explicit fallback state, not a silent style
      // dependency. Prism-only failures are also acceptable for older exports.
      const hasFallbackIndicator =
        bodyClasses?.includes('no-tailwind') ||
        bodyClasses?.includes('no-prism') ||
        bodyClasses?.includes('offline') ||
        htmlClasses?.includes('no-tailwind') ||
        htmlClasses?.includes('no-prism') ||
        htmlClasses?.includes('offline');

      const messageCount = await countMessages(page);
      expect(messageCount).toBeGreaterThan(0);
      expect(hasFallbackIndicator).toBe(true);
      expect(browserErrors.pageErrors).toEqual([]);
      expect(failedRequests.some((url) => url.includes('cdn.'))).toBe(true);

      const legibility = await page.locator('body').evaluate((element) => {
        const style = window.getComputedStyle(element);
        return {
          color: style.color,
          backgroundColor: style.backgroundColor,
          fontFamily: style.fontFamily,
        };
      });
      expect(legibility.color).not.toBe('');
      expect(legibility.backgroundColor).not.toBe('');
      expect(legibility.fontFamily).not.toBe('');

      await testInfo.attach('cdn-degradation-diagnostics', {
        body: Buffer.from(
          JSON.stringify(
            {
              failedRequests,
              consoleErrors: browserErrors.consoleErrors,
              pageErrors: browserErrors.pageErrors,
              bodyClasses,
              htmlClasses,
              legibility,
            },
            null,
            2
          )
        ),
        contentType: 'application/json',
      });
    }
  );
});

test.describe('Offline Mode Simulation', () => {
  test('page works in offline mode', async ({ page, noCdnExportPath, browserName }) => {
    // WebKit skip must be FIRST - setOffline fails immediately on WebKit with file:// URLs
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable with file:// URLs');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    // Go offline
    await page.context().setOffline(true);

    await page.goto(`file://${noCdnExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Page should work fully offline
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Go back online
    await page.context().setOffline(false);
  });

  test('all critical styles are inline', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await page.goto(`file://${noCdnExportPath}`, { waitUntil: 'domcontentloaded' });
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
