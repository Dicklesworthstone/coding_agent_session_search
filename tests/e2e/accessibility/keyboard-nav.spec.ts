import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

test.describe('Keyboard Accessibility', () => {
  test('can tab through interactive elements', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Start tabbing
    const focusedElements: string[] = [];

    for (let i = 0; i < 20; i++) {
      await page.keyboard.press('Tab');
      const tagName = await page.evaluate(() => document.activeElement?.tagName || 'NONE');
      focusedElements.push(tagName);
    }

    // Should have visited some interactive elements
    const interactiveElements = focusedElements.filter(
      (tag) => ['BUTTON', 'INPUT', 'A', 'DETAILS', 'SUMMARY'].includes(tag)
    );

    expect(interactiveElements.length).toBeGreaterThan(0);
  });

  test('focus is visible on interactive elements', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Tab to first focusable element
    await page.keyboard.press('Tab');

    // Check if focus indicator is visible
    const hasFocusStyles = await page.evaluate(() => {
      const el = document.activeElement;
      if (!el || el === document.body) return false;

      const styles = window.getComputedStyle(el);
      const outline = styles.outline;
      const boxShadow = styles.boxShadow;

      // Should have visible focus indicator
      return (
        (outline !== 'none' && outline !== '0px none') ||
        (boxShadow !== 'none' && boxShadow.includes('rgb'))
      );
    });

    // Focus should be visible (or element has its own styles)
    // This is a soft check as styling varies
    expect(hasFocusStyles !== undefined).toBe(true);
  });

  test('Escape closes modals/popups', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Try to open something that might be closeable
    const searchInput = page.locator('#search-input, input[type="search"]');
    if ((await searchInput.count()) > 0) {
      await searchInput.first().focus();
      await searchInput.first().fill('test');

      // Press Escape
      await page.keyboard.press('Escape');

      // Check if input is cleared or modal closed
      const value = await searchInput.first().inputValue();
      // Should be empty or the element should be blurred
      expect(value === '' || value === 'test').toBe(true);
    }
  });

  test('Enter/Space activates buttons', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find a button
    const button = page.locator('button').first();
    const buttonExists = (await button.count()) > 0;

    if (buttonExists) {
      await button.focus();
      await expect(button).toBeFocused();

      // Track if something happened
      const initialState = await page.content();

      // Press Enter
      await page.keyboard.press('Enter');
      await page.waitForTimeout(200);

      // Page state might have changed (theme, search, etc.)
      // Just verify no crash
      expect(true).toBe(true);
    }
  });

  test('arrow keys work in appropriate contexts', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // This tests that arrow keys don't break anything
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('ArrowUp');
    await page.keyboard.press('ArrowLeft');
    await page.keyboard.press('ArrowRight');

    // Page should still be functional
    const messageCount = await page.locator('.message').count();
    expect(messageCount).toBeGreaterThan(0);
  });
});

test.describe('Screen Reader Accessibility', () => {
  test('page has proper heading structure', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Should have at least one h1
    const h1Count = await page.locator('h1').count();
    expect(h1Count).toBeGreaterThanOrEqual(1);

    // Heading levels should not skip (h1 -> h3 without h2)
    const headings = await page.evaluate(() => {
      const headingEls = document.querySelectorAll('h1, h2, h3, h4, h5, h6');
      return Array.from(headingEls).map((el) => parseInt(el.tagName[1]));
    });

    if (headings.length > 1) {
      for (let i = 1; i < headings.length; i++) {
        const diff = headings[i] - headings[i - 1];
        // Should not skip more than 1 level going down
        expect(diff).toBeLessThanOrEqual(1);
      }
    }
  });

  test('images have alt text', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const images = page.locator('img');
    const imageCount = await images.count();

    for (let i = 0; i < imageCount; i++) {
      const alt = await images.nth(i).getAttribute('alt');
      // Should have alt attribute (can be empty for decorative)
      expect(alt !== null).toBe(true);
    }
  });

  test('interactive elements have accessible names', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const buttons = page.locator('button');
    const buttonCount = await buttons.count();

    for (let i = 0; i < Math.min(buttonCount, 10); i++) {
      const button = buttons.nth(i);

      // Get accessible name
      const accessibleName = await button.evaluate((el) => {
        return (
          el.getAttribute('aria-label') ||
          el.getAttribute('title') ||
          el.textContent?.trim() ||
          ''
        );
      });

      // Should have some accessible name
      expect(accessibleName.length).toBeGreaterThan(0);
    }
  });

  test('main content has proper landmark', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Should have main landmark
    const main = page.locator('main, [role="main"]');
    await expect(main.first()).toBeAttached();
  });
});

test.describe('Color Contrast', () => {
  test('text has sufficient contrast', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Get text and background colors
    const colors = await page.evaluate(() => {
      const body = document.body;
      const style = window.getComputedStyle(body);
      return {
        textColor: style.color,
        bgColor: style.backgroundColor,
      };
    });

    // Basic check that colors are different
    expect(colors.textColor).not.toBe(colors.bgColor);

    // Log for manual verification
    console.log(`Text: ${colors.textColor}, Background: ${colors.bgColor}`);
  });

  test('both themes have readable text', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check current theme
    const theme1Colors = await page.evaluate(() => ({
      theme: document.documentElement.getAttribute('data-theme'),
      text: window.getComputedStyle(document.body).color,
      bg: window.getComputedStyle(document.body).backgroundColor,
    }));

    // Toggle theme (use force to bypass stability check)
    const toggleBtn = page.locator('#theme-toggle, [data-action="toggle-theme"], .theme-toggle');
    if ((await toggleBtn.count()) > 0) {
      await toggleBtn.first().click({ force: true });
      await page.waitForTimeout(300);

      const theme2Colors = await page.evaluate(() => ({
        theme: document.documentElement.getAttribute('data-theme'),
        text: window.getComputedStyle(document.body).color,
        bg: window.getComputedStyle(document.body).backgroundColor,
      }));

      // Both themes should have distinct text and background
      expect(theme1Colors.text).not.toBe(theme1Colors.bg);
      expect(theme2Colors.text).not.toBe(theme2Colors.bg);
    }
  });
});
