/**
 * Mobile Decryption Performance Test
 *
 * Tests Argon2 key derivation performance under mobile
 * CPU throttling conditions.
 *
 * Target: <9 seconds for Argon2 derivation on mobile
 */

const { runDecryptTiming } = require("./decrypt-timing.test");

const MOBILE_CPU_SLOWDOWN = 4; // 4x CPU slowdown simulates mid-tier mobile

/**
 * Run decryption timing with mobile CPU throttling via CDP
 */
async function runMobileDecryptTiming(page, password) {
  const client = await page.context().newCDPSession(page);

  // Enable CPU throttling
  await client.send("Emulation.setCPUThrottlingRate", {
    rate: MOBILE_CPU_SLOWDOWN,
  });

  try {
    const result = await runDecryptTiming(page, password);
    return {
      ...result,
      mobileEmulation: true,
      cpuSlowdown: MOBILE_CPU_SLOWDOWN,
    };
  } finally {
    // Disable throttling
    await client.send("Emulation.setCPUThrottlingRate", { rate: 1 });
    await client.detach();
  }
}

/**
 * Create a mobile viewport context
 */
function getMobileViewport() {
  return {
    width: 375,
    height: 667,
    deviceScaleFactor: 2,
    isMobile: true,
    hasTouch: true,
  };
}

/**
 * Common mobile device configurations
 */
const MOBILE_DEVICES = {
  iphone12: {
    viewport: { width: 390, height: 844, deviceScaleFactor: 3, isMobile: true, hasTouch: true },
    userAgent:
      "Mozilla/5.0 (iPhone; CPU iPhone OS 14_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.0 Mobile/15E148 Safari/604.1",
    cpuSlowdown: 4,
  },
  pixel5: {
    viewport: { width: 393, height: 851, deviceScaleFactor: 2.75, isMobile: true, hasTouch: true },
    userAgent:
      "Mozilla/5.0 (Linux; Android 11; Pixel 5) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/90.0.4430.91 Mobile Safari/537.36",
    cpuSlowdown: 4,
  },
  galaxyS21: {
    viewport: { width: 360, height: 800, deviceScaleFactor: 3, isMobile: true, hasTouch: true },
    userAgent:
      "Mozilla/5.0 (Linux; Android 11; SM-G991B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/90.0.4430.91 Mobile Safari/537.36",
    cpuSlowdown: 4,
  },
  lowEndAndroid: {
    viewport: { width: 320, height: 568, deviceScaleFactor: 2, isMobile: true, hasTouch: true },
    userAgent:
      "Mozilla/5.0 (Linux; Android 8.0; Generic) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/80.0.3987.87 Mobile Safari/537.36",
    cpuSlowdown: 6, // Lower-end device, more slowdown
  },
};

/**
 * Run decrypt timing with specific device emulation
 */
async function runDeviceDecryptTiming(browser, url, password, deviceConfig) {
  const context = await browser.newContext({
    viewport: deviceConfig.viewport,
    userAgent: deviceConfig.userAgent,
    isMobile: true,
    hasTouch: true,
  });

  const page = await context.newPage();

  // Navigate to auth page
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#password", { timeout: 15_000 });

  // Apply CPU throttling
  const client = await context.newCDPSession(page);
  await client.send("Emulation.setCPUThrottlingRate", {
    rate: deviceConfig.cpuSlowdown,
  });

  try {
    const result = await runDecryptTiming(page, password);
    return {
      ...result,
      device: deviceConfig,
      mobileEmulation: true,
    };
  } finally {
    await client.send("Emulation.setCPUThrottlingRate", { rate: 1 });
    await client.detach();
    await context.close();
  }
}

/**
 * Assert mobile decryption targets
 */
function assertMobileDecrypt(metrics) {
  const TARGET_ARGON2_MOBILE_MS = 9000;
  const failures = [];

  if (!metrics || metrics.total_ms === null) {
    return { pass: false, failures: ["Mobile decryption timing not available"] };
  }

  const argonTime =
    metrics.timings.argon_start !== undefined && metrics.timings.unwrap_start !== undefined
      ? metrics.timings.unwrap_start - metrics.timings.argon_start
      : null;

  if (argonTime !== null && argonTime > TARGET_ARGON2_MOBILE_MS) {
    failures.push(
      `Mobile Argon2 derivation ${argonTime}ms exceeds ${TARGET_ARGON2_MOBILE_MS}ms target`,
    );
  }

  // Total decrypt should complete in reasonable time
  if (metrics.total_ms > 30000) {
    failures.push(`Total mobile decrypt ${metrics.total_ms}ms > 30s`);
  }

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      argonTime,
      totalTime: metrics.total_ms,
      target: TARGET_ARGON2_MOBILE_MS,
      cpuSlowdown: metrics.cpuSlowdown,
    },
  };
}

module.exports = {
  runMobileDecryptTiming,
  runDeviceDecryptTiming,
  getMobileViewport,
  MOBILE_DEVICES,
  assertMobileDecrypt,
};
