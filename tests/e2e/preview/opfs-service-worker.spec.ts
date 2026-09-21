import { expect, test } from "../setup/test-utils";

test.describe("Pages Preview - Service Worker and OPFS", () => {
  test("service worker controls the page and COI is enabled", async ({ page, previewUrl }) => {
    test.skip(!previewUrl, "Preview URL not available");

    await page.goto(previewUrl, { waitUntil: "domcontentloaded" });

    // Ensure service worker support and wait for registration
    await page.evaluate(async () => {
      if (!("serviceWorker" in navigator)) {
        throw new Error("Service worker not supported");
      }
      await navigator.serviceWorker.ready;
    });

    // Reload to ensure controller is set
    await page.reload({ waitUntil: "domcontentloaded" });

    await page.waitForFunction(() => navigator.serviceWorker.controller !== null, {
      timeout: 5000,
    });

    const status = await page.evaluate(() => ({
      hasController: !!navigator.serviceWorker.controller,
      controllerState: navigator.serviceWorker.controller?.state ?? null,
      crossOriginIsolated: self.crossOriginIsolated === true,
    }));

    expect(status.hasController).toBe(true);
    expect(status.crossOriginIsolated).toBe(true);
  });

  test("OPFS is available and writable when supported", async ({ page, previewUrl }) => {
    test.skip(!previewUrl, "Preview URL not available");

    await page.goto(previewUrl, { waitUntil: "domcontentloaded" });

    const result = await page.evaluate(async () => {
      const available = "storage" in navigator && "getDirectory" in navigator.storage;
      if (!available) {
        return { available: false };
      }
      try {
        const root = await navigator.storage.getDirectory();
        const handle = await root.getFileHandle("cass-opfs-test.txt", { create: true });
        const writable = await handle.createWritable();
        await writable.write("opfs-ok");
        await writable.close();
        const file = await handle.getFile();
        const text = await file.text();
        try {
          await root.removeEntry("cass-opfs-test.txt");
        } catch {
          // Ignore cleanup errors
        }
        return { available: true, text };
      } catch (error) {
        return { available: true, error: String(error) };
      }
    });

    if (!result.available) {
      test.skip(true, "OPFS not available in this browser");
    }

    expect(result.error).toBeUndefined();
    expect(result.text).toBe("opfs-ok");
  });
});
