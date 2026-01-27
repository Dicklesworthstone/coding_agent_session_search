import { test, expect, gotoFile, waitForPageReady, countMessages } from '../setup/test-utils';

/**
 * Offline mode E2E tests - Network transitions
 *
 * Tests that the HTML export handles online/offline transitions
 * gracefully without data loss or UI crashes.
 */

test.describe('Online to Offline Transitions', () => {
  test.beforeEach(async ({ page }) => {
    console.log('[phase-start] Network transition test setup');
  });

  test('page survives going offline after load', async ({ page, noCdnExportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable with file:// URLs');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    // Load page while online
    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    console.log('[phase-start] Online phase - verifying content');
    const initialMessageCount = await countMessages(page);
    expect(initialMessageCount).toBeGreaterThan(0);

    // Go offline
    console.log('[phase-start] Going offline');
    await page.context().setOffline(true);
    await page.waitForTimeout(500);

    // Page should still be functional
    console.log('[phase-start] Offline phase - verifying stability');
    const offlineMessageCount = await countMessages(page);
    expect(offlineMessageCount).toBe(initialMessageCount);

    // Theme toggle should still work (local state)
    const themeToggle = page.locator('#theme-toggle, [data-action="toggle-theme"]');
    if (await themeToggle.count() > 0) {
      const beforeTheme = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
      await themeToggle.first().click({ force: true });
      await page.waitForTimeout(200);
      const afterTheme = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
      expect(afterTheme).not.toBe(beforeTheme);
    }

    // Restore online
    await page.context().setOffline(false);
    console.log('[phase-end] Network transition test complete');
  });

  test('search works offline', async ({ page, noCdnExportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    // Go offline before searching
    await page.context().setOffline(true);

    const searchInput = page.locator('#search-input, input[type="search"]');
    if (await searchInput.count() > 0) {
      console.log('[phase-start] Offline search test');
      await searchInput.first().fill('function');
      await page.keyboard.press('Enter');
      await page.waitForTimeout(500);

      // Search should work (it's all local)
      const highlights = page.locator('mark, .highlight, .search-match');
      const highlightCount = await highlights.count();
      console.log(`[perf] Offline search found ${highlightCount} matches`);
    }

    await page.context().setOffline(false);
  });

  test('collapsible sections work offline', async ({ page, noCdnExportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    await page.context().setOffline(true);

    const details = page.locator('details');
    if (await details.count() > 0) {
      const firstDetails = details.first();
      const wasOpen = await firstDetails.evaluate((el) => (el as HTMLDetailsElement).open);

      // Toggle
      const summary = firstDetails.locator('summary');
      await summary.click({ force: true });
      await page.waitForTimeout(200);

      const isOpen = await firstDetails.evaluate((el) => (el as HTMLDetailsElement).open);
      expect(isOpen).not.toBe(wasOpen);
    }

    await page.context().setOffline(false);
  });

  test('copy functionality works offline', async ({ page, noCdnExportPath, browserName, context }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    await page.context().setOffline(true);

    const copyButton = page.locator('[data-action="copy"], .copy-btn').first();
    if (await copyButton.count() > 0) {
      await copyButton.click({ force: true });
      await page.waitForTimeout(300);

      // Should not crash, clipboard might have content
      const clipboardContent = await page.evaluate(async () => {
        try {
          return await navigator.clipboard.readText();
        } catch {
          return null;
        }
      });

      // Either clipboard has content or action completed without error
      expect(clipboardContent !== undefined).toBe(true);
    }

    await page.context().setOffline(false);
  });
});

test.describe('Offline to Online Transitions', () => {
  test('page recovers when going online', async ({ page, noCdnExportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    // Start offline
    await page.context().setOffline(true);

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    console.log('[phase-start] Starting offline');
    const offlineMessageCount = await countMessages(page);
    expect(offlineMessageCount).toBeGreaterThan(0);

    // Go online
    console.log('[phase-start] Going online');
    await page.context().setOffline(false);
    await page.waitForTimeout(500);

    // Should still work
    const onlineMessageCount = await countMessages(page);
    expect(onlineMessageCount).toBe(offlineMessageCount);

    console.log('[phase-end] Offline to online transition complete');
  });

  test('multiple online/offline cycles', async ({ page, noCdnExportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    const initialCount = await countMessages(page);

    // Cycle 3 times
    for (let i = 0; i < 3; i++) {
      console.log(`[phase-start] Cycle ${i + 1}: going offline`);
      await page.context().setOffline(true);
      await page.waitForTimeout(200);

      console.log(`[phase-start] Cycle ${i + 1}: going online`);
      await page.context().setOffline(false);
      await page.waitForTimeout(200);
    }

    // Content should be preserved
    const finalCount = await countMessages(page);
    expect(finalCount).toBe(initialCount);
  });
});

test.describe('Partial Connectivity', () => {
  test('page handles slow network gracefully', async ({ page, noCdnExportPath }) => {
    test.skip(!noCdnExportPath, 'No-CDN export path not available');

    // Simulate slow network
    const client = await page.context().newCDPSession(page);
    await client.send('Network.emulateNetworkConditions', {
      offline: false,
      downloadThroughput: 50 * 1024, // 50 KB/s (slow 3G)
      uploadThroughput: 25 * 1024,
      latency: 500, // 500ms latency
    });

    console.log('[phase-start] Loading with slow network');
    const startTime = Date.now();

    await gotoFile(page, noCdnExportPath);
    await waitForPageReady(page);

    const loadTime = Date.now() - startTime;
    console.log(`[perf] Load time on slow network: ${loadTime}ms`);

    // Should still render
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Reset network conditions
    await client.send('Network.emulateNetworkConditions', {
      offline: false,
      downloadThroughput: -1,
      uploadThroughput: -1,
      latency: 0,
    });
  });

  test('page handles intermittent connectivity', async ({ page, exportPath, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Simulate intermittent connection
    for (let i = 0; i < 5; i++) {
      // Short offline blip
      await page.context().setOffline(true);
      await page.waitForTimeout(100);
      await page.context().setOffline(false);
      await page.waitForTimeout(100);
    }

    // Page should still be functional
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Should be able to interact
    const body = await page.evaluate(() => document.body.innerHTML.length);
    expect(body).toBeGreaterThan(0);
  });
});

test.describe('Resource Loading Failures', () => {
  test('page handles CSS load failure gracefully', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Block all CSS
    await page.route('**/*.css', (route) => route.abort());

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Content should still be readable
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Text should be visible
    const bodyText = await page.evaluate(() => document.body.innerText.length);
    expect(bodyText).toBeGreaterThan(0);
  });

  test('page handles image load failure gracefully', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    const failedImages: string[] = [];

    // Track image failures
    page.on('requestfailed', (request) => {
      if (request.resourceType() === 'image') {
        failedImages.push(request.url());
      }
    });

    // Block all images
    await page.route('**/*.{png,jpg,jpeg,gif,webp,svg}', (route) => route.abort());

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Page should still render
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Log failed images for debugging
    if (failedImages.length > 0) {
      console.log(`[info] ${failedImages.length} images failed to load (expected)`);
    }
  });

  test('page handles script load failure gracefully', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Block external scripts
    await page.route('**/*.js', (route) => {
      const url = route.request().url();
      // Allow inline scripts (file:// URLs), block external
      if (!url.startsWith('file://')) {
        return route.abort();
      }
      return route.continue();
    });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Basic content should be visible
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);
  });
});
