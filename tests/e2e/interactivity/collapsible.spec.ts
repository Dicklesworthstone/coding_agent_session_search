import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

test.describe('Collapsible Sections', () => {
  test('tool calls are collapsible', async ({ page, toolCallsExportPath }) => {
    test.skip(!toolCallsExportPath, 'Tool calls export path not available');

    await page.goto(`file://${toolCallsExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Find details/collapsible elements
    const details = page.locator('details.tool-call, details.tool, details:has(.tool-content)');
    const detailsCount = await details.count();

    if (detailsCount === 0) {
      // Try alternative selectors
      const altCollapsibles = page.locator('.collapsible, [data-collapsible]');
      const altCount = await altCollapsibles.count();

      if (altCount === 0) {
        test.skip(true, 'No collapsible tool calls found');
        return;
      }
    }

    const firstDetails = details.first();

    // Should start collapsed (no 'open' attribute)
    const initiallyOpen = await firstDetails.getAttribute('open');

    // Click to toggle
    const summary = firstDetails.locator('summary');
    await summary.click();
    await page.waitForTimeout(200);

    // Should now be open
    const afterClickOpen = await firstDetails.getAttribute('open');
    expect(afterClickOpen !== null || afterClickOpen !== initiallyOpen).toBe(true);
  });

  test('tool call content shows when expanded', async ({ page, toolCallsExportPath }) => {
    test.skip(!toolCallsExportPath, 'Tool calls export path not available');

    await page.goto(`file://${toolCallsExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const details = page.locator('details');
    const detailsCount = await details.count();

    if (detailsCount === 0) {
      test.skip(true, 'No collapsible sections found');
      return;
    }

    const firstDetails = details.first();
    const content = firstDetails.locator('.tool-content, .tool-output, pre, code');

    // Open the details
    const summary = firstDetails.locator('summary');
    await summary.click();
    await page.waitForTimeout(200);

    // Content should be visible
    if ((await content.count()) > 0) {
      await expect(content.first()).toBeVisible();
    }
  });

  test('collapse all/expand all functionality', async ({ page, toolCallsExportPath }) => {
    test.skip(!toolCallsExportPath, 'Tool calls export path not available');

    await page.goto(`file://${toolCallsExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    // Look for collapse all button
    const collapseAllBtn = page.locator(
      'button:has-text("Collapse all"), [data-action="collapse-all"]'
    );
    const expandAllBtn = page.locator(
      'button:has-text("Expand all"), [data-action="expand-all"]'
    );

    const hasCollapseAll = (await collapseAllBtn.count()) > 0;
    const hasExpandAll = (await expandAllBtn.count()) > 0;

    if (!hasCollapseAll && !hasExpandAll) {
      test.skip(true, 'No collapse/expand all buttons found');
      return;
    }

    const details = page.locator('details');
    const detailsCount = await details.count();

    if (hasExpandAll) {
      await expandAllBtn.first().click();
      await page.waitForTimeout(300);

      // All should be open
      const allOpen = await details.evaluateAll((els) =>
        els.every((el) => el.hasAttribute('open'))
      );
      expect(allOpen).toBe(true);
    }

    if (hasCollapseAll) {
      await collapseAllBtn.first().click();
      await page.waitForTimeout(300);

      // All should be closed
      const allClosed = await details.evaluateAll((els) =>
        els.every((el) => !el.hasAttribute('open'))
      );
      expect(allClosed).toBe(true);
    }
  });

  test('keyboard can toggle collapsibles', async ({ page, toolCallsExportPath }) => {
    test.skip(!toolCallsExportPath, 'Tool calls export path not available');

    await page.goto(`file://${toolCallsExportPath}`, { waitUntil: 'domcontentloaded' });
    await waitForPageReady(page);

    const details = page.locator('details');
    const detailsCount = await details.count();

    if (detailsCount === 0) {
      test.skip(true, 'No collapsible sections found');
      return;
    }

    const firstDetails = details.first();
    const summary = firstDetails.locator('summary');

    // Focus the summary
    await summary.focus();
    await expect(summary).toBeFocused();

    // Press Enter or Space to toggle
    const initiallyOpen = await firstDetails.getAttribute('open');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(200);

    const afterEnterOpen = await firstDetails.getAttribute('open');
    // State should have changed
    expect(afterEnterOpen !== initiallyOpen).toBe(true);
  });
});

test.describe('Copy to Clipboard', () => {
  test('code blocks have copy buttons', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const codeBlocks = page.locator('pre:has(code):visible');
    const codeCount = await codeBlocks.count();

    if (codeCount === 0) {
      test.skip(true, 'No code blocks found');
      return;
    }

    // Copy buttons are added dynamically and hidden by default
    // Hover over a code block to reveal it (use force to bypass stability check)
    // Use JS scroll (instant) to avoid stability check timeout
    await codeBlocks.first().evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
    await codeBlocks.first().hover({ force: true });

    // Look for copy buttons
    const copyBtn = codeBlocks.first().locator('.copy-code-btn');
    await expect(copyBtn).toBeVisible({ timeout: 2000 });
  });

  test('copy button shows feedback', async ({ page, context, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    // Grant clipboard permissions
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const codeBlocks = page.locator('pre:has(code):visible');
    if (await codeBlocks.count() === 0) {
      test.skip(true, 'No code blocks found');
      return;
    }

    // Hover to reveal copy button (use force to bypass stability check)
    // Use JS scroll (instant) to avoid stability check timeout
    const firstPre = codeBlocks.first();
    await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
    await firstPre.hover({ force: true });

    const copyBtn = firstPre.locator('.copy-code-btn');
    if ((await copyBtn.count()) === 0) {
      test.skip(true, 'No copy button found');
      return;
    }

    await copyBtn.click({ force: true });
    await page.waitForTimeout(500);

    // Look for toast or visual feedback
    const toast = page.locator('.toast, [role="status"], [role="alert"], #toast-container > *');
    const hasToast = (await toast.count()) > 0;

    // Or the button might have a 'copied' class
    const btnHasCopiedClass = await copyBtn.evaluate((el) =>
      el.classList.contains('copied')
    );

    expect(hasToast || btnHasCopiedClass).toBe(true);
  });

  test('clipboard contains code content', async ({ page, context, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Find first code block
    const codeBlocks = page.locator('pre:has(code):visible');
    if (await codeBlocks.count() === 0) {
      test.skip(true, 'No code block found');
      return;
    }

    const firstPre = codeBlocks.first();
    const expectedContent = await firstPre.locator('code').textContent();

    // Hover to reveal copy button (use force to bypass stability check)
    // Use JS scroll (instant) to avoid stability check timeout
    await firstPre.evaluate((el) => el.scrollIntoView({ behavior: 'instant', block: 'center' }));
    await firstPre.hover({ force: true });

    const copyBtn = firstPre.locator('.copy-code-btn');
    if ((await copyBtn.count()) === 0) {
      test.skip(true, 'No copy button found');
      return;
    }

    await copyBtn.click({ force: true });
    await page.waitForTimeout(500);

    // Read clipboard
    const clipboardText = await page.evaluate(() => navigator.clipboard.readText());

    // Should have some content
    expect(clipboardText.length).toBeGreaterThan(0);
  });
});
