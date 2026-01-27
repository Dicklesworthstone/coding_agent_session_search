import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

/**
 * Mobile device E2E tests - Responsive layout verification
 *
 * Tests that the HTML export renders correctly across different
 * mobile viewport sizes and orientations.
 */

test.describe('Responsive Layout', () => {
  test.beforeEach(async ({ page }) => {
    // Log device info
    const viewport = page.viewportSize();
    console.log(`[device-context] Testing viewport: ${viewport?.width}x${viewport?.height}`);
  });

  test('content fits within viewport width', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const viewport = page.viewportSize();
    if (!viewport) return;

    // Check for horizontal overflow
    const hasHorizontalScroll = await page.evaluate((vw) => {
      return document.body.scrollWidth > vw;
    }, viewport.width);

    // Some horizontal scroll is acceptable for code blocks, but not excessive
    const scrollWidth = await page.evaluate(() => document.body.scrollWidth);
    const maxAcceptableWidth = viewport.width * 1.1; // Allow 10% overflow for code blocks

    expect(scrollWidth).toBeLessThanOrEqual(maxAcceptableWidth);
  });

  test('text is readable without horizontal scrolling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const viewport = page.viewportSize();
    if (!viewport) return;

    // Check that main text content doesn't overflow
    const textOverflows = await page.evaluate((vw) => {
      const textElements = document.querySelectorAll('p, .message-content, .content');
      for (const el of textElements) {
        const rect = el.getBoundingClientRect();
        if (rect.width > vw) {
          return true;
        }
      }
      return false;
    }, viewport.width);

    expect(textOverflows).toBe(false);
  });

  test('navigation elements are accessible on small screens', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const viewport = page.viewportSize();
    if (!viewport) return;

    // Check that important elements are visible and tappable
    const importantElements = [
      '#theme-toggle, [data-action="toggle-theme"]',
      '#search-input, input[type="search"]',
      'header, .header, nav',
    ];

    for (const selector of importantElements) {
      const element = page.locator(selector).first();
      if (await element.count() > 0) {
        const isVisible = await element.isVisible();
        if (isVisible) {
          const box = await element.boundingBox();
          if (box) {
            // Element should be within viewport
            expect(box.x).toBeGreaterThanOrEqual(0);
            expect(box.x + box.width).toBeLessThanOrEqual(viewport.width + 10);

            // Tap target should be at least 44x44 pixels (WCAG mobile guideline)
            const effectiveSize = Math.max(box.width, box.height);
            // Log if below recommended size
            if (effectiveSize < 44) {
              console.log(`[a11y-warning] Element ${selector} tap target is ${effectiveSize}px (recommended: 44px)`);
            }
          }
        }
      }
    }
  });

  test('font size is readable on mobile', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check body text font size
    const fontSizes = await page.evaluate(() => {
      const results: { selector: string; size: number }[] = [];

      // Check various text elements
      const selectors = ['body', 'p', '.message-content', '.content', 'pre code'];
      for (const selector of selectors) {
        const el = document.querySelector(selector);
        if (el) {
          const style = window.getComputedStyle(el);
          const size = parseFloat(style.fontSize);
          results.push({ selector, size });
        }
      }
      return results;
    });

    // Body text should be at least 14px on mobile for readability
    for (const { selector, size } of fontSizes) {
      if (selector === 'body' || selector === 'p') {
        expect(size).toBeGreaterThanOrEqual(14);
      }
    }
  });

  test('touch targets are adequately sized', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find all interactive elements
    const interactiveElements = page.locator('button, a, input, [role="button"]');
    const count = await interactiveElements.count();

    const smallTargets: string[] = [];

    for (let i = 0; i < Math.min(count, 20); i++) {
      const element = interactiveElements.nth(i);
      const box = await element.boundingBox();
      if (box) {
        const minDimension = Math.min(box.width, box.height);
        // WCAG 2.5.8 recommends 44x44 minimum
        if (minDimension < 44) {
          const text = await element.textContent();
          smallTargets.push(`${text?.slice(0, 20) || 'unnamed'} (${box.width}x${box.height})`);
        }
      }
    }

    // Log warnings but don't fail - some small targets are acceptable
    if (smallTargets.length > 0) {
      console.log(`[a11y-info] Small touch targets found: ${smallTargets.join(', ')}`);
    }

    // Should not have majority of targets below minimum
    expect(smallTargets.length).toBeLessThan(count / 2);
  });

  test('images scale appropriately', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const viewport = page.viewportSize();
    if (!viewport) return;

    const images = page.locator('img');
    const imageCount = await images.count();

    for (let i = 0; i < imageCount; i++) {
      const img = images.nth(i);
      const box = await img.boundingBox();
      if (box) {
        // Image should not exceed viewport width (minus padding)
        expect(box.width).toBeLessThanOrEqual(viewport.width);

        // Check if image has proper responsive styling
        const hasResponsiveStyle = await img.evaluate((el) => {
          const style = window.getComputedStyle(el);
          return style.maxWidth === '100%' || style.width === '100%' || el.style.maxWidth === '100%';
        });

        // Log if not responsive
        if (!hasResponsiveStyle && box.width > viewport.width * 0.8) {
          console.log(`[responsive-warning] Image may not be responsive: ${box.width}px`);
        }
      }
    }
  });

  test('code blocks are scrollable, not overflowing', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const viewport = page.viewportSize();
    if (!viewport) return;

    const codeBlocks = page.locator('pre, .code-block');
    const codeCount = await codeBlocks.count();

    for (let i = 0; i < codeCount; i++) {
      const block = codeBlocks.nth(i);
      const box = await block.boundingBox();

      if (box) {
        // Code block container should not exceed viewport
        expect(box.width).toBeLessThanOrEqual(viewport.width);

        // Check for overflow-x: auto/scroll
        const hasScrollableOverflow = await block.evaluate((el) => {
          const style = window.getComputedStyle(el);
          return ['auto', 'scroll'].includes(style.overflowX);
        });

        // Long code should be scrollable
        const contentWidth = await block.evaluate((el) => el.scrollWidth);
        if (contentWidth > viewport.width) {
          expect(hasScrollableOverflow).toBe(true);
        }
      }
    }
  });
});

