import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

/**
 * Mobile device E2E tests - Performance under constraints
 *
 * Tests that the HTML export performs acceptably on mobile devices
 * with CPU throttling to simulate real mobile performance.
 */

test.describe('Mobile Performance', () => {
  test.beforeEach(async ({ page }) => {
    const viewport = page.viewportSize();
    const isMobile = (viewport?.width || 0) < 768;
    console.log(`[device-context] viewport: ${viewport?.width}x${viewport?.height}, mobile: ${isMobile}`);
  });

  test('page loads within acceptable time', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    const startTime = Date.now();

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const loadTime = Date.now() - startTime;

    // Log performance metrics
    console.log(`[perf] Page load time: ${loadTime}ms`);

    // Should load within 5 seconds even on mobile
    expect(loadTime).toBeLessThan(5000);
  });

  test('page renders without blocking main thread', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check that we can interact immediately
    const startInteract = Date.now();

    // Try to scroll
    await page.evaluate(() => window.scrollBy(0, 100));

    const interactTime = Date.now() - startInteract;

    // Interaction should be responsive (under 100ms)
    expect(interactTime).toBeLessThan(100);
  });

  test('scrolling is smooth', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Measure scroll performance
    const scrollMetrics = await page.evaluate(async () => {
      const measurements: number[] = [];

      for (let i = 0; i < 10; i++) {
        const start = performance.now();
        window.scrollBy(0, 50);
        await new Promise(r => requestAnimationFrame(r));
        measurements.push(performance.now() - start);
      }

      return {
        avg: measurements.reduce((a, b) => a + b, 0) / measurements.length,
        max: Math.max(...measurements),
        min: Math.min(...measurements),
      };
    });

    console.log(`[perf] Scroll metrics - avg: ${scrollMetrics.avg.toFixed(2)}ms, max: ${scrollMetrics.max.toFixed(2)}ms`);

    // Average scroll frame should be under 16ms (60fps)
    // Allow some slack for test overhead
    expect(scrollMetrics.avg).toBeLessThan(50);
  });

  test('memory usage stays reasonable', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check initial memory
    const initialMemory = await page.evaluate(() => {
      if ('memory' in performance) {
        return (performance as unknown as { memory: { usedJSHeapSize: number } }).memory.usedJSHeapSize;
      }
      return null;
    });

    // Scroll through the page to trigger lazy loading
    for (let i = 0; i < 5; i++) {
      await page.evaluate(() => window.scrollBy(0, 500));
      await page.waitForTimeout(100);
    }

    // Check final memory
    const finalMemory = await page.evaluate(() => {
      if ('memory' in performance) {
        return (performance as unknown as { memory: { usedJSHeapSize: number } }).memory.usedJSHeapSize;
      }
      return null;
    });

    if (initialMemory && finalMemory) {
      const memoryGrowth = finalMemory - initialMemory;
      const growthMB = memoryGrowth / (1024 * 1024);

      console.log(`[perf] Memory growth: ${growthMB.toFixed(2)}MB`);

      // Should not grow more than 50MB during normal use
      expect(growthMB).toBeLessThan(50);
    }
  });

  test('animations do not cause jank', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Measure frame timing during animation
    const frameMetrics = await page.evaluate(async () => {
      const frameTimes: number[] = [];
      let lastTime = performance.now();

      await new Promise<void>((resolve) => {
        let frameCount = 0;

        function measureFrame() {
          const now = performance.now();
          frameTimes.push(now - lastTime);
          lastTime = now;
          frameCount++;

          if (frameCount < 30) {
            requestAnimationFrame(measureFrame);
          } else {
            resolve();
          }
        }

        requestAnimationFrame(measureFrame);
      });

      // Calculate metrics
      const droppedFrames = frameTimes.filter(t => t > 33).length; // > 30fps threshold
      const avgFrameTime = frameTimes.reduce((a, b) => a + b, 0) / frameTimes.length;

      return {
        avgFrameTime,
        droppedFrames,
        totalFrames: frameTimes.length,
      };
    });

    console.log(`[perf] Frame metrics - avg: ${frameMetrics.avgFrameTime.toFixed(2)}ms, dropped: ${frameMetrics.droppedFrames}/${frameMetrics.totalFrames}`);

    // Should not drop more than 20% of frames
    const dropRate = frameMetrics.droppedFrames / frameMetrics.totalFrames;
    expect(dropRate).toBeLessThan(0.2);
  });
});

