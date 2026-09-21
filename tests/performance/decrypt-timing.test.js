async function runDecryptTiming(page, password) {
  const timings = await page.evaluate((pwd) => {
    return new Promise((resolve, reject) => {
      const progressEl = document.querySelector("#auth-progress .progress-text");
      const input = document.getElementById("password");
      const unlock = document.getElementById("unlock-btn");

      if (!input || !unlock) {
        reject(new Error("Auth elements not found"));
        return;
      }

      const marks = {
        start: performance.now(),
      };

      const record = (key) => {
        if (marks[key] === undefined) {
          marks[key] = performance.now();
        }
      };

      const observer = progressEl
        ? new MutationObserver(() => {
            const text = progressEl.textContent || "";
            if (text.includes("Deriving key")) {
              record("argon_start");
            }
            if (text.includes("Unwrapping key")) {
              record("unwrap_start");
            }
            if (text.startsWith("Decrypting")) {
              record("decrypt_start");
            }
            if (text.includes("Decompressing")) {
              record("decompress_start");
            }
            if (text.includes("Loading database")) {
              record("db_load_start");
            }
          })
        : null;

      if (observer && progressEl) {
        observer.observe(progressEl, { childList: true, subtree: true, characterData: true });
      }

      window.addEventListener(
        "cass:db-ready",
        () => {
          record("db_ready");
          if (observer) {
            observer.disconnect();
          }
          resolve(marks);
        },
        { once: true },
      );

      input.value = pwd;
      unlock.click();
    });
  }, password);

  const total = timings.db_ready !== undefined ? timings.db_ready - timings.start : null;
  return {
    timings,
    total_ms: total,
  };
}

module.exports = { runDecryptTiming };
