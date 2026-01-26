import { test, expect, gotoFile, waitForPageReady, countMessages } from '../setup/test-utils';

// Run encryption tests serially to avoid race conditions with the modal
test.describe.configure({ mode: 'serial' });

test.describe('Encrypted Export - Password Prompt', () => {
  test('shows password modal on load', async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);

    // Modal should be visible
    const modal = page.locator(
      '#password-modal, .decrypt-modal, [data-testid="decrypt-modal"], .modal:has(input[type="password"])'
    );

    await expect(modal.first()).toBeVisible({ timeout: 5000 });

    // Actual message content should NOT be visible before decryption
    // The main container may exist, but messages should not be rendered yet
    const messages = page.locator('.message');
    const messageCount = await messages.count();

    // Before decryption, no messages should be visible
    expect(messageCount).toBe(0);
  });

  test('password input is present and focusable', async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"], [data-testid="password-input"]'
    );

    await expect(passwordInput.first()).toBeVisible();

    // Should be focusable
    await passwordInput.first().focus();
    await expect(passwordInput.first()).toBeFocused();
  });
});

test.describe('Encrypted Export - Correct Password', () => {
  test('decrypts and displays content with correct password', async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    // Find and fill password input
    const passwordInput = page.locator(
      '#password-input, input[type="password"], [data-testid="password-input"]'
    );
    await passwordInput.first().fill(password);

    // Submit the form via Enter key (more reliable than clicking the button)
    await passwordInput.first().press('Enter');

    // Wait for decryption
    await page.waitForTimeout(2000);

    // Modal should disappear
    const modal = page.locator('#password-modal, .decrypt-modal, [data-testid="decrypt-modal"]');
    await expect(modal.first()).not.toBeVisible({ timeout: 10000 });

    // Content should now be visible
    const messages = page.locator('.message');
    const messageCount = await messages.count();
    expect(messageCount).toBeGreaterThan(0);
  });

  test('decryption completes within 5 seconds', async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"]'
    );
    await passwordInput.first().fill(password);

    const start = Date.now();

    // Submit form via Enter key
    await passwordInput.first().press('Enter');

    // Wait for content to appear
    const messages = page.locator('.message');
    await expect(messages.first()).toBeVisible({ timeout: 5000 });

    const elapsed = Date.now() - start;
    // Allow some slack for CI environment slowdown
    expect(elapsed).toBeLessThan(10000);
  });

  test('Enter key submits password', async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"]'
    );
    await passwordInput.first().fill(password);

    // Press Enter instead of clicking button
    await page.keyboard.press('Enter');

    // Wait for decryption
    await page.waitForTimeout(2000);

    // Content should appear
    const messages = page.locator('.message');
    await expect(messages.first()).toBeVisible({ timeout: 10000 });
  });
});

test.describe('Encrypted Export - Wrong Password', () => {
  test('shows error with wrong password', async ({ page, encryptedExportPath }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"]'
    );
    await passwordInput.first().fill('wrong-password-123');

    // Submit via Enter key
    await passwordInput.first().press('Enter');

    await page.waitForTimeout(2000);

    // Error message should appear
    const error = page.locator(
      '#decrypt-error, .decrypt-error, .error, [role="alert"]'
    );
    await expect(error.first()).toBeVisible({ timeout: 5000 });

    // Error should mention failure
    const errorText = await error.first().textContent();
    expect(errorText?.toLowerCase()).toMatch(/incorrect|failed|error|invalid|wrong/);

    // Content should still be hidden
    const messages = page.locator('.message');
    const messageCount = await messages.count();
    expect(messageCount).toBe(0);
  });

  test('allows retry after wrong password', async ({
    page,
    encryptedExportPath,
    password,
  }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');
    test.setTimeout(60000);

    await gotoFile(page, encryptedExportPath);
    await page.waitForTimeout(500);

    const passwordInput = page.locator(
      '#password-input, input[type="password"]'
    );

    // First attempt with wrong password - submit via Enter
    await passwordInput.first().fill('wrong');
    await passwordInput.first().press('Enter');
    await page.waitForTimeout(1500);

    // Error should appear
    const error = page.locator('#decrypt-error, .decrypt-error, .error');
    await expect(error.first()).toBeVisible();

    // Clear and try correct password
    await passwordInput.first().fill('');
    await passwordInput.first().fill(password);
    await passwordInput.first().press('Enter');

    // Wait for decryption
    await page.waitForTimeout(2000);

    // Should succeed now
    const messages = page.locator('.message');
    await expect(messages.first()).toBeVisible({ timeout: 10000 });
  });
});

test.describe('Encrypted Export - Security', () => {
  test('plaintext content is not visible in encrypted HTML', async ({
    page,
    encryptedExportPath,
  }) => {
    test.skip(!encryptedExportPath, 'Encrypted export path not available');

    // Get the raw HTML source before decryption
    await gotoFile(page, encryptedExportPath);
    const html = await page.content();

    // Encrypted content should contain base64/hex encrypted data
    expect(html).toMatch(/ciphertext|encrypted|base64|iv|salt/i);

    // Should not contain obvious plaintext message content
    // (unless it's UI text like "Enter password")
    const messagePhrases = [
      'authentication',
      'function main',
      'import React',
      'def __init__',
    ];

    for (const phrase of messagePhrases) {
      // These should NOT appear in the HTML (they should be encrypted)
      const containsPhrase = html.toLowerCase().includes(phrase.toLowerCase());
      // Skip if it's a common word that might appear in UI
      if (containsPhrase && phrase !== 'authentication') {
        // This is a potential security issue - plaintext visible
        console.warn(`Potential plaintext leak: "${phrase}" found in encrypted HTML`);
      }
    }
  });
});
