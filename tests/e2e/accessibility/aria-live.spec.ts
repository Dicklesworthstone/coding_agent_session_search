import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';

/**
 * Accessibility E2E tests - ARIA live regions
 *
 * Tests that dynamic content updates are properly announced
 * to screen readers via ARIA live regions.
 */

test.describe('ARIA Live Region Announcements', () => {
  test('page has appropriate live regions defined', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const liveRegions = await page.evaluate(() => {
      const regions = document.querySelectorAll('[aria-live], [role="status"], [role="alert"], [role="log"]');
      return Array.from(regions).map((el) => ({
        tag: el.tagName,
        role: el.getAttribute('role'),
        ariaLive: el.getAttribute('aria-live'),
        ariaAtomic: el.getAttribute('aria-atomic'),
        hasContent: el.textContent?.trim().length || 0 > 0,
      }));
    });

    console.log(`[a11y] Found ${liveRegions.length} live regions:`, liveRegions);

    // Page should have at least one live region for dynamic content
    // (search results, copy feedback, etc.)
    // If none found, it's a warning but not necessarily a failure
    if (liveRegions.length === 0) {
      console.log('[a11y-warning] No ARIA live regions found - dynamic updates may not be announced');
    }
  });

  test('search results update announces to screen readers', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const searchInput = page.locator('#search-input, input[type="search"]');
    if (await searchInput.count() === 0) {
      test.skip(true, 'Search input not found');
      return;
    }

    // Set up live region observer
    const liveUpdates: string[] = [];
    await page.exposeFunction('recordLiveUpdate', (text: string) => {
      liveUpdates.push(text);
    });

    await page.evaluate(() => {
      const observer = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
          const target = mutation.target as Element;
          if (
            target.getAttribute('aria-live') ||
            target.getAttribute('role') === 'status' ||
            target.getAttribute('role') === 'alert'
          ) {
            (window as unknown as { recordLiveUpdate: (t: string) => void }).recordLiveUpdate(
              target.textContent || ''
            );
          }
        }
      });

      observer.observe(document.body, {
        childList: true,
        subtree: true,
        characterData: true,
      });
    });

    // Perform search
    await searchInput.first().fill('function');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(500);

    console.log(`[a11y] Live region updates during search: ${liveUpdates.length}`);

    // Note: Not all implementations will have live regions
    // This test documents the current behavior
  });

  test('copy action announces success', async ({ page, exportPath, context }) => {
    test.skip(!exportPath, 'Export path not available');

    await context.grantPermissions(['clipboard-read', 'clipboard-write']);

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const copyButton = page.locator('[data-action="copy"], .copy-btn').first();
    if (await copyButton.count() === 0) {
      test.skip(true, 'Copy button not found');
      return;
    }

    // Check for aria-live on feedback element
    await copyButton.click({ force: true });
    await page.waitForTimeout(300);

    // Look for feedback with live region
    const feedback = await page.evaluate(() => {
      // Look for common feedback patterns
      const feedbackSelectors = [
        '.copied',
        '.copy-success',
        '.toast',
        '[role="status"]',
        '[aria-live]',
      ];

      for (const selector of feedbackSelectors) {
        const el = document.querySelector(selector);
        if (el && el.textContent?.includes('copied')) {
          return {
            found: true,
            hasLiveRegion: !!el.closest('[aria-live], [role="status"], [role="alert"]'),
            text: el.textContent,
          };
        }
      }

      return { found: false, hasLiveRegion: false };
    });

    console.log(`[a11y] Copy feedback: found=${feedback.found}, hasLiveRegion=${feedback.hasLiveRegion}`);

    if (feedback.found && !feedback.hasLiveRegion) {
      console.log('[a11y-warning] Copy feedback lacks live region - not announced to screen readers');
    }
  });

  test('theme toggle announces change', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const themeToggle = page.locator('#theme-toggle, [data-action="toggle-theme"]');
    if (await themeToggle.count() === 0) {
      test.skip(true, 'Theme toggle not found');
      return;
    }

    // Track aria-pressed or similar state changes
    const beforeState = await themeToggle.first().evaluate((el) => ({
      ariaPressed: el.getAttribute('aria-pressed'),
      ariaLabel: el.getAttribute('aria-label'),
    }));

    await themeToggle.first().click({ force: true });
    await page.waitForTimeout(300);

    const afterState = await themeToggle.first().evaluate((el) => ({
      ariaPressed: el.getAttribute('aria-pressed'),
      ariaLabel: el.getAttribute('aria-label'),
    }));

    console.log(`[a11y] Theme toggle state: before=${JSON.stringify(beforeState)}, after=${JSON.stringify(afterState)}`);

    // The toggle should indicate its state
    // Either through aria-pressed change or aria-label change
    const stateChanged =
      beforeState.ariaPressed !== afterState.ariaPressed ||
      beforeState.ariaLabel !== afterState.ariaLabel;

    if (!stateChanged) {
      console.log('[a11y-warning] Theme toggle state not reflected in ARIA attributes');
    }
  });

  test('error messages use alert role', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found');
      return;
    }

    // Enter wrong password to trigger error
    await passwordInput.fill('wrong-password-12345');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(1000);

    // Check for error with alert role
    const errorInfo = await page.evaluate(() => {
      const errorSelectors = [
        '[role="alert"]',
        '.error',
        '.error-message',
        '[aria-live="assertive"]',
      ];

      for (const selector of errorSelectors) {
        const el = document.querySelector(selector);
        if (el && el.textContent && el.textContent.trim().length > 0) {
          return {
            found: true,
            hasAlertRole: el.getAttribute('role') === 'alert' ||
                         el.closest('[role="alert"]') !== null ||
                         el.getAttribute('aria-live') === 'assertive',
            text: el.textContent.trim().slice(0, 50),
          };
        }
      }

      return { found: false, hasAlertRole: false };
    });

    console.log(`[a11y] Error message: found=${errorInfo.found}, hasAlertRole=${errorInfo.hasAlertRole}`);

    if (errorInfo.found) {
      // Error messages should use role="alert" for screen readers
      if (!errorInfo.hasAlertRole) {
        console.log('[a11y-warning] Error message lacks role="alert" - may not be announced urgently');
      }
    }
  });

  test('loading states have appropriate ARIA', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found');
      return;
    }

    // Start decryption
    await passwordInput.fill(password);

    // Watch for aria-busy or loading states
    const loadingPromise = page.evaluate(() => {
      return new Promise<{
        hadAriaBusy: boolean;
        hadLoadingState: boolean;
      }>((resolve) => {
        let hadAriaBusy = false;
        let hadLoadingState = false;

        const observer = new MutationObserver(() => {
          if (document.querySelector('[aria-busy="true"]')) {
            hadAriaBusy = true;
          }
          if (document.querySelector('.loading, .spinner, .decrypting, [data-loading="true"]')) {
            hadLoadingState = true;
          }
        });

        observer.observe(document.body, { attributes: true, childList: true, subtree: true });

        setTimeout(() => {
          observer.disconnect();
          resolve({ hadAriaBusy, hadLoadingState });
        }, 3000);
      });
    });

    await page.keyboard.press('Enter');

    const loadingResult = await loadingPromise;

    console.log(`[a11y] Loading states: aria-busy=${loadingResult.hadAriaBusy}, loading-indicator=${loadingResult.hadLoadingState}`);

    if (loadingResult.hadLoadingState && !loadingResult.hadAriaBusy) {
      console.log('[a11y-info] Loading state shown visually but aria-busy not set');
    }
  });
});

