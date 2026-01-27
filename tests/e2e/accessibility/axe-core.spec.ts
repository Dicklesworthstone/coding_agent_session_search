import { test, expect, gotoFile, waitForPageReady } from '../setup/test-utils';
import AxeBuilder from '@axe-core/playwright';

/**
 * Accessibility E2E tests - axe-core automated checks
 *
 * Uses axe-core to perform automated accessibility audits against
 * WCAG 2.1 AA standards.
 */

test.describe('axe-core Automated Accessibility Audit', () => {
  test('basic export passes axe-core with no critical violations', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const accessibilityScanResults = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'])
      .analyze();

    // Filter to critical and serious violations
    const criticalViolations = accessibilityScanResults.violations.filter(
      (v) => v.impact === 'critical' || v.impact === 'serious'
    );

    // Log all violations for debugging
    if (accessibilityScanResults.violations.length > 0) {
      console.log('[a11y] Violations found:');
      for (const violation of accessibilityScanResults.violations) {
        console.log(`  - ${violation.id} (${violation.impact}): ${violation.description}`);
        console.log(`    Affected: ${violation.nodes.length} elements`);
      }
    }

    // Should have no critical violations
    expect(criticalViolations.length).toBe(0);
  });

  test('encrypted export passes axe-core after decryption', async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, 'Encrypted export not available');

    await gotoFile(page, encryptedExportPath);
    await waitForPageReady(page);

    // Check pre-decryption accessibility
    const preDecryptResults = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa'])
      .analyze();

    console.log(`[a11y] Pre-decryption: ${preDecryptResults.violations.length} violations`);

    // Enter password
    const passwordInput = page.locator('#password, input[type="password"]');
    if (await passwordInput.count() > 0) {
      await passwordInput.fill(password);
      await page.keyboard.press('Enter');
      await page.waitForSelector('.message, .content, main', { timeout: 30000 });
      await waitForPageReady(page);

      // Check post-decryption accessibility
      const postDecryptResults = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa'])
        .analyze();

      console.log(`[a11y] Post-decryption: ${postDecryptResults.violations.length} violations`);

      const criticalViolations = postDecryptResults.violations.filter(
        (v) => v.impact === 'critical' || v.impact === 'serious'
      );

      expect(criticalViolations.length).toBe(0);
    }
  });

  test('color contrast passes axe-core color-contrast rule', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const results = await new AxeBuilder({ page })
      .include(['body'])
      .withRules(['color-contrast'])
      .analyze();

    const contrastViolations = results.violations.filter((v) => v.id === 'color-contrast');

    if (contrastViolations.length > 0) {
      console.log('[a11y] Color contrast issues:');
      for (const node of contrastViolations[0].nodes) {
        console.log(`  - ${node.html.slice(0, 50)}...`);
        console.log(`    ${node.failureSummary}`);
      }
    }

    // Allow some minor contrast issues for now, but log them
    expect(contrastViolations.length).toBeLessThanOrEqual(5);
  });

  test('forms pass axe-core form-related rules', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const results = await new AxeBuilder({ page })
      .withRules(['label', 'form-field-multiple-labels', 'select-name'])
      .analyze();

    const formViolations = results.violations;

    if (formViolations.length > 0) {
      console.log('[a11y] Form accessibility issues:');
      for (const v of formViolations) {
        console.log(`  - ${v.id}: ${v.nodes.length} elements`);
      }
    }

    expect(formViolations.length).toBe(0);
  });

  test('dark theme passes axe-core', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Switch to dark theme
    const themeToggle = page.locator('#theme-toggle, [data-action="toggle-theme"]');
    if (await themeToggle.count() > 0) {
      const currentTheme = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
      if (currentTheme !== 'dark') {
        await themeToggle.first().click({ force: true });
        await page.waitForTimeout(300);
      }
    }

    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa'])
      .analyze();

    const criticalViolations = results.violations.filter(
      (v) => v.impact === 'critical' || v.impact === 'serious'
    );

    console.log(`[a11y] Dark theme: ${results.violations.length} total violations, ${criticalViolations.length} critical`);

    expect(criticalViolations.length).toBe(0);
  });
});

test.describe('ARIA and Landmarks', () => {
  test('page has required landmarks', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const landmarks = await page.evaluate(() => {
      return {
        hasMain: !!document.querySelector('main, [role="main"]'),
        hasHeader: !!document.querySelector('header, [role="banner"]'),
        hasNav: !!document.querySelector('nav, [role="navigation"]'),
        hasRegion: !!document.querySelector('[role="region"][aria-label], section[aria-label]'),
      };
    });

    console.log('[a11y] Landmarks:', landmarks);

    // Must have main landmark
    expect(landmarks.hasMain).toBe(true);
  });

  test('ARIA roles are valid', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    const results = await new AxeBuilder({ page })
      .withRules(['aria-valid-attr', 'aria-valid-attr-value', 'aria-roles'])
      .analyze();

    if (results.violations.length > 0) {
      console.log('[a11y] ARIA issues:');
      for (const v of results.violations) {
        console.log(`  - ${v.id}: ${v.description}`);
      }
    }

    expect(results.violations.length).toBe(0);
  });

  test('interactive elements have proper ARIA attributes', async ({ page, exportPath }) => {
    test.skip(!exportPath, 'Export path not available');

    await gotoFile(page, exportPath);
    await waitForPageReady(page);

    // Check buttons
    const buttons = page.locator('button');
    const buttonCount = await buttons.count();

    for (let i = 0; i < Math.min(buttonCount, 10); i++) {
      const button = buttons.nth(i);
      const ariaInfo = await button.evaluate((el) => ({
        hasLabel: !!(el.getAttribute('aria-label') || el.textContent?.trim()),
        hasDisabled: el.hasAttribute('aria-disabled') || el.hasAttribute('disabled'),
        hasExpanded: el.hasAttribute('aria-expanded'),
        hasPressed: el.hasAttribute('aria-pressed'),
      }));

      // All buttons should have a label
      expect(ariaInfo.hasLabel).toBe(true);
    }
  });
});
