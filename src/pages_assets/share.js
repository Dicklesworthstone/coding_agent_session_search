/**
 * cass Archive Share Link Generator
 *
 * Generates shareable URLs for conversations, messages, and search queries.
 * Works with hash-based routing for static hosting compatibility.
 */

import {
  buildConversationPath,
  buildSearchPath,
  parseConversationRouteParts,
  splitRouteQuery,
} from "./router.js";

/**
 * Get the base URL (everything before the hash)
 * @returns {string} Base URL
 */
function getBaseUrl() {
  const url = new URL(window.location.href);
  // Remove hash and query params
  url.hash = "";
  url.search = "";
  return url.toString();
}

/**
 * Generate a shareable link for a conversation
 * @param {number|string} conversationId - Conversation ID
 * @param {number|string|null} messageId - Optional message ID to link to
 * @returns {string} Shareable URL
 */
export function getConversationLink(conversationId, messageId = null) {
  const base = getBaseUrl();
  const path = buildConversationPath(conversationId, messageId);
  return `${base}#${path}`;
}

/**
 * Generate a shareable link for a search query
 * @param {string} query - Search query
 * @param {Object} filters - Optional filters (agent, since, until)
 * @returns {string} Shareable URL
 */
export function getSearchLink(query, filters = {}) {
  const base = getBaseUrl();
  const path = buildSearchPath(query, filters);
  return `${base}#${path}`;
}

/**
 * Generate a shareable link for the settings panel
 * @returns {string} Shareable URL
 */
export function getSettingsLink() {
  const base = getBaseUrl();
  return `${base}#/settings`;
}

/**
 * Generate a shareable link for the stats panel
 * @returns {string} Shareable URL
 */
export function getStatsLink() {
  const base = getBaseUrl();
  return `${base}#/stats`;
}

/**
 * Generate a shareable link for the home/search page
 * @returns {string} Shareable URL
 */
export function getHomeLink() {
  const base = getBaseUrl();
  return `${base}#/`;
}

/**
 * Copy text while the originating request is still current. Archive lock is
 * a cancellation boundary even for callers without an explicit AbortSignal.
 * Cancellation prevents NEW clipboard/fallback work; it cannot retract bytes
 * already handed to the browser's non-cancellable writeText() operation.
 * @param {string} text - Text to copy
 * @param {Object} options - Optional signal and synchronous isCurrent predicate
 * @returns {Promise<boolean>} True only for a successful, still-current request
 */
export async function copyTextToClipboard(text, { signal, isCurrent = () => true } = {}) {
  if (typeof text !== "string" || typeof isCurrent !== "function") {
    throw new TypeError("Clipboard copy requires text and a current-request predicate");
  }
  const lockEvents = globalThis.window;
  let textArea = null;
  let cancelled = false;
  const clearTemporaryText = () => {
    if (textArea) {
      textArea.value = "";
      textArea.parentNode?.removeChild(textArea);
    }
  };
  const cancel = () => {
    cancelled = true;
    clearTemporaryText();
  };
  const active = () => !cancelled && !signal?.aborted && isCurrent();

  try {
    lockEvents?.addEventListener("cass:lock", cancel);
    signal?.addEventListener("abort", cancel, { once: true });
    if (!active()) return false;

    const clipboard = globalThis.navigator?.clipboard;
    if (clipboard?.writeText) {
      try {
        await clipboard.writeText(text);
        return active();
      } catch (error) {
        // A permission prompt may outlive navigation, abort or archive lock.
        // Never create a new plaintext textarea or retry copy after that.
        if (!active()) return false;
        console.error("[Share] Failed to copy text via Clipboard API:", error);
      }
    }

    if (!active() || !document.body || typeof document.execCommand !== "function") {
      return false;
    }
    textArea = document.createElement("textarea");
    textArea.value = text;
    textArea.setAttribute("readonly", "");
    textArea.style.position = "fixed";
    textArea.style.left = "-9999px";
    textArea.style.top = "-9999px";
    textArea.style.opacity = "0";
    document.body.appendChild(textArea);
    textArea.focus();
    // Focus/selection handlers may synchronously invalidate the request.
    if (!active()) return false;
    textArea.select();
    if (!active()) return false;
    return Boolean(document.execCommand("copy")) && active();
  } catch (error) {
    console.error("[Share] Clipboard copy failed:", error);
    return false;
  } finally {
    lockEvents?.removeEventListener("cass:lock", cancel);
    signal?.removeEventListener("abort", cancel);
    clearTemporaryText();
  }
}

