// Placeholder COI detector.
// This keeps the bundle build green until the upstream asset is provided.
(() => {
  try {
    if (typeof window === 'undefined') {
      return;
    }

    if (window.crossOriginIsolated) {
      return;
    }

    const banner = document.querySelector('.coi-degraded-banner');
    if (banner) {
      banner.classList.remove('hidden');
    }
  } catch (err) {
    // No-op; avoid breaking the page if detection fails.
    console.warn('coi-detector failed', err);
  }
})();
