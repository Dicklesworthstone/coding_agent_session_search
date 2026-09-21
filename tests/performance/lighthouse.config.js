/**
 * Lighthouse configuration for cass Archive Web Viewer
 *
 * Tests both performance and accessibility (WCAG 2.1 AA compliance).
 * Run with: npx lighthouse <url> --config-path=./lighthouse.config.js
 */
module.exports = {
  extends: "lighthouse:default",
  settings: {
    throttlingMethod: "simulate",
    throttling: {
      rttMs: 150,
      throughputKbps: 1600,
      cpuSlowdownMultiplier: 4,
    },
    // Include both performance and accessibility audits
    onlyCategories: ["performance", "accessibility", "best-practices"],
    formFactor: "desktop",
    // Skip audits that require network (we're testing local files)
    skipAudits: ["is-on-https", "redirects-http", "uses-http2"],
  },
  // Custom assertions for CI failures
  assertions: {
    // Accessibility must score at least 90
    "categories:accessibility": ["error", { minScore: 0.9 }],
    // Performance must score at least 85 (NFR-2: <3s initial load on 3G)
    "categories:performance": ["error", { minScore: 0.85 }],
    // Critical accessibility rules must pass
    "color-contrast": "error",
    "document-title": "error",
    "html-has-lang": "error",
    "image-alt": "error",
    label: "error",
    "link-name": "error",
    "button-name": "error",
    "heading-order": "warn",
    bypass: "error", // Skip link
    "focus-visible": "warn",
  },
};
