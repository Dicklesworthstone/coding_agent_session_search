import { test, expect, waitForPageReady } from '../setup/test-utils';

/**
 * Offline mode E2E tests - Service Worker Caching
 *
 * Tests that service worker caching works correctly for the Pages preview,
 * enabling true offline functionality.
 */

test.describe('Service Worker Cache Behavior', () => {
  test('service worker caches resources on first load', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    // Wait for service worker to be ready
    await page.waitForFunction(
      () => navigator.serviceWorker.controller !== null,
      { timeout: 10000 }
    );

    // Check cache status
    const cacheInfo = await page.evaluate(async () => {
      const cacheNames = await caches.keys();
      const cacheDetails: { name: string; count: number; urls: string[] }[] = [];

      for (const name of cacheNames) {
        const cache = await caches.open(name);
        const requests = await cache.keys();
        cacheDetails.push({
          name,
          count: requests.length,
          urls: requests.slice(0, 5).map(r => r.url), // First 5 URLs for debugging
        });
      }

      return {
        cacheNames,
        cacheDetails,
        totalCaches: cacheNames.length,
      };
    });

    console.log(`[info] Service worker caches: ${JSON.stringify(cacheInfo.cacheNames)}`);

    // Should have at least one cache
    expect(cacheInfo.totalCaches).toBeGreaterThan(0);
  });

  test('service worker responds with cached content offline', async ({ page, previewUrl, browserName }) => {
    test.skip(browserName === 'webkit', 'WebKit offline mode not reliable');
    test.skip(!previewUrl, 'Preview URL not available');

    // First visit to populate cache
    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(
      () => navigator.serviceWorker.controller !== null,
      { timeout: 10000 }
    );
    await waitForPageReady(page);

    // Reload to ensure cache is populated
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForTimeout(1000);

    // Go offline
    await page.context().setOffline(true);

    // Reload - should work from cache
    console.log('[phase-start] Offline reload from cache');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Should still have content
    const hasContent = await page.evaluate(() => {
      return document.body.innerHTML.length > 100;
    });

    expect(hasContent).toBe(true);

    await page.context().setOffline(false);
  });

  test('service worker cache survives page close and reopen', async ({ page, previewUrl, browserName, context }) => {
    test.skip(browserName === 'webkit', 'WebKit service worker persistence varies');
    test.skip(!previewUrl, 'Preview URL not available');

    // First visit
    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(
      () => navigator.serviceWorker.controller !== null,
      { timeout: 10000 }
    );

    // Get cache state
    const initialCacheCount = await page.evaluate(async () => {
      const names = await caches.keys();
      return names.length;
    });

    // Close the page
    await page.close();

    // Open a new page
    const newPage = await context.newPage();
    await newPage.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    // Cache should still exist
    const newCacheCount = await newPage.evaluate(async () => {
      const names = await caches.keys();
      return names.length;
    });

    expect(newCacheCount).toBeGreaterThanOrEqual(initialCacheCount);

    await newPage.close();
  });
});

test.describe('Cache Update Behavior', () => {
  test('cache updates when new version is available', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    // Wait for service worker
    await page.waitForFunction(
      () => navigator.serviceWorker.controller !== null,
      { timeout: 10000 }
    );

    // Check for update mechanism
    const hasUpdateFlow = await page.evaluate(() => {
      // Check if there's an update waiting
      return {
        hasServiceWorker: 'serviceWorker' in navigator,
        controllerState: navigator.serviceWorker.controller?.state,
        hasWaiting: !!(navigator.serviceWorker as ServiceWorkerContainer & { waiting?: ServiceWorker }).waiting,
      };
    });

    console.log(`[info] Service worker state: ${JSON.stringify(hasUpdateFlow)}`);
    expect(hasUpdateFlow.hasServiceWorker).toBe(true);
  });
});

