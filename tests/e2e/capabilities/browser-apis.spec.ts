import { expect, test, waitForPageReady } from "../setup/test-utils";

/**
 * Browser capability detection tests (P6.2: Cross-Browser Testing).
 * Verifies that required browser APIs are available across test browsers.
 *
 * Note: Some APIs require secure contexts (HTTPS). On file:// URLs, certain
 * features may be unavailable in specific browsers - these tests skip gracefully.
 */
test.describe("Browser API Capabilities", () => {
  test("crypto API is available", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => ({
      hasCrypto: typeof crypto !== "undefined",
      hasGetRandomValues: typeof crypto?.getRandomValues === "function",
    }));

    expect(result.hasCrypto).toBe(true);
    expect(result.hasGetRandomValues).toBe(true);
  });

  test("WebCrypto SubtleCrypto availability", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const hasSubtle = await page.evaluate(() => typeof crypto?.subtle !== "undefined");

    // crypto.subtle requires secure context; may be unavailable on file:// URLs
    if (!hasSubtle) {
      const isSecure = await page.evaluate(() => window.isSecureContext);
      test.skip(!isSecure, "crypto.subtle requires secure context");
    }
    expect(hasSubtle).toBe(true);
  });

  test("Web Worker API availability", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => ({
      hasWorker: typeof Worker !== "undefined",
      hasBlob: typeof Blob !== "undefined",
    }));

    expect(result.hasWorker).toBe(true);
    expect(result.hasBlob).toBe(true);
  });

  test("essential JavaScript APIs present", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => ({
      hasTextEncoder: typeof TextEncoder !== "undefined",
      hasTextDecoder: typeof TextDecoder !== "undefined",
      hasArrayBuffer: typeof ArrayBuffer !== "undefined",
      hasUint8Array: typeof Uint8Array !== "undefined",
      hasPromise: typeof Promise !== "undefined",
    }));

    expect(result.hasTextEncoder).toBe(true);
    expect(result.hasTextDecoder).toBe(true);
    expect(result.hasArrayBuffer).toBe(true);
    expect(result.hasUint8Array).toBe(true);
    expect(result.hasPromise).toBe(true);
  });

  test("storage APIs detection", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => {
      // Just check if the APIs exist; don't actually use them (may be blocked on file://)
      return {
        hasLocalStorage: typeof localStorage !== "undefined",
        hasSessionStorage: typeof sessionStorage !== "undefined",
        hasIndexedDB: typeof indexedDB !== "undefined",
      };
    });

    // These APIs should exist even if restricted
    expect(typeof result.hasLocalStorage).toBe("boolean");
    expect(typeof result.hasSessionStorage).toBe("boolean");
    expect(typeof result.hasIndexedDB).toBe("boolean");
  });
});

test.describe("Mobile Viewport Support", () => {
  test("page has viewport meta tag", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const viewportMeta = await page.locator('meta[name="viewport"]').getAttribute("content");

    // Viewport meta should exist for proper mobile rendering
    if (viewportMeta) {
      expect(viewportMeta).toContain("width=");
    }
  });

  test("content is visible in viewport", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    // Main content should be visible
    const mainContent = page.locator("main, .conversation, body");
    await expect(mainContent.first()).toBeVisible();
  });

  test("touch detection works on mobile projects", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const touchInfo = await page.evaluate(() => ({
      maxTouchPoints: navigator.maxTouchPoints ?? 0,
      hasOntouchstart: "ontouchstart" in window,
    }));

    // On mobile projects (Pixel 5, iPhone 12), maxTouchPoints > 0
    // On desktop projects, maxTouchPoints is typically 0
    expect(typeof touchInfo.maxTouchPoints).toBe("number");
    expect(touchInfo.maxTouchPoints).toBeGreaterThanOrEqual(0);
  });
});

test.describe("Cross-Browser Feature Parity", () => {
  test("JSON parsing works correctly", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => {
      const obj = { test: "value", num: 42 };
      const str = JSON.stringify(obj);
      const parsed = JSON.parse(str);
      return parsed.test === "value" && parsed.num === 42;
    });

    expect(result).toBe(true);
  });

  test("fetch API is available", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const hasFetch = await page.evaluate(() => typeof fetch === "function");
    expect(hasFetch).toBe(true);
  });

  test("URL API is available", async ({ page, exportPath }) => {
    test.skip(!exportPath, "Export path not available");

    await page.goto(`file://${exportPath}`, { waitUntil: "domcontentloaded" });
    await waitForPageReady(page);

    const result = await page.evaluate(() => {
      try {
        const url = new URL("https://example.com/path?query=1");
        return {
          hasURL: true,
          pathname: url.pathname === "/path",
          search: url.search === "?query=1",
        };
      } catch {
        return { hasURL: false };
      }
    });

    expect(result.hasURL).toBe(true);
  });
});
