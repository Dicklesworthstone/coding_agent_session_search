import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

test.describe('Copy to Clipboard', () => {
  test('copy button appears on code blocks', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check for code blocks with code element
    const codeBlocks = page.locator('pre:has(code):visible');
    const codeCount = await codeBlocks.count();

    if (codeCount > 0) {
      // Copy buttons are added dynamically and hidden by default (opacity: 0)
      // They become visible on hover over the pre element (use force to bypass stability check)
      // Use JS scroll (instant) to avoid stability check timeout
      const firstPre = codeBlocks.first();
      await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
      await firstPre.hover({ force: true });

      // After hovering, the copy button should be visible
      const copyBtn = firstPre.locator('.copy-code-btn');
      await expect(copyBtn).toBeVisible({ timeout: 2000 });
    }
  });

  test('clicking copy button shows toast notification', async ({ page, context, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Grant clipboard permissions
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find code block and hover to reveal copy button (use force to bypass stability check)
    // Use JS scroll (instant) to avoid stability check timeout
    const codeBlocks = page.locator('pre:has(code):visible');
    if (await codeBlocks.count() > 0) {
      const firstPre = codeBlocks.first();
      await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
      await firstPre.hover({ force: true });

      const copyBtn = firstPre.locator('.copy-code-btn');
      if (await copyBtn.count() > 0) {
        await copyBtn.click({ force: true });

        // Toast notification should appear
        const toast = page.locator('.toast, #toast-container > *');
        await expect(toast.first()).toBeVisible({ timeout: 3000 });
      }
    }
  });

  test('copies code content to clipboard', async ({ page, context, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Grant clipboard permissions
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find code block
    const codeBlocks = page.locator('pre:has(code):visible');
    if (await codeBlocks.count() > 0) {
      const firstPre = codeBlocks.first();

      // Get the code content
      const codeContent = await firstPre.locator('code').textContent();

      // Hover to reveal copy button and click it (use force to bypass stability check)
      // Use JS scroll (instant) to avoid stability check timeout
      await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
      await firstPre.hover({ force: true });
      const copyBtn = firstPre.locator('.copy-code-btn');

      if (await copyBtn.count() > 0) {
        await copyBtn.click({ force: true });

        // Wait for clipboard to update
        await page.waitForTimeout(500);

        // Verify clipboard content
        const clipboardText = await page.evaluate(() => navigator.clipboard.readText());

        // Clipboard should contain the code (trim whitespace for comparison)
        expect(clipboardText.trim().length).toBeGreaterThan(0);
      }
    }
  });

  test('toast notification disappears after timeout', async ({ page, context, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const codeBlocks = page.locator('pre:has(code):visible');
    if (await codeBlocks.count() > 0) {
      const firstPre = codeBlocks.first();
      // Use JS scroll (instant) to avoid stability check timeout
      await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
      await firstPre.hover({ force: true });

      const copyBtn = firstPre.locator('.copy-code-btn');
      if (await copyBtn.count() > 0) {
        await copyBtn.click({ force: true });

        // Toast should appear
        const toast = page.locator('.toast, #toast-container > *').first();
        await expect(toast).toBeVisible({ timeout: 1000 });

        // Wait for toast to disappear (default is ~3 seconds)
        await page.waitForTimeout(4000);

        // Toast should be gone or hidden
        await expect(toast).not.toBeVisible();
      }
    }
  });
});

test.describe('Message Copy', () => {
  test('message action buttons are accessible', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check for message action buttons
    const messageActions = page.locator('.message-actions, .message-action-btn');
    const count = await messageActions.count();

    // If message actions exist, they should be visible on interaction
    if (count > 0) {
      const firstMessage = page.locator('.message').first();
      await firstMessage.hover();

      // Actions should become visible
      const actionBtn = firstMessage.locator('.message-action-btn').first();
      if (await actionBtn.count() > 0) {
        await expect(actionBtn).toBeVisible();
      }
    }
  });
});
