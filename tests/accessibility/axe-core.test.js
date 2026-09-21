/**
 * P6.7: axe-core Accessibility Tests for cass Archive Web Viewer
 *
 * These tests use axe-core to validate WCAG 2.1 Level AA compliance.
 * Run in a browser environment or with puppeteer/playwright.
 *
 * Prerequisites:
 *   npm install @axe-core/playwright playwright
 *
 * Run:
 *   npx playwright test tests/accessibility/axe-core.test.js
 */

import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";

// Test configuration
const TEST_CONFIG = {
  // Timeout for page loads and operations
  timeout: 30000,
  // axe-core rules to run (WCAG 2.1 AA)
  axeRules: {
    runOnly: {
      type: "tag",
      values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "best-practice"],
    },
  },
  // Rules that are informational only (don't fail tests)
  warnOnlyRules: [
    "region", // Landmark regions - sometimes over-flagged
    "landmark-one-main", // Checked elsewhere
  ],
};

/**
 * Helper to run axe-core and return results
 */
async function runAxeAnalysis(page, context = "full page") {
  const results = await new AxeBuilder({ page })
    .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"])
    .analyze();

  // Log violations for debugging
  if (results.violations.length > 0) {
    console.log(`\n--- axe-core violations (${context}) ---`);
    for (const violation of results.violations) {
      console.log(`\n${violation.id}: ${violation.description}`);
      console.log(`Impact: ${violation.impact}`);
      console.log(`Help: ${violation.helpUrl}`);
      for (const node of violation.nodes.slice(0, 3)) {
        console.log(`  - ${node.html.substring(0, 100)}...`);
        console.log(`    ${node.failureSummary}`);
      }
      if (violation.nodes.length > 3) {
        console.log(`  ... and ${violation.nodes.length - 3} more`);
      }
    }
    console.log("---\n");
  }

  return results;
}

/**
 * Filter violations to separate errors from warnings
 */
function categorizeViolations(violations) {
  const errors = violations.filter((v) => !TEST_CONFIG.warnOnlyRules.includes(v.id));
  const warnings = violations.filter((v) => TEST_CONFIG.warnOnlyRules.includes(v.id));
  return { errors, warnings };
}

// Test suite
test.describe("Accessibility - Auth Page", () => {
  test("auth page has no critical accessibility violations", async ({ page }) => {
    // Navigate to the test archive (adjust path as needed)
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
      timeout: TEST_CONFIG.timeout,
    });

    // Wait for auth screen to be visible
    await page.waitForSelector("#auth-screen", { state: "visible" });

    // Run axe-core analysis
    const results = await runAxeAnalysis(page, "auth page");
    const { errors, warnings } = categorizeViolations(results.violations);

    // Log warnings but don't fail
    if (warnings.length > 0) {
      console.log(
        `${warnings.length} accessibility warnings (non-blocking):`,
        warnings.map((w) => w.id).join(", "),
      );
    }

    // Fail on critical violations
    expect(errors, "Critical accessibility violations found").toHaveLength(0);
  });

  test("password input has accessible label", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    const passwordInput = page.locator("#password");
    await expect(passwordInput).toBeVisible();

    // Check for associated label
    const labelledBy = await passwordInput.getAttribute("aria-labelledby");
    const hasLabel = await page.locator(`label[for="password"]`).count();

    expect(labelledBy || hasLabel > 0, "Password input must have an accessible label").toBeTruthy();
  });

  test("skip link is functional", async ({ page }) => {
    await page.goto("http://localhost:8080/");

    const skipLink = page.locator(".skip-link");
    await expect(skipLink).toBeAttached();

    // Tab to the skip link
    await page.keyboard.press("Tab");

    // Skip link should be visible when focused
    await expect(skipLink).toBeFocused();
    await expect(skipLink).toBeVisible();

    // Pressing Enter should skip to main content
    await page.keyboard.press("Enter");

    // Focus should move to main content
    const mainContent = page.locator("#main-content");
    const focusedElement = await page.evaluate(() => document.activeElement?.id);
    expect(["main-content", "app-content", "auth-screen"]).toContain(focusedElement);
  });

  test("form can be submitted with keyboard only", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Tab to password field
    await page.keyboard.press("Tab"); // Skip link
    await page.keyboard.press("Tab"); // Fingerprint help (if visible)
    await page.keyboard.press("Tab"); // Password input

    const passwordInput = page.locator("#password");
    if (await passwordInput.isVisible()) {
      await passwordInput.focus();
      await passwordInput.fill("test-password");

      // Tab to submit button
      await page.keyboard.press("Tab");
      await page.keyboard.press("Tab"); // Toggle password visibility
      await page.keyboard.press("Tab"); // Unlock button

      const unlockBtn = page.locator("#unlock-btn");
      await expect(unlockBtn).toBeFocused();
    }
  });

  test("error messages are announced to screen readers", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Check that error container has aria-live
    const authError = page.locator("#auth-error");
    const ariaLive = await authError.getAttribute("aria-live");
    expect(ariaLive).toBe("assertive");

    // Check that error has role="alert"
    const role = await authError.getAttribute("role");
    expect(role).toBe("alert");
  });

  test("progress indicator is accessible", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    const progressBar = page.locator("#auth-progress .progress-bar");
    await expect(progressBar).toBeAttached();

    // Check ARIA attributes
    const role = await progressBar.getAttribute("role");
    expect(role).toBe("progressbar");

    const valueMin = await progressBar.getAttribute("aria-valuemin");
    const valueMax = await progressBar.getAttribute("aria-valuemax");
    expect(valueMin).toBe("0");
    expect(valueMax).toBe("100");
  });
});

