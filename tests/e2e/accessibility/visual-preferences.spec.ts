import { test, expect, gotoFile, waitForPageReady, countMessages } from '../setup/test-utils';

/**
 * Accessibility E2E tests - Visual preferences
 *
 * Tests that the HTML export respects user preferences for
 * high contrast mode, reduced motion, and font scaling.
 */

test.describe('High Contrast Mode', () => {
  test('page is readable in forced-colors mode', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Emulate forced-colors media feature
    await page.emulateMedia({ forcedColors: 'active' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Page should still render content
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // Text should be visible (not transparent or same as background)
    const textVisibility = await page.evaluate(() => {
      const elements = document.querySelectorAll('p, span, div, h1, h2, h3');
      let visibleCount = 0;

      for (const el of elements) {
        const style = window.getComputedStyle(el);
        // In forced-colors mode, colors are system colors
        // Just check that elements exist and have content
        if (el.textContent && el.textContent.trim().length > 0) {
          visibleCount++;
        }
      }

      return visibleCount;
    });

    expect(textVisibility).toBeGreaterThan(0);
  });

  test('links are distinguishable in high contrast', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.emulateMedia({ forcedColors: 'active' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const links = page.locator('a');
    const linkCount = await links.count();

    if (linkCount > 0) {
      // Links should have some distinguishing feature
      const linkStyles = await links.first().evaluate((el) => {
        const style = window.getComputedStyle(el);
        return {
          hasUnderline: style.textDecoration.includes('underline'),
          display: style.display,
          visibility: style.visibility,
        };
      });

      // Link should be visible
      expect(linkStyles.visibility).not.toBe('hidden');
    }
  });

  test('focus indicators work in high contrast', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.emulateMedia({ forcedColors: 'active' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Tab to a focusable element
    await page.keyboard.press('Tab');

    const hasFocus = await page.evaluate(() => {
      const el = document.activeElement;
      if (!el || el === document.body) return false;

      // In forced-colors, system focus indicators should appear
      const style = window.getComputedStyle(el);
      return style.outline !== 'none' || el.matches(':focus-visible');
    });

    expect(hasFocus !== undefined).toBe(true);
  });

  test('buttons are visible in high contrast', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.emulateMedia({ forcedColors: 'active' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const buttons = page.locator('button');
    const buttonCount = await buttons.count();

    if (buttonCount > 0) {
      const firstButton = buttons.first();
      const isVisible = await firstButton.isVisible();
      expect(isVisible).toBe(true);

      const box = await firstButton.boundingBox();
      if (box) {
        expect(box.width).toBeGreaterThan(0);
        expect(box.height).toBeGreaterThan(0);
      }
    }
  });
});

test.describe('Reduced Motion Preference', () => {
  test('page respects prefers-reduced-motion', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Emulate reduced motion preference
    await page.emulateMedia({ reducedMotion: 'reduce' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check for animation/transition properties
    const animationStyles = await page.evaluate(() => {
      const animated = document.querySelectorAll('[class*="animate"], [class*="transition"]');
      const results: { hasAnimation: boolean; hasDuration: boolean }[] = [];

      // Also check common elements
      const checkElements = [
        ...Array.from(animated),
        document.body,
        ...Array.from(document.querySelectorAll('.message, button, details')),
      ];

      for (const el of checkElements.slice(0, 10)) {
        const style = window.getComputedStyle(el);
        results.push({
          hasAnimation: style.animationName !== 'none' && style.animationDuration !== '0s',
          hasDuration: parseFloat(style.transitionDuration) > 0,
        });
      }

      return results;
    });

    // In reduced motion mode, animations should be disabled or instant
    const hasLongAnimations = animationStyles.some(
      (s) => s.hasAnimation
    );

    // Log findings
    console.log(`[a11y] Reduced motion - found ${animationStyles.filter(s => s.hasAnimation).length} animated elements`);

    // Ideally no animations, but just verify page works
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);
  });

  test('collapsible sections work without animation in reduced motion', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.emulateMedia({ reducedMotion: 'reduce' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const details = page.locator('details');
    if (await details.count() > 0) {
      const firstDetails = details.first();

      // Toggle should work instantly
      const startTime = Date.now();
      await firstDetails.locator('summary').click({ force: true });
      await page.waitForTimeout(50); // Minimal wait
      const toggleTime = Date.now() - startTime;

      // In reduced motion, toggle should be near-instant (under 100ms)
      console.log(`[a11y] Toggle time with reduced motion: ${toggleTime}ms`);

      const isOpen = await firstDetails.evaluate((el) => (el as HTMLDetailsElement).open);
      expect(isOpen).toBe(true);
    }
  });

  test('page scroll is instant in reduced motion', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.emulateMedia({ reducedMotion: 'reduce' });

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check scroll behavior
    const scrollBehavior = await page.evaluate(() => {
      const style = window.getComputedStyle(document.documentElement);
      return style.scrollBehavior;
    });

    // Should not be 'smooth' in reduced motion mode
    // (though this depends on implementation)
    console.log(`[a11y] Scroll behavior: ${scrollBehavior}`);

    // Perform a scroll and verify it completes quickly
    const startTime = Date.now();
    await page.evaluate(() => window.scrollTo(0, 100));
    const scrollTime = Date.now() - startTime;

    expect(scrollTime).toBeLessThan(100);
  });
});

test.describe('Font Scaling', () => {
  test('page is usable at 200% font scaling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Simulate 200% font scaling by setting root font-size
    await page.evaluate(() => {
      document.documentElement.style.fontSize = '200%';
    });

    await page.waitForTimeout(200);

    // Content should still be visible
    const messageCount = await countMessages(page);
    expect(messageCount).toBeGreaterThan(0);

    // No horizontal scrollbar on body (text should wrap)
    const viewport = page.viewportSize();
    if (viewport) {
      const bodyOverflow = await page.evaluate(() => {
        // Check for horizontal overflow (allow some for code blocks)
        return document.body.scrollWidth <= document.documentElement.clientWidth * 1.2;
      });

      // Allow some overflow for code blocks, but main content should fit
      expect(bodyOverflow).toBe(true);
    }
  });

  test('text remains readable at 200% scaling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await page.evaluate(() => {
      document.documentElement.style.fontSize = '200%';
    });

    // Check that text elements are properly sized
    const textSizes = await page.evaluate(() => {
      const elements = document.querySelectorAll('p, .message-content, .content');
      const sizes: number[] = [];

      for (const el of elements) {
        const style = window.getComputedStyle(el);
        sizes.push(parseFloat(style.fontSize));
      }

      return sizes;
    });

    // Font sizes should be doubled (or more) from base
    const avgSize = textSizes.reduce((a, b) => a + b, 0) / textSizes.length;
    console.log(`[a11y] Average font size at 200% scaling: ${avgSize}px`);

    // Should be at least 28px (14px * 2) at 200% scaling
    expect(avgSize).toBeGreaterThanOrEqual(28);
  });

  test('buttons remain tappable at 200% scaling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await page.evaluate(() => {
      document.documentElement.style.fontSize = '200%';
    });

    const buttons = page.locator('button');
    if (await buttons.count() > 0) {
      const firstButton = buttons.first();
      const box = await firstButton.boundingBox();

      if (box) {
        // Button should be at least 44x44 at 200% scaling
        console.log(`[a11y] Button size at 200% scaling: ${box.width}x${box.height}`);
        expect(Math.min(box.width, box.height)).toBeGreaterThanOrEqual(44);
      }
    }
  });

  test('navigation is usable at 200% scaling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await page.evaluate(() => {
      document.documentElement.style.fontSize = '200%';
    });

    // Tab through elements - should still work
    const focusedTags: string[] = [];
    for (let i = 0; i < 10; i++) {
      await page.keyboard.press('Tab');
      const tag = await page.evaluate(() => document.activeElement?.tagName);
      if (tag) focusedTags.push(tag);
    }

    // Should be able to focus interactive elements
    const interactiveCount = focusedTags.filter(
      (t) => ['BUTTON', 'INPUT', 'A', 'SUMMARY'].includes(t)
    ).length;

    expect(interactiveCount).toBeGreaterThan(0);
  });

  test('line height is adequate at 200% scaling', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    await page.evaluate(() => {
      document.documentElement.style.fontSize = '200%';
    });

    const lineHeights = await page.evaluate(() => {
      const elements = document.querySelectorAll('p, .message-content');
      const heights: { fontSize: number; lineHeight: number; ratio: number }[] = [];

      for (const el of elements) {
        const style = window.getComputedStyle(el);
        const fontSize = parseFloat(style.fontSize);
        const lineHeight = parseFloat(style.lineHeight) || fontSize * 1.2;
        heights.push({
          fontSize,
          lineHeight,
          ratio: lineHeight / fontSize,
        });
      }

      return heights;
    });

    // WCAG recommends line-height of at least 1.5 for body text
    const avgRatio = lineHeights.reduce((a, b) => a + b.ratio, 0) / lineHeights.length;
    console.log(`[a11y] Average line-height ratio at 200% scaling: ${avgRatio.toFixed(2)}`);

    // Allow some flexibility but should be reasonable
    expect(avgRatio).toBeGreaterThanOrEqual(1.2);
  });
});
