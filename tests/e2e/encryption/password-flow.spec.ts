import { countMessages, expect, gotoFile, test, waitForPageReady } from "../setup/test-utils";

// Run encryption tests serially to avoid race conditions with the modal
test.describe.configure({ mode: "serial" });

test.describe("Encrypted Export - Password Prompt", () => {
  test("shows password modal on load", async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await gotoFile(page, encryptedExportPath);

    // Modal should be visible
    const modal = page.locator(
      '#password-modal, .decrypt-modal, [data-testid="decrypt-modal"], .modal:has(input[type="password"])',
    );

    await expect(modal.first()).toBeVisible({ timeout: 5000 });

    // Actual message content should NOT be visible before decryption
    // The main container may exist, but messages should not be rendered yet
    const messages = page.locator(".message");
    const messageCount = await messages.count();

    // Before decryption, no messages should be visible
    expect(messageCount).toBe(0);
  });

  test("password input is present and focusable", async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"], [data-testid="password-input"]',
    );

    await expect(passwordInput.first()).toBeVisible();

    // Should be focusable
    await passwordInput.first().focus();
    await expect(passwordInput.first()).toBeFocused();
  });
});

test.describe("Encrypted Export - Correct Password", () => {
  test("decrypts and displays content with correct password", async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await test.step("Load encrypted export", async () => {
      await gotoFile(page, encryptedExportPath);
      await page.waitForTimeout(500);
    });

    await test.step("Enter password and submit", async () => {
      const passwordInput = page.locator(
        '#password-input, input[type="password"], [data-testid="password-input"]',
      );
      await passwordInput.first().fill(password);
      await passwordInput.first().press("Enter");
      await page.waitForTimeout(2000);
    });

    await test.step("Verify decrypted content", async () => {
      const modal = page.locator('#password-modal, .decrypt-modal, [data-testid="decrypt-modal"]');
      await expect(modal.first()).not.toBeVisible({ timeout: 10000 });

      const messages = page.locator(".message");
      const messageCount = await messages.count();
      expect(messageCount).toBeGreaterThan(0);
    });
  });

  test("decryption completes within 5 seconds", async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator('#password-input, input[type="password"]');
    await passwordInput.first().fill(password);

    const start = Date.now();

    // Submit form via Enter key
    await passwordInput.first().press("Enter");

    // Wait for content to appear
    const messages = page.locator(".message");
    await expect(messages.first()).toBeVisible({ timeout: 5000 });

    const elapsed = Date.now() - start;
    // Allow some slack for CI environment slowdown
    expect(elapsed).toBeLessThan(10000);
  });

  test("Enter key submits password", async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator('#password-input, input[type="password"]');
    await passwordInput.first().fill(password);

    // Press Enter instead of clicking button
    await page.keyboard.press("Enter");

    // Wait for decryption
    await page.waitForTimeout(2000);

    // Content should appear
    const messages = page.locator(".message");
    await expect(messages.first()).toBeVisible({ timeout: 10000 });
  });
});

test.describe("Encrypted Export - Wrong Password", () => {
  test("shows error with wrong password", async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");

    await test.step("Load encrypted export", async () => {
      await gotoFile(page, encryptedExportPath);
      await page.waitForTimeout(500);
    });

    await test.step("Submit wrong password", async () => {
      const passwordInput = page.locator('#password-input, input[type="password"]');
      await passwordInput.first().fill("wrong-password-123");
      await passwordInput.first().press("Enter");
      await page.waitForTimeout(2000);
    });

    await test.step("Verify error and content hidden", async () => {
      const error = page.locator('#decrypt-error, .decrypt-error, .error, [role="alert"]');
      await expect(error.first()).toBeVisible({ timeout: 5000 });

      const errorText = await error.first().textContent();
      expect(errorText?.toLowerCase()).toMatch(/incorrect|failed|error|invalid|wrong/);

      const messages = page.locator(".message");
      const messageCount = await messages.count();
      expect(messageCount).toBe(0);
    });
  });

  test("allows retry after wrong password", async ({ page, encryptedExportPath, password }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");
    test.setTimeout(60000);

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator('#password-input, input[type="password"]');

    // First attempt with wrong password - submit via Enter
    await passwordInput.first().fill("wrong");
    await passwordInput.first().press("Enter");
    await page.waitForTimeout(1500);

    // Error should appear
    const error = page.locator("#decrypt-error, .decrypt-error, .error");
    await expect(error.first()).toBeVisible();

    // Clear and try correct password
    await passwordInput.first().fill("");
    await passwordInput.first().fill(password);
    await passwordInput.first().press("Enter");

    // Wait for decryption
    await page.waitForTimeout(2000);

    // Should succeed now
    const messages = page.locator(".message");
    await expect(messages.first()).toBeVisible({ timeout: 10000 });
  });
});

test.describe("Encrypted Export - Security", () => {
  // 2l1b0.67 in a real browser: before unlock, neither the title nor the
  // session metadata nor any message text is readable; after unlock the same
  // conversation text appears, so the negative cannot pass on an empty page.
  // The phrases come from the export's fixture (claude_code_auth_fix.jsonl):
  // its first prompt, workspace path and git branch.
  test("no plaintext title, metadata or messages before unlock", async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, "Encrypted export path not available");
    const secrets = ["JWT token refresh", "/home/dev/myapp", "fix/auth-refresh"];

    await gotoFile(page, encryptedExportPath);
    expect(await page.title()).toBe("Encrypted conversation");
    const sealed = await page.content();
    for (const secret of secrets) {
      expect(sealed, `"${secret}" is readable before unlock`).not.toContain(secret);
    }

    const passwordInput = page.locator(
      '#password-input, input[type="password"], [data-testid="password-input"]',
    );
    await passwordInput.first().fill(password);
    await passwordInput.first().press("Enter");
    await expect(page.locator(".message").first()).toBeVisible({ timeout: 10000 });
    expect(await page.content()).toContain(secrets[0]);
  });
});
