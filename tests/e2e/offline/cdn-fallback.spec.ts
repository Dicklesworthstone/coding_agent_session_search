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
    await waitForPageReady(page);

    const cdnPatterns = [
      'cdn.tailwindcss.com',
      'cdn.jsdelivr.net',
      'fonts.googleapis.com',
      'unpkg.com',
      'cdnjs.cloudflare.com',
    ];
    const activeExternalReferences = await page.evaluate((blockedHosts) => {
      return Array.from(
        document.querySelectorAll(
          'script[src], link[href], img[src], iframe[src], source[src], audio[src], video[src]'
        )
      ).flatMap((element) => {
        const attribute = element.hasAttribute('href') ? 'href' : 'src';
        const rawValue = element.getAttribute(attribute);
        if (!rawValue) {
          return [];
        }
        try {
          const url = new URL(rawValue, document.baseURI);
          const hostname = url.hostname.toLowerCase();
          return blockedHosts.some(
            (host) => hostname === host || hostname.endsWith(`.${host}`)
          )
            ? [{ tag: element.tagName.toLowerCase(), attribute, hostname }]
            : [];
        } catch {
          return [{ tag: element.tagName.toLowerCase(), attribute, hostname: 'invalid-url' }];
        }
      });
    }, cdnPatterns);

    expect(activeExternalReferences).toEqual([]);
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
      const failedRequests: Array<{
        scheme: string;
        hostname: string;
        resourceType: string;
      }> = [];
      page.on('requestfailed', (request) => {
        try {
          const url = new URL(request.url());
          failedRequests.push({
            scheme: url.protocol.replace(/:$/, ''),
            hostname: url.hostname,
            resourceType: request.resourceType(),
          });
        } catch {
          failedRequests.push({
            scheme: 'invalid',
            hostname: 'invalid-url',
            resourceType: request.resourceType(),
          });
        }
      });

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
      expect(failedRequests.some(({ hostname }) => hostname.includes('cdn.'))).toBe(true);

      const firstMessage = page.locator('.message').first();
      await expect(firstMessage).toBeVisible();
      const firstContent = firstMessage.locator('.message-content').first();
      await expect(firstContent).toBeVisible();
      const legibility = await firstContent.evaluate((element) => {
        type Rgba = [number, number, number, number];
        const canvas = document.createElement('canvas');
        canvas.width = 1;
        canvas.height = 1;
        const context = canvas.getContext('2d', { willReadFrequently: true });
        if (!context) {
          throw new Error('2D canvas unavailable for computed contrast check');
        }
        const cssColorToRgba = (color: string): Rgba => {
          context.clearRect(0, 0, 1, 1);
          context.fillStyle = color;
          context.fillRect(0, 0, 1, 1);
          const pixel = context.getImageData(0, 0, 1, 1).data;
          return [pixel[0], pixel[1], pixel[2], pixel[3] / 255];
        };
        const composite = (foreground: Rgba, background: Rgba): Rgba => {
          const alpha = foreground[3] + background[3] * (1 - foreground[3]);
          if (alpha === 0) {
            return [0, 0, 0, 0];
          }
          return [
            (foreground[0] * foreground[3] +
              background[0] * background[3] * (1 - foreground[3])) /
              alpha,
            (foreground[1] * foreground[3] +
              background[1] * background[3] * (1 - foreground[3])) /
              alpha,
            (foreground[2] * foreground[3] +
              background[2] * background[3] * (1 - foreground[3])) /
              alpha,
            alpha,
          ];
        };
        const ancestors: Element[] = [];
        for (let current: Element | null = element; current; current = current.parentElement) {
          ancestors.push(current);
        }
        let background: Rgba = [255, 255, 255, 1];
        for (const ancestor of ancestors.reverse()) {
          const layer = cssColorToRgba(window.getComputedStyle(ancestor).backgroundColor);
          background = composite(layer, background);
        }
        const style = window.getComputedStyle(element);
        const foreground = composite(cssColorToRgba(style.color), background);
        const luminance = (color: Rgba): number => {
          const linear = color.slice(0, 3).map((channel) => {
            const normalized = channel / 255;
            return normalized <= 0.04045
              ? normalized / 12.92
              : ((normalized + 0.055) / 1.055) ** 2.4;
          });
          return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
        };
        const foregroundLuminance = luminance(foreground);
        const backgroundLuminance = luminance(background);
        const contrastRatio =
          (Math.max(foregroundLuminance, backgroundLuminance) + 0.05) /
          (Math.min(foregroundLuminance, backgroundLuminance) + 0.05);
        const rect = element.getBoundingClientRect();
        return {
          contrastRatio,
          foreground: foreground.slice(0, 3).map(Math.round),
          background: background.slice(0, 3).map(Math.round),
          fontFamily: style.fontFamily,
          rect: { width: rect.width, height: rect.height },
          textLength: element.textContent?.trim().length ?? 0,
        };
      });
      expect(legibility.rect.width).toBeGreaterThan(0);
      expect(legibility.rect.height).toBeGreaterThan(0);
      expect(legibility.textLength).toBeGreaterThan(0);
      expect(legibility.contrastRatio).toBeGreaterThanOrEqual(4.5);
      expect(legibility.fontFamily).not.toBe('');

      const firstCodeBlock = page.locator('pre').first();
      let codeBlock: { present: boolean; fontFamily?: string } = { present: false };
      if ((await firstCodeBlock.count()) > 0) {
        await expect(firstCodeBlock).toBeVisible();
        const fontFamily = await firstCodeBlock.evaluate(
          (element) => window.getComputedStyle(element).fontFamily
        );
        expect(fontFamily.toLowerCase()).toMatch(
          /mono|courier|consolas|ui-monospace|sfmono/
        );
        codeBlock = { present: true, fontFamily };
      }

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
              codeBlock,
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
