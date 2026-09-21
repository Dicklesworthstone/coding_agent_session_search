/**
 * Performance Assertions
 *
 * Validates P6.3 exit criteria targets.
 */

const TARGETS = {
  // Lighthouse
  lighthouseScore: 85,
  firstContentfulPaint: 2000, // ms

  // Decryption
  argon2Desktop: 3000, // ms
  argon2Mobile: 9000, // ms
  dbDecryptPer10MB: 2000, // ms

  // Search
  searchLatency: 100, // ms

  // Memory
  memoryLeakThreshold: 10 * 1024 * 1024, // 10 MB

  // Scroll
  scrollFps: 60,
  scrollFpsMinimum: 55, // Allow 5 FPS variance

  // Total blocking time
  totalBlockingTime: 300, // ms
};

function assertLighthouse(metrics) {
  const failures = [];

  if (metrics.error) {
    failures.push(`Lighthouse error: ${metrics.error}`);
    return { pass: false, failures };
  }

  if (metrics.performanceScore < TARGETS.lighthouseScore) {
    failures.push(`Performance score ${metrics.performanceScore} < ${TARGETS.lighthouseScore}`);
  }

  if (metrics.fcp > TARGETS.firstContentfulPaint) {
    failures.push(`FCP ${metrics.fcp}ms > ${TARGETS.firstContentfulPaint}ms`);
  }

  if (metrics.tbt > TARGETS.totalBlockingTime) {
    failures.push(`Total Blocking Time ${metrics.tbt}ms > ${TARGETS.totalBlockingTime}ms`);
  }

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      performanceScore: metrics.performanceScore,
      fcp: metrics.fcp,
      lcp: metrics.lcp,
      tti: metrics.tti,
      tbt: metrics.tbt,
    },
  };
}

function assertDecrypt(metrics, isMobile = false) {
  const failures = [];

  if (!metrics || metrics.total_ms === null) {
    failures.push("Decryption timing not available");
    return { pass: false, failures };
  }

  const argonTime =
    metrics.timings.argon_start !== undefined && metrics.timings.unwrap_start !== undefined
      ? metrics.timings.unwrap_start - metrics.timings.argon_start
      : null;

  const target = isMobile ? TARGETS.argon2Mobile : TARGETS.argon2Desktop;

  if (argonTime !== null && argonTime > target) {
    failures.push(
      `Argon2 derivation ${argonTime}ms > ${target}ms (${isMobile ? "mobile" : "desktop"})`,
    );
  }

  if (metrics.total_ms > 30000) {
    failures.push(`Total decrypt time ${metrics.total_ms}ms > 30000ms`);
  }

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      argonTime,
      totalTime: metrics.total_ms,
      target,
    },
  };
}

function assertSearch(results) {
  const failures = [];

  if (!results || !Array.isArray(results)) {
    failures.push("Search results not available");
    return { pass: false, failures };
  }

  const slowQueries = results.filter((r) => r.elapsed_ms > TARGETS.searchLatency);

  if (slowQueries.length > 0) {
    for (const q of slowQueries) {
      failures.push(`Query "${q.query}" took ${q.elapsed_ms}ms > ${TARGETS.searchLatency}ms`);
    }
  }

  const avgLatency =
    results.length > 0 ? results.reduce((sum, r) => sum + r.elapsed_ms, 0) / results.length : 0;

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      totalQueries: results.length,
      slowQueries: slowQueries.length,
      avgLatency: Math.round(avgLatency * 100) / 100,
    },
  };
}

function assertMemory(metrics) {
  const failures = [];

  if (!metrics || metrics.leakBytes === null) {
    failures.push("Memory metrics not available");
    return { pass: false, failures };
  }

  if (metrics.leakBytes > TARGETS.memoryLeakThreshold) {
    failures.push(
      `Memory leak detected: ${(metrics.leakBytes / (1024 * 1024)).toFixed(2)}MB > 10MB`,
    );
  }

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      leakMB: metrics.leakMB,
      baselineMB: metrics.baseline.jsHeapBytes
        ? (metrics.baseline.jsHeapBytes / (1024 * 1024)).toFixed(2)
        : null,
      afterMB: metrics.after.jsHeapBytes
        ? (metrics.after.jsHeapBytes / (1024 * 1024)).toFixed(2)
        : null,
    },
  };
}

function assertScroll(metrics) {
  const failures = [];

  if (!metrics || metrics.error) {
    failures.push(`Scroll test error: ${metrics?.error || "unknown"}`);
    return { pass: false, failures };
  }

  if (metrics.effectiveFps < TARGETS.scrollFpsMinimum) {
    failures.push(`Scroll FPS ${metrics.effectiveFps} < ${TARGETS.scrollFpsMinimum} minimum`);
  }

  if (metrics.verySlowFrames > 0) {
    failures.push(`${metrics.verySlowFrames} frames below 30fps during scroll`);
  }

  if (metrics.longTaskCount > 5) {
    failures.push(`${metrics.longTaskCount} long tasks during scroll (>5)`);
  }

  return {
    pass: failures.length === 0,
    failures,
    metrics: {
      effectiveFps: metrics.effectiveFps,
      avgFrameTime: metrics.avgFrameTime,
      p95FrameTime: metrics.p95FrameTime,
      slowFrames: metrics.slowFrames,
      verySlowFrames: metrics.verySlowFrames,
      longTasks: metrics.longTaskCount,
    },
  };
}

function assertAll(perfData) {
  const results = {
    lighthouse: perfData.lighthouse
      ? assertLighthouse(perfData.lighthouse)
      : { pass: true, skipped: true },
    decrypt: assertDecrypt(perfData.decrypt),
    search: assertSearch(perfData.search),
    memory: assertMemory(perfData.memory),
    scroll: perfData.scroll ? assertScroll(perfData.scroll) : { pass: true, skipped: true },
  };

  const allPassed = Object.values(results).every((r) => r.pass);
  const allFailures = Object.entries(results)
    .filter(([, r]) => !r.pass && !r.skipped)
    .flatMap(([name, r]) => r.failures.map((f) => `[${name}] ${f}`));

  return {
    pass: allPassed,
    results,
    failures: allFailures,
    summary: {
      total: Object.keys(results).length,
      passed: Object.values(results).filter((r) => r.pass).length,
      failed: Object.values(results).filter((r) => !r.pass && !r.skipped).length,
      skipped: Object.values(results).filter((r) => r.skipped).length,
    },
  };
}

module.exports = {
  TARGETS,
  assertLighthouse,
  assertDecrypt,
  assertSearch,
  assertMemory,
  assertScroll,
  assertAll,
};