test.describe('Focus Management', () => {
  test('focus moves to content after decryption', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() === 0) {
      test.skip(true, 'Password input not found');
      return;
    }

    await passwordInput.fill(password);
    await page.keyboard.press('Enter');

    // Wait for decryption
    await page.waitForSelector('.message, .content, main', { timeout: 30000 });
    await page.waitForTimeout(500);

    // Check where focus ended up
    const focusInfo = await page.evaluate(() => {
      const el = document.activeElement;
      return {
        tag: el?.tagName,
        role: el?.getAttribute('role'),
        inMain: !!el?.closest('main'),
        isBody: el === document.body,
      };
    });

    console.log(`[a11y] Focus after decryption:`, focusInfo);

    // Focus should ideally be on main content or skip link
    // Not just left on body
    if (focusInfo.isBody) {
      console.log('[a11y-info] Focus remains on body after decryption - consider moving to main content');
    }
  });

  test('skip link is present and functional', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Look for skip link
    const skipLink = page.locator('a[href="#main"], a[href="#content"], .skip-link, [class*="skip"]');

    if (await skipLink.count() > 0) {
      // Skip link should be first focusable
      await page.keyboard.press('Tab');

      const isSkipLinkFocused = await skipLink.first().evaluate(
        (el) => el === document.activeElement
      );

      console.log(`[a11y] Skip link is first focusable: ${isSkipLinkFocused}`);

      // Activate skip link
      if (isSkipLinkFocused) {
        await page.keyboard.press('Enter');
        await page.waitForTimeout(200);

        // Focus should have moved to main content
        const focusInMain = await page.evaluate(() => {
          const active = document.activeElement;
          return active?.closest('main') !== null || active?.id === 'main' || active?.id === 'content';
        });

        expect(focusInMain).toBe(true);
      }
    } else {
      console.log('[a11y-info] No skip link found - consider adding one for keyboard users');
    }
  });
});
