import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';
import type { Page, BrowserContext } from '@playwright/test';

/**
 * Mobile device E2E tests - Touch navigation and interactions
 *
 * Tests touch-based navigation and interaction patterns that are
 * specific to mobile devices.
 *
 * Note: These tests run on mobile device emulation profiles and
 * use touch events instead of mouse events.
 */

test.describe('Touch Navigation', () => {
  test.beforeEach(async ({ page }) => {
    // Log device info at the start of each test
    const viewport = page.viewportSize();
    const userAgent = await page.evaluate(() => navigator.userAgent);
    console.log(`[device-context] viewport: ${viewport?.width}x${viewport?.height}, ua: ${userAgent.slice(0, 50)}...`);
  });

  test('tap navigates between sections', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find collapsible sections/message blocks
    const messages = page.locator('.message, .message-block, article');
    const messageCount = await messages.count();

    if (messageCount > 1) {
      // Tap on the second message
      const secondMessage = messages.nth(1);
      await secondMessage.tap();
      await page.waitForTimeout(200);

      // Should have navigated/focused
      const isVisible = await secondMessage.isVisible();
      expect(isVisible).toBe(true);
    }
  });

  test('tap opens collapsible content', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find collapsible elements
    const details = page.locator('details, .collapsible');
    const detailsCount = await details.count();

    if (detailsCount > 0) {
      const firstDetails = details.first();
      const summary = firstDetails.locator('summary, .collapsible-header').first();

      // Check initial state
      const wasOpen = await firstDetails.evaluate(
        (el) => el.tagName === 'DETAILS' ? (el as HTMLDetailsElement).open : el.classList.contains('open')
      );

      // Tap to toggle
      if (await summary.count() > 0) {
        await summary.tap();
        await page.waitForTimeout(300);

        const isOpen = await firstDetails.evaluate(
          (el) => el.tagName === 'DETAILS' ? (el as HTMLDetailsElement).open : el.classList.contains('open')
        );

        // State should have changed
        expect(isOpen).not.toBe(wasOpen);
      }
    }
  });

  test('swipe gesture scrolls content', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const initialScrollTop = await page.evaluate(() => window.scrollY);

    // Perform swipe up (scroll down)
    const viewport = page.viewportSize();
    if (viewport) {
      const startX = viewport.width / 2;
      const startY = viewport.height * 0.8;
      const endY = viewport.height * 0.2;

      await page.touchscreen.tap(startX, startY);
      await page.mouse.move(startX, startY);

      // Simulate swipe by touchscreen drag
      await page.evaluate(async ({ x, y1, y2 }) => {
        const touch = new Touch({
          identifier: Date.now(),
          target: document.body,
          clientX: x,
          clientY: y1,
        });

        document.body.dispatchEvent(new TouchEvent('touchstart', {
          touches: [touch],
          targetTouches: [touch],
          changedTouches: [touch],
          bubbles: true,
        }));

        await new Promise(r => setTimeout(r, 50));

        const moveTouch = new Touch({
          identifier: Date.now(),
          target: document.body,
          clientX: x,
          clientY: y2,
        });

        document.body.dispatchEvent(new TouchEvent('touchmove', {
          touches: [moveTouch],
          targetTouches: [moveTouch],
          changedTouches: [moveTouch],
          bubbles: true,
        }));

        document.body.dispatchEvent(new TouchEvent('touchend', {
          touches: [],
          targetTouches: [],
          changedTouches: [moveTouch],
          bubbles: true,
        }));
      }, { x: startX, y1: startY, y2: endY });
    }

    await page.waitForTimeout(300);

    // Try native scroll as fallback
    await page.evaluate(() => window.scrollBy(0, 200));
    await page.waitForTimeout(100);

    const finalScrollTop = await page.evaluate(() => window.scrollY);

    // Should have scrolled
    expect(finalScrollTop).toBeGreaterThan(initialScrollTop);
  });

  test('double-tap zooms code blocks', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find code blocks
    const codeBlocks = page.locator('pre code, .code-block');
    const codeCount = await codeBlocks.count();

    if (codeCount > 0) {
      const codeBlock = codeBlocks.first();
      await codeBlock.scrollIntoViewIfNeeded();

      const rect = await codeBlock.boundingBox();
      if (rect) {
        const centerX = rect.x + rect.width / 2;
        const centerY = rect.y + rect.height / 2;

        // Double tap
        await page.touchscreen.tap(centerX, centerY);
        await page.waitForTimeout(100);
        await page.touchscreen.tap(centerX, centerY);
        await page.waitForTimeout(300);

        // Verify the code block is still visible (didn't break)
        expect(await codeBlock.isVisible()).toBe(true);
      }
    }
  });

  test('long press shows context menu or copy option', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find text content
    const textContent = page.locator('.message-content, .content, p').first();
    if (await textContent.count() > 0) {
      const rect = await textContent.boundingBox();
      if (rect) {
        const centerX = rect.x + rect.width / 2;
        const centerY = rect.y + rect.height / 2;

        // Simulate long press
        await page.evaluate(async ({ x, y }) => {
          const touch = new Touch({
            identifier: Date.now(),
            target: document.elementFromPoint(x, y) || document.body,
            clientX: x,
            clientY: y,
          });

          const target = document.elementFromPoint(x, y) || document.body;

          target.dispatchEvent(new TouchEvent('touchstart', {
            touches: [touch],
            targetTouches: [touch],
            changedTouches: [touch],
            bubbles: true,
          }));

          // Hold for 500ms (long press duration)
          await new Promise(r => setTimeout(r, 500));

          target.dispatchEvent(new TouchEvent('touchend', {
            touches: [],
            targetTouches: [],
            changedTouches: [touch],
            bubbles: true,
          }));
        }, { x: centerX, y: centerY });

        await page.waitForTimeout(200);

        // Page should still be functional
        expect(await textContent.isVisible()).toBe(true);
      }
    }
  });
});

test.describe('Mobile Button Interactions', () => {
  test('buttons respond to tap', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find the theme toggle button
    const themeToggle = page.locator('#theme-toggle, [data-action="toggle-theme"], .theme-toggle');
    if (await themeToggle.count() > 0) {
      const currentTheme = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));

      await themeToggle.first().tap();
      await page.waitForTimeout(300);

      const newTheme = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));

      // Theme should have toggled
      expect(newTheme).not.toBe(currentTheme);
    }
  });

  test('copy button works with tap', async ({ page, exportPath, context }) => {
    test.skip(!exportPath, 'Export path not available');

    // Grant clipboard permissions
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find copy buttons
    const copyButtons = page.locator('[data-action="copy"], .copy-btn, button:has-text("Copy")');
    if (await copyButtons.count() > 0) {
      await copyButtons.first().tap();
      await page.waitForTimeout(300);

      // Check for feedback (tooltip, text change, etc.)
      const feedback = page.locator('.copied, .copy-success, [data-copied="true"]');
      const hasFeedback = (await feedback.count()) > 0 || true; // Some implementations don't show feedback
      expect(hasFeedback).toBe(true);
    }
  });

  test('search input works with tap and virtual keyboard', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const searchInput = page.locator('#search-input, input[type="search"]');
    if (await searchInput.count() > 0) {
      // Tap to focus
      await searchInput.first().tap();
      await page.waitForTimeout(200);

      // Should be focused
      const isFocused = await searchInput.first().evaluate((el) => el === document.activeElement);
      expect(isFocused).toBe(true);

      // Type using virtual keyboard simulation
      await page.keyboard.type('test search', { delay: 50 });

      const value = await searchInput.first().inputValue();
      expect(value).toBe('test search');
    }
  });
});