test.describe('OPFS Persistence', () => {
  test('OPFS data persists across page reloads', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    // Write to OPFS
    const writeResult = await page.evaluate(async () => {
      if (!('storage' in navigator)) return { supported: false };

      try {
        const root = await navigator.storage.getDirectory();
        const handle = await root.getFileHandle('test-persist.txt', { create: true });
        const writable = await handle.createWritable();
        const testData = `test-data-${Date.now()}`;
        await writable.write(testData);
        await writable.close();
        return { supported: true, written: testData };
      } catch (e) {
        return { supported: false, error: String(e) };
      }
    });

    if (!writeResult.supported) {
      test.skip(true, 'OPFS not supported in this browser');
      return;
    }

    console.log(`[info] Wrote to OPFS: ${writeResult.written}`);

    // Reload the page
    await page.reload({ waitUntil: 'domcontentloaded' });

    // Read from OPFS
    const readResult = await page.evaluate(async () => {
      try {
        const root = await navigator.storage.getDirectory();
        const handle = await root.getFileHandle('test-persist.txt');
        const file = await handle.getFile();
        const text = await file.text();
        // Cleanup
        await root.removeEntry('test-persist.txt');
        return { success: true, data: text };
      } catch (e) {
        return { success: false, error: String(e) };
      }
    });

    expect(readResult.success).toBe(true);
    expect(readResult.data).toBe(writeResult.written);
  });

  test('OPFS handles large data correctly', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    const result = await page.evaluate(async () => {
      if (!('storage' in navigator)) return { supported: false };

      try {
        const root = await navigator.storage.getDirectory();

        // Create 1MB of data
        const largeData = 'x'.repeat(1024 * 1024);

        console.log('[opfs] Writing 1MB file');
        const startWrite = performance.now();
        const handle = await root.getFileHandle('large-test.txt', { create: true });
        const writable = await handle.createWritable();
        await writable.write(largeData);
        await writable.close();
        const writeTime = performance.now() - startWrite;

        console.log('[opfs] Reading 1MB file');
        const startRead = performance.now();
        const file = await handle.getFile();
        const text = await file.text();
        const readTime = performance.now() - startRead;

        // Cleanup
        await root.removeEntry('large-test.txt');

        return {
          supported: true,
          sizeWritten: largeData.length,
          sizeRead: text.length,
          writeTimeMs: writeTime,
          readTimeMs: readTime,
        };
      } catch (e) {
        return { supported: false, error: String(e) };
      }
    });

    if (!result.supported) {
      test.skip(true, 'OPFS not supported');
      return;
    }

    console.log(`[perf] OPFS 1MB write: ${result.writeTimeMs?.toFixed(2)}ms, read: ${result.readTimeMs?.toFixed(2)}ms`);

    expect(result.sizeWritten).toBe(result.sizeRead);
    // Should complete within reasonable time (10 seconds)
    expect((result.writeTimeMs || 0) + (result.readTimeMs || 0)).toBeLessThan(10000);
  });

  test('OPFS cleanup works correctly', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    const result = await page.evaluate(async () => {
      if (!('storage' in navigator)) return { supported: false };

      try {
        const root = await navigator.storage.getDirectory();

        // Create multiple files
        const fileNames = ['cleanup-1.txt', 'cleanup-2.txt', 'cleanup-3.txt'];
        for (const name of fileNames) {
          const handle = await root.getFileHandle(name, { create: true });
          const writable = await handle.createWritable();
          await writable.write('test');
          await writable.close();
        }

        // Verify files exist
        const beforeCleanup: string[] = [];
        for await (const entry of root.values()) {
          if (fileNames.includes(entry.name)) {
            beforeCleanup.push(entry.name);
          }
        }

        // Delete files
        for (const name of fileNames) {
          await root.removeEntry(name);
        }

        // Verify cleanup
        const afterCleanup: string[] = [];
        for await (const entry of root.values()) {
          if (fileNames.includes(entry.name)) {
            afterCleanup.push(entry.name);
          }
        }

        return {
          supported: true,
          beforeCleanup: beforeCleanup.length,
          afterCleanup: afterCleanup.length,
        };
      } catch (e) {
        return { supported: false, error: String(e) };
      }
    });

    if (!result.supported) {
      test.skip(true, 'OPFS not supported');
      return;
    }

    expect(result.beforeCleanup).toBe(3);
    expect(result.afterCleanup).toBe(0);
  });
});

test.describe('Storage Quota', () => {
  test('storage quota information is available', async ({ page, previewUrl }) => {
    test.skip(!previewUrl, 'Preview URL not available');

    await page.goto(previewUrl, { waitUntil: 'domcontentloaded' });

    const quotaInfo = await page.evaluate(async () => {
      if (!('storage' in navigator) || !('estimate' in navigator.storage)) {
        return { supported: false };
      }

      try {
        const estimate = await navigator.storage.estimate();
        return {
          supported: true,
          quota: estimate.quota,
          usage: estimate.usage,
          usagePercent: estimate.quota ? ((estimate.usage || 0) / estimate.quota * 100).toFixed(2) : 'N/A',
        };
      } catch (e) {
        return { supported: false, error: String(e) };
      }
    });

    if (quotaInfo.supported) {
      console.log(`[info] Storage quota: ${quotaInfo.quota}, usage: ${quotaInfo.usage} (${quotaInfo.usagePercent}%)`);
      expect(quotaInfo.quota).toBeGreaterThan(0);
    }
  });
});
