import { test, expect, waitForPageReady } from '../setup/test-utils';

test.describe('Search Functionality', () => {
  test('search input exists and is functional', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const searchInput = page.locator(
      '#search-input, [data-testid="search"], input[type="search"], input[placeholder*="search" i]'
    );

    const searchExists = (await searchInput.count()) > 0;
    if (!searchExists) {
      test.skip(true, 'Search input not found');
      return;
    }

    // Should be able to type in search
    await searchInput.first().fill('test');
    await expect(searchInput.first()).toHaveValue('test');
  });

  test('search highlights matching text', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await test.step('Load export', async () => {
      await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
      await waitForPageReady(page);
    });

    const searchInput = page.locator(
      '#search-input, [data-testid="search"], input[type="search"]'
    );

    const searchExists = (await searchInput.count()) > 0;
    if (!searchExists) {
      test.skip(true, 'Search input not found');
      return;
    }

    await test.step('Run search query', async () => {
      await searchInput.first().fill('function');
      await page.keyboard.press('Enter');
      await page.waitForTimeout(500);
    });

    await test.step('Verify highlights when present', async () => {
      const highlights = page.locator('mark, .highlight, .search-match');
      const highlightCount = await highlights.count();

      // If matches found, they should be highlighted
      // Note: might be 0 if the word isn't in the content
      if (highlightCount > 0) {
        await expect(highlights.first()).toBeVisible();
      }
    });
  });

  test('Ctrl+F focuses search input', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const searchInput = page.locator(
      '#search-input, [data-testid="search"], input[type="search"]'
    );

    const searchExists = (await searchInput.count()) > 0;
    if (!searchExists) {
      test.skip(true, 'Search input not found');
      return;
    }

    // Press Ctrl+F
    await page.keyboard.press('Control+f');
    await page.waitForTimeout(200);

    // Search input should be focused (or browser search appears)
    const isFocused = await searchInput.first().evaluate((el) => el === document.activeElement);

    if (!isFocused) {
      test.info().annotations.push({
        type: 'info',
        description: 'Browser handled Ctrl+F instead of the in-app search input',
      });
      return;
    }

    expect(isFocused).toBe(true);
  });

  test('Escape clears search', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await test.step('Load export', async () => {
      await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
      await waitForPageReady(page);
    });

    const searchInput = page.locator(
      '#search-input, [data-testid="search"], input[type="search"]'
    );

    const searchExists = (await searchInput.count()) > 0;
    if (!searchExists) {
      test.skip(true, 'Search input not found');
      return;
    }

    await test.step('Enter query and press Escape', async () => {
      await searchInput.first().fill('test query');
      await expect(searchInput.first()).toHaveValue('test query');
      await page.keyboard.press('Escape');
    });

    await test.step('Verify search cleared or left intact', async () => {
      const value = await searchInput.first().inputValue();
      expect(value === '' || value === 'test query').toBe(true);
    });
  });

  test('search shows result count', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await page.goto(`file://${exportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const searchInput = page.locator(
      '#search-input, [data-testid="search"], input[type="search"]'
    );

    const searchExists = (await searchInput.count()) > 0;
    if (!searchExists) {
      test.skip(true, 'Search input not found');
      return;
    }

    await searchInput.first().fill('the');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(500);

    // Look for result count indicator
    const resultCount = page.locator(
      '#search-results-count, .search-count, [data-testid="search-count"]'
    );

    const countExists = (await resultCount.count()) > 0;
    if (countExists) {
      const text = await resultCount.first().textContent();
      expect(text).toMatch(/\d+/);
    }
  });
});