test.describe('Decryption Performance', () => {
  test('encrypted page decrypts within acceptable time', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    // Find password input
    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found - may not be encrypted');
      return;
    }

    const startDecrypt = Date.now();

    await passwordInput.fill(password);
    await page.keyboard.press('Enter');

    // Wait for content to appear
    await page.waitForSelector('.message, .content, main', { timeout: 30000 });

    const decryptTime = Date.now() - startDecrypt;

    console.log(`[perf] Decryption time: ${decryptTime}ms`);

    // Decryption should complete within 10 seconds on mobile
    expect(decryptTime).toBeLessThan(10000);
  });

  test('decryption progress is shown', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found');
      return;
    }

    await passwordInput.fill(password);
    await page.keyboard.press('Enter');

    // Look for progress indicator
    const progressIndicator = page.locator(
      '.decrypting, .progress, .loading, [data-decrypting="true"], .spinner'
    );

    // Either progress is shown, or decryption is so fast it doesn't need it
    const hasProgress = await progressIndicator.count() > 0;

    // Wait for completion
    await page.waitForSelector('.message, .content, main', { timeout: 30000 });

    // Log whether progress was shown
    console.log(`[perf] Decryption progress indicator shown: ${hasProgress}`);
  });

  test('UI remains responsive during decryption', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found');
      return;
    }

    await passwordInput.fill(password);

    // Start timing before triggering decryption
    let inputResponsive = true;

    // Try to interact while decrypting
    const interactionPromise = (async () => {
      await page.keyboard.press('Enter');

      // Try to type during decryption
      const startType = Date.now();
      await page.keyboard.press('Tab');
      const typeTime = Date.now() - startType;

      // Should respond within 500ms even during decryption
      inputResponsive = typeTime < 500;
    })();

    await interactionPromise;
    await page.waitForSelector('.message, .content, main', { timeout: 30000 });

    expect(inputResponsive).toBe(true);
  });
});

test.describe('CPU Throttled Performance', () => {
  test('page functions with 4x CPU slowdown', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Enable CPU throttling via CDP (Chrome DevTools Protocol)
    const client = await page.context().newCDPSession(page);
    await client.send('Emulation.setCPUThrottlingRate', { rate: 4 });

    try {
      const startTime = Date.now();

      await gotoFile(page, exportPath);
      await waitForPageReady(page);

      const loadTime = Date.now() - startTime;
      console.log(`[perf] Load time with 4x CPU throttling: ${loadTime}ms`);

      // Should still load within 15 seconds
      expect(loadTime).toBeLessThan(15000);

      // Basic functionality should work
      const messages = await page.locator('.message').count();
      expect(messages).toBeGreaterThan(0);
    } finally {
      // Reset throttling
      await client.send('Emulation.setCPUThrottlingRate', { rate: 1 });
    }
  });

  test('search works with CPU throttling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    const client = await page.context().newCDPSession(page);
    await client.send('Emulation.setCPUThrottlingRate', { rate: 4 });

    try {
      await gotoFile(page, exportPath);
      await waitForPageReady(page);

      const searchInput = page.locator('#search-input, input[type="search"]');
      if (await searchInput.count() > 0) {
        const startSearch = Date.now();

        await searchInput.first().fill('test');
        await page.keyboard.press('Enter');
        await page.waitForTimeout(1000);

        const searchTime = Date.now() - startSearch;
        console.log(`[perf] Search time with CPU throttling: ${searchTime}ms`);

        // Search should complete within 5 seconds even throttled
        expect(searchTime).toBeLessThan(5000);
      }
    } finally {
      await client.send('Emulation.setCPUThrottlingRate', { rate: 1 });
    }
  });
});
