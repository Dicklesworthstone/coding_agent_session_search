// Per coding_agent_session_search-vz9t8.7. Validates the browser-side
// Trust-On-First-Use fingerprint enforcement for HTML exports.
//
// The TOFU logic lives in src/pages_assets/auth.js::verifyTofu(currentFingerprint, storageKey).
// This spec exercises it via a page.evaluate() bridge, simulating localStorage
// state for each scenario.

import { test, expect } from '@playwright/test';

// Inline the verifyTofu function under test. This MUST stay in sync with
// src/pages_assets/auth.js::verifyTofu — when that file changes, regenerate
// this constant. Embedding the source rather than fetching avoids requiring
// a built export to test.
const VERIFY_TOFU_SOURCE = `
async function verifyTofu(currentFingerprint, storageKey) {
    try {
        const storedFingerprint = localStorage.getItem(storageKey);
        if (!storedFingerprint) {
            localStorage.setItem(storageKey, currentFingerprint);
            return { valid: true, isFirstVisit: true };
        }
        if (storedFingerprint === currentFingerprint) {
            return { valid: true, isFirstVisit: false };
        }
        return {
            valid: false,
            reason: 'TOFU_VIOLATION',
            previousFingerprint: storedFingerprint,
            currentFingerprint: currentFingerprint
        };
    } catch (e) {
        console.warn('TOFU check unavailable:', e);
        return { valid: true, isFirstVisit: true };
    }
}
`;

async function setupPageWithTofu(page: import('@playwright/test').Page) {
  await page.route('http://cass-tofu.test/**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/html',
      body: '<!doctype html><html><head></head><body></body></html>',
    });
  });
  await page.goto(`http://cass-tofu.test/${Date.now()}`);
  // Inject verifyTofu into the page.
  await page.evaluate((src: string) => {
    const script = document.createElement('script');
    script.textContent = src;
    document.head.appendChild(script);
  }, VERIFY_TOFU_SOURCE);
}

test.describe('Integrity TOFU verification (vz9t8.7)', () => {
  test.beforeEach(async ({ page }) => {
    await setupPageWithTofu(page);
    // Clear localStorage between tests.
    await page.evaluate(() => {
      try {
        localStorage.clear();
      } catch (_) { /* ignored */ }
    });
  });

  test('integrity_first_visit_records_fingerprint', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'first_visit_test_start', test: 'integrity_first_visit_records_fingerprint' }));
    const result = await page.evaluate(async () => {
      // @ts-expect-error injected
      return await verifyTofu('aabbccdd11223344', 'tofu_test_key_1');
    });
    expect(result).toMatchObject({ valid: true, isFirstVisit: true });

    const stored = await page.evaluate(() => localStorage.getItem('tofu_test_key_1'));
    expect(stored).toBe('aabbccdd11223344');
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'first_visit_test_pass', stored }));
  });

  test('integrity_subsequent_same_fingerprint_passes', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'same_fingerprint_test_start' }));
    await page.evaluate(() => localStorage.setItem('tofu_test_key_2', 'AAAA1111'));
    const result = await page.evaluate(async () => {
      // @ts-expect-error injected
      return await verifyTofu('AAAA1111', 'tofu_test_key_2');
    });
    expect(result).toMatchObject({ valid: true, isFirstVisit: false });

    // localStorage must still contain the original.
    const stored = await page.evaluate(() => localStorage.getItem('tofu_test_key_2'));
    expect(stored).toBe('AAAA1111');
  });

  test('integrity_tofu_violation_detected', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'violation_test_start' }));
    await page.evaluate(() => localStorage.setItem('tofu_test_key_3', 'OLD_FP_AAAA'));
    const result = await page.evaluate(async () => {
      // @ts-expect-error injected
      return await verifyTofu('NEW_FP_BBBB', 'tofu_test_key_3');
    });
    expect(result.valid).toBe(false);
    expect(result.reason).toBe('TOFU_VIOLATION');
    expect(result.previousFingerprint).toBe('OLD_FP_AAAA');
    expect(result.currentFingerprint).toBe('NEW_FP_BBBB');

    // Storage must NOT be silently overwritten.
    const stored = await page.evaluate(() => localStorage.getItem('tofu_test_key_3'));
    expect(stored).toBe('OLD_FP_AAAA');
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'violation_test_pass', previous: result.previousFingerprint, current: result.currentFingerprint }));
  });

  test('integrity_operator_can_clear_recorded_fingerprint', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'rotation_recovery_test_start' }));
    // Step 1: record a fingerprint.
    await page.evaluate(async () => {
      // @ts-expect-error injected
      return await verifyTofu('FP_INITIAL', 'tofu_test_key_4');
    });

    // Step 2: clear it (simulating operator action).
    await page.evaluate(() => localStorage.removeItem('tofu_test_key_4'));

    // Step 3: validate a new fingerprint — should be treated as first visit.
    const result = await page.evaluate(async () => {
      // @ts-expect-error injected
      return await verifyTofu('FP_ROTATED', 'tofu_test_key_4');
    });
    expect(result).toMatchObject({ valid: true, isFirstVisit: true });
    const stored = await page.evaluate(() => localStorage.getItem('tofu_test_key_4'));
    expect(stored).toBe('FP_ROTATED');
  });

  test('integrity_works_when_localstorage_disabled', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'localstorage_disabled_test_start' }));
    // Simulate Safari private mode by replacing localStorage with a stub
    // that throws on any access.
    await page.evaluate(() => {
      const throwAlways = () => {
        throw new DOMException('SecurityError', 'SecurityError');
      };
      Object.defineProperty(window, 'localStorage', {
        configurable: true,
        get: throwAlways,
      });
    });

    const result = await page.evaluate(async () => {
      try {
        // @ts-expect-error injected
        return await verifyTofu('FP_PRIVATE', 'tofu_test_key_5');
      } catch (e) {
        return { caught: String(e) };
      }
    });

    // Per the contract: function does NOT throw; returns a permissive result.
    expect(result).toMatchObject({ valid: true, isFirstVisit: true });
  });

  test('integrity_distinct_keys_isolate_archives', async ({ page }) => {
    // eslint-disable-next-line no-console
    console.info(JSON.stringify({ event: 'archive_isolation_test_start' }));
    // Different storage keys → different fingerprints don't collide.
    await page.evaluate(async () => {
      // @ts-expect-error injected
      await verifyTofu('FP_ARCHIVE_A', 'tofu_archive_a');
      // @ts-expect-error injected
      await verifyTofu('FP_ARCHIVE_B', 'tofu_archive_b');
    });
    const a = await page.evaluate(() => localStorage.getItem('tofu_archive_a'));
    const b = await page.evaluate(() => localStorage.getItem('tofu_archive_b'));
    expect(a).toBe('FP_ARCHIVE_A');
    expect(b).toBe('FP_ARCHIVE_B');
  });
});
