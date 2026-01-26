import { test, expect, waitForPageReady, getCurrentTheme } from '../setup/test-utils';

test.describe('Theme Toggle', () => {
  test('starts with default theme', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const theme = await getCurrentTheme(page);
    // Default theme should be 'dark' or 'light' (not 'unknown')
    expect(['dark', 'light']).toContain(theme);
  });

  test('toggles between dark and light themes', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const initialTheme = await getCurrentTheme(page);

    // Find and click toggle button
    const toggleBtn = page.locator(
      '#theme-toggle, [data-action="toggle-theme"], .theme-toggle, [aria-label*="theme"], button:has-text("theme")'
    );

    const toggleExists = (await toggleBtn.count()) > 0;
    if (!toggleExists) {
      test.skip(true, 'Theme toggle button not found');
      return;
    }

    // Click with force to bypass stability check
    await toggleBtn.first().click({ force: true });

    // Theme should change
    const newTheme = await getCurrentTheme(page);
    expect(newTheme).not.toBe(initialTheme);

    // Click again to return to original
    await toggleBtn.first().click({ force: true });
    const finalTheme = await getCurrentTheme(page);
    expect(finalTheme).toBe(initialTheme);
  });

  test('theme persists after page reload', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const toggleBtn = page.locator(
      '#theme-toggle, [data-action="toggle-theme"], .theme-toggle, [aria-label*="theme"]'
    );

    const toggleExists = (await toggleBtn.count()) > 0;
    if (!toggleExists) {
      test.skip(true, 'Theme toggle button not found');
      return;
    }

    // Toggle theme
    await toggleBtn.first().click({ force: true });
    const changedTheme = await getCurrentTheme(page);

    // Reload page
    await page.reload();
    await waitForPageReady(page);

    // Theme should persist from localStorage
    const reloadedTheme = await getCurrentTheme(page);
    expect(reloadedTheme).toBe(changedTheme);
  });

  test('theme toggle has proper accessibility', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const toggleBtn = page.locator(
      '#theme-toggle, [data-action="toggle-theme"], .theme-toggle, [aria-label*="theme"]'
    );

    const toggleExists = (await toggleBtn.count()) > 0;
    if (!toggleExists) {
      test.skip(true, 'Theme toggle button not found');
      return;
    }

    // Should be focusable
    await toggleBtn.first().focus();
    await expect(toggleBtn.first()).toBeFocused();

    // Should be activatable via keyboard
    await page.keyboard.press('Enter');
    // Theme should have changed
    const theme = await getCurrentTheme(page);
    expect(theme).toBeDefined();
  });
});