/**
 * Copy a link to the clipboard
 * @param {string} link - Link to copy
 * @returns {Promise<boolean>} True if successful
 */
export async function copyLinkToClipboard(link) {
  return copyTextToClipboard(link);
}

/**
 * Copy conversation link to clipboard with feedback
 * @param {number|string} conversationId - Conversation ID
 * @param {number|string|null} messageId - Optional message ID
 * @returns {Promise<{success: boolean, link: string}>} Result
 */
export async function copyConversationLink(conversationId, messageId = null) {
  const link = getConversationLink(conversationId, messageId);
  const success = await copyLinkToClipboard(link);
  return { success, link };
}

/**
 * Copy search link to clipboard with feedback
 * @param {string} query - Search query
 * @param {Object} filters - Optional filters
 * @returns {Promise<{success: boolean, link: string}>} Result
 */
export async function copySearchLink(query, filters = {}) {
  const link = getSearchLink(query, filters);
  const success = await copyLinkToClipboard(link);
  return { success, link };
}

/**
 * Share link using Web Share API (if available)
 * @param {Object} options - Share options
 * @param {string} options.title - Share title
 * @param {string} options.text - Share text/description
 * @param {string} options.url - URL to share
 * @returns {Promise<boolean>} True if shared successfully
 */
export async function shareLink(options) {
  if (!navigator.share) {
    console.debug("[Share] Web Share API not available");
    return false;
  }

  try {
    await navigator.share(options);
    return true;
  } catch (error) {
    // User cancelled or share failed
    if (error.name !== "AbortError") {
      console.error("[Share] Share failed:", error);
    }
    return false;
  }
}

/**
 * Share a conversation using Web Share API
 * @param {number|string} conversationId - Conversation ID
 * @param {string} title - Conversation title
 * @param {number|string|null} messageId - Optional message ID
 * @returns {Promise<boolean>} True if shared successfully
 */
export async function shareConversation(conversationId, title, messageId = null) {
  const link = getConversationLink(conversationId, messageId);

  const shareOptions = {
    title: title || "Conversation",
    text: `Check out this conversation${messageId ? " (message #" + messageId + ")" : ""}`,
    url: link,
  };

  return shareLink(shareOptions);
}

/**
 * Check if Web Share API is available
 * @returns {boolean} True if available
 */
export function isWebShareAvailable() {
  return !!navigator.share;
}

/**
 * Parse a share link to extract route info
 * @param {string} link - Share link to parse
 * @returns {Object|null} Parsed route info or null if invalid
 */
export function parseShareLink(link) {
  try {
    const url = new URL(link);
    const hash = url.hash.slice(1); // Remove #

    if (!hash) {
      return { view: "search", params: {}, query: {} };
    }

    const [pathPart, queryPart] = splitRouteQuery(hash);
    const parts = pathPart.split("/").filter(Boolean);

    // Parse query params
    const query = {};
    if (queryPart) {
      const searchParams = new URLSearchParams(queryPart);
      for (const [key, value] of searchParams) {
        query[key] = value;
      }
    }

    // Home/search
    if (parts.length === 0) {
      return { view: "search", params: {}, query };
    }

    if (parts[0] === "search" && parts.length === 1) {
      return { view: "search", params: {}, query };
    }

    // Conversation
    if (parts[0] === "c") {
      const conversationParams = parseConversationRouteParts(parts);
      if (!conversationParams) {
        return null;
      }

      return {
        view: "conversation",
        params: conversationParams,
        query,
      };
    }

    // Settings
    if (parts[0] === "settings" && parts.length === 1) {
      return { view: "settings", params: {}, query };
    }

    // Stats
    if (parts[0] === "stats" && parts.length === 1) {
      return { view: "stats", params: {}, query };
    }

    return null;
  } catch (error) {
    console.error("[Share] Failed to parse link:", error);
    return null;
  }
}

// Export default
export default {
  getConversationLink,
  getSearchLink,
  getSettingsLink,
  getStatsLink,
  getHomeLink,
  copyTextToClipboard,
  copyLinkToClipboard,
  copyConversationLink,
  copySearchLink,
  shareLink,
  shareConversation,
  isWebShareAvailable,
  parseShareLink,
};