test.describe('Orientation Changes', () => {
  test('portrait to landscape transition works', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const initialViewport = page.viewportSize();
    if (!initialViewport) return;

    // Capture initial state
    const initialMessageCount = await page.locator('.message').count();

    // Simulate orientation change (swap width and height)
    await page.setViewportSize({
      width: initialViewport.height,
      height: initialViewport.width,
    });

    await page.waitForTimeout(300);

    // Content should still be present
    const newMessageCount = await page.locator('.message').count();
    expect(newMessageCount).toBe(initialMessageCount);

    // Layout should still be valid
    const hasOverflow = await page.evaluate(() => {
      return document.body.scrollWidth > window.innerWidth * 1.1;
    });

    expect(hasOverflow).toBe(false);
  });

  test('landscape to portrait transition works', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const initialViewport = page.viewportSize();
    if (!initialViewport) return;

    // Start in landscape (ensure width > height)
    if (initialViewport.width < initialViewport.height) {
      await page.setViewportSize({
        width: initialViewport.height,
        height: initialViewport.width,
      });
      await page.waitForTimeout(200);
    }

    // Get message count in landscape
    const landscapeMessageCount = await page.locator('.message').count();

    // Switch to portrait
    const currentViewport = page.viewportSize();
    if (currentViewport) {
      await page.setViewportSize({
        width: currentViewport.height,
        height: currentViewport.width,
      });
    }

    await page.waitForTimeout(300);

    // Content should still be present
    const portraitMessageCount = await page.locator('.message').count();
    expect(portraitMessageCount).toBe(landscapeMessageCount);
  });

  test('layout adjusts on viewport resize', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const sizes = [
      { width: 320, height: 568 },  // iPhone SE
      { width: 375, height: 667 },  // iPhone 8
      { width: 390, height: 844 },  // iPhone 14
      { width: 412, height: 915 },  // Pixel 7
    ];

    for (const size of sizes) {
      await page.setViewportSize(size);
      await page.waitForTimeout(200);

      // Check layout is valid
      const contentWidth = await page.evaluate(() => {
        const main = document.querySelector('main') || document.body;
        return main.getBoundingClientRect().width;
      });

      expect(contentWidth).toBeLessThanOrEqual(size.width);
    }
  });
});
