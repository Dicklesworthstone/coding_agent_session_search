/**
 * Virtual Scroll Performance Test
 *
 * Measures scroll frame rate and detects long tasks during
 * rapid scrolling through a large result list.
 *
 * Target: 60fps (16.67ms per frame)
 */

async function runScrollPerformance(page, options = {}) {
  const {
    scrollContainer = "#results-container",
    scrollSteps = 100,
    stepDelay = 16, // ~60fps timing
    bufferScroll = true,
  } = options;

  const result = await page.evaluate(
    async ({ container, steps, delay, buffer }) => {
      const el = document.querySelector(container);
      if (!el) {
        return { error: "Scroll container not found", selector: container };
      }

      const scrollHeight = el.scrollHeight;
      const clientHeight = el.clientHeight;
      const scrollableDistance = scrollHeight - clientHeight;

      if (scrollableDistance <= 0) {
        return {
          error: "No scrollable content",
          scrollHeight,
          clientHeight,
        };
      }

      const frameTimes = [];
      const longTasks = [];
      let lastFrameTime = performance.now();

      // Track long tasks during scroll
      const taskObserver = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          longTasks.push({
            name: entry.name,
            duration: entry.duration,
            startTime: entry.startTime,
          });
        }
      });

      try {
        taskObserver.observe({ entryTypes: ["longtask"] });
      } catch (e) {
        // longtask not supported in all browsers
      }

      // Run scroll animation
      for (let i = 0; i <= steps; i++) {
        const targetScroll = (scrollableDistance / steps) * i;

        if (buffer) {
          el.scrollTo({ top: targetScroll, behavior: "instant" });
        } else {
          el.scrollTop = targetScroll;
        }

        await new Promise((resolve) => {
          requestAnimationFrame(() => {
            const now = performance.now();
            const frameTime = now - lastFrameTime;
            frameTimes.push(frameTime);
            lastFrameTime = now;

            setTimeout(resolve, delay);
          });
        });
      }

      taskObserver.disconnect();

      // Calculate metrics
      const validFrames = frameTimes.filter((t) => t > 0);
      const avgFrameTime =
        validFrames.length > 0 ? validFrames.reduce((a, b) => a + b, 0) / validFrames.length : 0;
      const maxFrameTime = validFrames.length > 0 ? Math.max(...validFrames) : 0;
      const minFrameTime = validFrames.length > 0 ? Math.min(...validFrames) : 0;

      // Count frames exceeding 16.67ms (below 60fps)
      const slowFrames = validFrames.filter((t) => t > 16.67).length;
      const verySlowFrames = validFrames.filter((t) => t > 33.33).length; // Below 30fps

      // Calculate effective FPS
      const effectiveFps = avgFrameTime > 0 ? 1000 / avgFrameTime : 0;

      // P95 frame time
      const sortedFrames = [...validFrames].sort((a, b) => a - b);
      const p95Index = Math.floor(sortedFrames.length * 0.95);
      const p95FrameTime = sortedFrames[p95Index] || 0;

      return {
        totalFrames: validFrames.length,
        avgFrameTime: Math.round(avgFrameTime * 100) / 100,
        maxFrameTime: Math.round(maxFrameTime * 100) / 100,
        minFrameTime: Math.round(minFrameTime * 100) / 100,
        p95FrameTime: Math.round(p95FrameTime * 100) / 100,
        effectiveFps: Math.round(effectiveFps * 10) / 10,
        slowFrames,
        verySlowFrames,
        longTaskCount: longTasks.length,
        longTasks: longTasks.slice(0, 10), // First 10 long tasks
        scrollableDistance,
        // Thresholds
        ok: effectiveFps >= 55 && verySlowFrames === 0, // Allow slight variance
        smooth: effectiveFps >= 58 && slowFrames < validFrames.length * 0.05,
      };
    },
    {
      container: scrollContainer,
      steps: scrollSteps,
      delay: stepDelay,
      buffer: bufferScroll,
    },
  );

  return result;
}

/**
 * Run bidirectional scroll test (down then up)
 */
async function runBidirectionalScroll(page, options = {}) {
  const downResult = await runScrollPerformance(page, options);
  if (downResult.error) {
    return { down: downResult, up: null, combined: null };
  }

  // Scroll back up
  const upOptions = { ...options, reverse: true };
  const upResult = await page.evaluate(
    async ({ container, steps, delay }) => {
      const el = document.querySelector(container);
      if (!el) return { error: "Container not found" };

      const scrollHeight = el.scrollHeight;
      const clientHeight = el.clientHeight;
      const scrollableDistance = scrollHeight - clientHeight;

      const frameTimes = [];
      let lastFrameTime = performance.now();

      // Scroll up
      for (let i = steps; i >= 0; i--) {
        const targetScroll = (scrollableDistance / steps) * i;
        el.scrollTo({ top: targetScroll, behavior: "instant" });

        await new Promise((resolve) => {
          requestAnimationFrame(() => {
            const now = performance.now();
            frameTimes.push(now - lastFrameTime);
            lastFrameTime = now;
            setTimeout(resolve, delay);
          });
        });
      }

      const validFrames = frameTimes.filter((t) => t > 0);
      const avgFrameTime =
        validFrames.length > 0 ? validFrames.reduce((a, b) => a + b, 0) / validFrames.length : 0;
      const effectiveFps = avgFrameTime > 0 ? 1000 / avgFrameTime : 0;

      return {
        totalFrames: validFrames.length,
        avgFrameTime: Math.round(avgFrameTime * 100) / 100,
        effectiveFps: Math.round(effectiveFps * 10) / 10,
        ok: effectiveFps >= 55,
      };
    },
    {
      container: options.scrollContainer || "#results-container",
      steps: options.scrollSteps || 100,
      delay: options.stepDelay || 16,
    },
  );

  // Combined metrics
  const combinedFps = (downResult.effectiveFps + (upResult.effectiveFps || 0)) / 2;
  const combined = {
    avgFps: Math.round(combinedFps * 10) / 10,
    ok: downResult.ok && (upResult.ok || upResult.error),
    totalLongTasks: downResult.longTaskCount,
  };

  return { down: downResult, up: upResult, combined };
}

module.exports = { runScrollPerformance, runBidirectionalScroll };