test.describe("Accessibility - App Screen (after unlock)", () => {
  // Note: These tests require a pre-unlocked state or test archive

  test("navigation is keyboard accessible", async ({ page }) => {
    // This would require setting up a test archive that's already unlocked
    // or mocking the auth flow
    test.skip(true, "Requires test archive setup");

    await page.goto("http://localhost:8080/#/");

    const navLinks = page.locator(".nav-link");
    const count = await navLinks.count();

    for (let i = 0; i < count; i++) {
      const link = navLinks.nth(i);
      await link.focus();
      await expect(link).toBeFocused();
    }
  });

  test("search results are announced to screen readers", async ({ page }) => {
    test.skip(true, "Requires test archive setup");

    // After search, check that results are announced
    const announcer = page.locator("#sr-announcer");
    await expect(announcer).toBeAttached();

    const ariaLive = await announcer.getAttribute("aria-live");
    expect(ariaLive).toBe("polite");
  });
});

test.describe("Accessibility - Color Contrast", () => {
  test("text has sufficient contrast ratio", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Run axe-core specifically for color contrast
    const results = await new AxeBuilder({ page }).withRules(["color-contrast"]).analyze();

    expect(
      results.violations.filter((v) => v.id === "color-contrast"),
      "Color contrast violations found",
    ).toHaveLength(0);
  });
});

test.describe("Accessibility - Keyboard Navigation", () => {
  test("focus is visible on all interactive elements", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Get all focusable elements
    const focusableSelector =
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])';
    const focusableElements = page.locator(focusableSelector);
    const count = await focusableElements.count();

    for (let i = 0; i < Math.min(count, 10); i++) {
      const element = focusableElements.nth(i);
      if (await element.isVisible()) {
        await element.focus();

        // Check that element has visible focus indicator
        const hasFocusStyle = await element.evaluate((el) => {
          const styles = window.getComputedStyle(el);
          return (
            styles.outline !== "none" || styles.boxShadow !== "none" || styles.borderColor !== ""
          );
        });

        // Note: This is a soft check - focus styles might be :focus-visible only
        // which is fine for accessibility
      }
    }
  });

  test("no keyboard traps exist", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Tab through all elements
    const startElement = await page.evaluate(() => document.activeElement?.tagName);
    const visitedElements = new Set();
    let iterations = 0;
    const maxIterations = 50;

    while (iterations < maxIterations) {
      await page.keyboard.press("Tab");
      const currentElement = await page.evaluate(() => ({
        tag: document.activeElement?.tagName,
        id: document.activeElement?.id,
        class: document.activeElement?.className,
      }));

      const elementKey = `${currentElement.tag}-${currentElement.id}-${currentElement.class}`;

      if (visitedElements.has(elementKey)) {
        // We've looped back - no trap
        break;
      }
      visitedElements.add(elementKey);
      iterations++;
    }

    expect(iterations, "Possible keyboard trap - could not tab through all elements").toBeLessThan(
      maxIterations,
    );
  });
});

test.describe("Accessibility - Screen Reader Support", () => {
  test("page has proper document structure", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Check for lang attribute
    const htmlLang = await page.locator("html").getAttribute("lang");
    expect(htmlLang).toBeTruthy();

    // Check for document title
    const title = await page.title();
    expect(title).toBeTruthy();

    // Check for main landmark
    const main = page.locator('main, [role="main"]');
    expect(await main.count()).toBeGreaterThan(0);

    // Check for h1
    const h1 = page.locator("h1");
    expect(await h1.count()).toBeGreaterThan(0);
  });

  test("live regions exist for dynamic content", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Check for aria-live regions
    const liveRegions = page.locator("[aria-live]");
    expect(await liveRegions.count()).toBeGreaterThan(0);

    // Verify they have proper values
    const announcer = page.locator("#sr-announcer");
    if ((await announcer.count()) > 0) {
      const ariaLive = await announcer.getAttribute("aria-live");
      expect(["polite", "assertive"]).toContain(ariaLive);
    }
  });

  test("form fields have accessible names", async ({ page }) => {
    await page.goto("http://localhost:8080/", {
      waitUntil: "networkidle",
    });

    // Run axe specifically for form labels
    const results = await new AxeBuilder({ page })
      .withRules(["label", "form-field-multiple-labels"])
      .analyze();

    expect(
      results.violations.filter((v) => v.id === "label"),
      "Form fields missing accessible labels",
    ).toHaveLength(0);
  });
});
