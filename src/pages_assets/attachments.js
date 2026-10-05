/**
 * cass Archive Attachment Manager
 *
 * Handles lazy loading and decryption of attachments stored in the blobs/ directory.
 * Each blob is encrypted with AES-256-GCM using unique nonces derived from the blob hash.
 *
 * Security notes:
 * - Blobs are fetched on-demand to minimize memory usage
 * - Decrypted data is cached with configurable limits
 * - Object URLs are revoked when cache entries are evicted
 */

// Domain separator for HKDF nonce derivation (must match Rust)
const BLOB_NONCE_DOMAIN = "cass-blob-nonce-v1";

// Cache configuration
const CACHE_CONFIG = {
  MAX_ENTRIES: 50, // Maximum cached blobs
  MAX_SIZE_BYTES: 50 * 1024 * 1024, // 50 MB max cache size
};

// Bound admission independently of the retained cache: two fetch/decrypt jobs,
// at most 64 queued requests, and a finite lifetime for each admitted job.
// Limits cover encoded bytes, not image decoding, Blob/URL copies, or WASM.
const LOAD_CONFIG = {
  MAX_CONCURRENT: 2,
  MAX_PENDING: 64,
  MAX_MANIFEST_BYTES: 8 * 1024 * 1024,
  TIMEOUT_MS: 120_000,
};
const AES_GCM_TAG_BYTES = 16;
const activeLoads = new Set();
const pendingLoads = [];
const attachmentObservers = new Set();

function ensureActiveLoad(epoch, signal) {
  if (!isCurrentEpoch(epoch)) {
    throw createInvalidationError();
  }
  if (signal.aborted) {
    throw signal.reason;
  }
}

function drainAttachmentLoads() {
  while (activeLoads.size < LOAD_CONFIG.MAX_CONCURRENT && pendingLoads.length) {
    pendingLoads.shift().start();
  }
}

function withAttachmentLoad(epoch, task) {
  return new Promise((resolve, reject) => {
    if (!isCurrentEpoch(epoch)) {
      reject(createInvalidationError());
      return;
    }
    const start = () => {
      const controller = new AbortController();
      activeLoads.add(controller);
      const timeout = setTimeout(() => controller.abort(createAttachmentError(
        "Attachment request timed out; try again", "ATTACHMENT_REQUEST_TIMED_OUT",
      )), LOAD_CONFIG.TIMEOUT_MS);
      Promise.resolve().then(() => {
        ensureActiveLoad(epoch, controller.signal);
        return task(controller.signal);
      }).then(resolve, (error) => {
        reject(controller.signal.aborted ? controller.signal.reason : error);
      }).finally(() => {
        clearTimeout(timeout);
        controller.abort();
        // Do not release admission on abort alone: WebCrypto cannot be
        // interrupted, and must settle before another large job starts.
        activeLoads.delete(controller);
        drainAttachmentLoads();
      });
    };
    if (activeLoads.size < LOAD_CONFIG.MAX_CONCURRENT) {
      start();
    } else if (pendingLoads.length < LOAD_CONFIG.MAX_PENDING) {
      pendingLoads.push({ start, reject });
    } else {
      reject(createAttachmentError(
        "Too many attachment requests; try again after current loads finish",
        "ATTACHMENT_LOAD_QUEUE_FULL",
      ));
    }
  });
}

async function readAttachmentCiphertext(response, limit, epoch, signal) {
  const tooLarge = () => createAttachmentError(
    `Attachment exceeds the browser loading limit of ${limit - AES_GCM_TAG_BYTES} bytes`,
    "ATTACHMENT_TOO_LARGE",
  );
  const declared = response.headers?.get("content-length");
  if (declared && /^\d+$/.test(declared) && Number(declared) > limit) {
    void response.body?.cancel().catch(() => {});
    throw tooLarge();
  }
  ensureActiveLoad(epoch, signal);
  // Fetch responses in supported browsers expose a stream. Keep compatibility
  // with non-streaming embedders/test transports; that fallback can check the
  // size only AFTER their arrayBuffer allocation, not bound that allocation.
  if (!response.body?.getReader) {
    const bytes = new Uint8Array(await response.arrayBuffer());
    ensureActiveLoad(epoch, signal);
    if (bytes.byteLength > limit) throw tooLarge();
    return bytes;
  }
  const reader = response.body.getReader();
  const cancel = () => { void reader.cancel().catch(() => {}); };
  signal.addEventListener("abort", cancel, { once: true });
  let buffer = new Uint8Array(0);
  let length = 0;
  let complete = false;
  try {
    while (true) {
      ensureActiveLoad(epoch, signal);
      const { done, value } = await reader.read();
      ensureActiveLoad(epoch, signal);
      if (done) {
        complete = true;
        return buffer.subarray(0, length);
      }
      if (value.byteLength > limit - length) throw tooLarge();
      const needed = length + value.byteLength;
      if (needed > buffer.byteLength) {
        // A growable byte buffer avoids retaining one object per network
        // fragment (an adversarial stream could deliver millions of bytes
        // one at a time). Never grow past the actual per-response ceiling.
        const grown = new Uint8Array(Math.min(limit, Math.max(needed, 65536, buffer.length * 2)));
        grown.set(buffer.subarray(0, length));
        buffer = grown;
      }
      buffer.set(value, length);
      length = needed;
    }
  } finally {
    signal.removeEventListener("abort", cancel);
    if (!complete) cancel();
    reader.releaseLock();
  }
}

// Module state
let manifest = null;
let isManifestLoaded = false;
let manifestLoadPromise = null;
let manifestLoadEpoch = 0;
let attachmentEpoch = 0;

// Blob cache: hash -> { data: Uint8Array, objectUrls: Map<string, string>, size: number }
const blobCache = new Map();
const blobLoadPromises = new Map();
const blobUrlPromises = new Map();
let cacheSize = 0;

// LRU tracking
const lruOrder = [];

function isCurrentEpoch(epoch) {
  return epoch === attachmentEpoch;
}

function createAttachmentError(message, code) {
  const error = new Error(message);
  error.code = code;
  return error;
}

function attachmentFailureMessage(error, fallback) {
  switch (error?.code) {
    case "ATTACHMENT_TOO_LARGE":
    case "ATTACHMENT_LOAD_QUEUE_FULL":
    case "ATTACHMENT_REQUEST_TIMED_OUT":
      return error.message;
    default:
      return fallback;
  }
}

function createInvalidationError() {
  return createAttachmentError("Attachment request invalidated", "ATTACHMENT_REQUEST_INVALIDATED");
}

function shouldCacheManifestAbsence(error) {
  return error?.code === "ATTACHMENT_MANIFEST_ABSENT";
}

/**
 * Initialize the attachment system
 * Fetches and decrypts the manifest if attachments are present
 *
 * @param {Uint8Array} dek - Data encryption key
 * @param {Uint8Array} exportId - Export ID bytes
 * @returns {Promise<object|null>} Manifest or null if no attachments
 */
export async function initAttachments(dek, exportId) {
  const epoch = attachmentEpoch;

  if (isManifestLoaded) {
    return manifest;
  }

  if (manifestLoadPromise && manifestLoadEpoch === epoch) {
    return manifestLoadPromise;
  }

  manifestLoadEpoch = epoch;
  manifestLoadPromise = (async () => {
    try {
      const loadedManifest = await loadManifest(dek, exportId, epoch);
      if (!isCurrentEpoch(epoch)) {
        throw createInvalidationError();
      }
      manifest = loadedManifest;
      isManifestLoaded = true;
      return manifest;
    } catch (error) {
      if (!isCurrentEpoch(epoch)) throw createInvalidationError();
      if (error?.code !== "ATTACHMENT_REQUEST_INVALIDATED") {
        console.warn("[Attachments] No attachments found or manifest failed:", error.message);
      }
      if (isCurrentEpoch(epoch)) {
        manifest = null;
        isManifestLoaded = shouldCacheManifestAbsence(error);
      }
      if (shouldCacheManifestAbsence(error)) {
        return null;
      }
      throw error;
    } finally {
      if (manifestLoadEpoch === epoch) {
        manifestLoadPromise = null;
      }
    }
  })();

  return manifestLoadPromise;
}

/**
 * Load and decrypt the manifest
 */
async function loadManifest(dek, exportId, epoch) {
  return withAttachmentLoad(epoch, async (signal) => {
    const response = await fetch("./blobs/manifest.enc", { signal });
    if (!response.ok) {
      if (response.status === 404) {
        throw createAttachmentError("Manifest not found", "ATTACHMENT_MANIFEST_ABSENT");
      }
      throw createAttachmentError(
        `Failed to load attachment manifest: ${response.status}`,
        "ATTACHMENT_MANIFEST_FETCH_FAILED",
      );
    }

    const ciphertext = await readAttachmentCiphertext(
      response, LOAD_CONFIG.MAX_MANIFEST_BYTES + AES_GCM_TAG_BYTES, epoch, signal,
    );

    // Derive nonce using HKDF
    const nonce = await deriveBlobNonce("manifest");

    // Import DEK for decryption
    const dekKey = await crypto.subtle.importKey("raw", dek, { name: "AES-GCM" }, false, ["decrypt"]);

    ensureActiveLoad(epoch, signal);

    // Decrypt with AAD = export_id only
    const plaintext = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: nonce,
        additionalData: exportId,
      },
      dekKey,
      ciphertext,
    );

    try {
      ensureActiveLoad(epoch, signal);

      // Parse JSON manifest
      const decoder = new TextDecoder();
      const manifestJson = decoder.decode(plaintext);
      let parsedManifest;
      try {
        parsedManifest = JSON.parse(manifestJson);
      } catch (error) {
        throw createAttachmentError(
          `Invalid attachment manifest JSON: ${error.message}`,
          "ATTACHMENT_MANIFEST_INVALID",
        );
      }
      return validateManifest(parsedManifest);
    } finally {
      new Uint8Array(plaintext).fill(0);
    }
  });
}

/**
 * Check if attachments are available
 * @returns {boolean}
 */
export function hasAttachments() {
  return manifest !== null && manifest.entries?.length > 0;
}

/**
 * Get manifest information
 * @returns {object|null}
 */
export function getManifest() {
  return manifest;
}

/**
 * Get attachments for a specific message
 * @param {number} messageId - Message ID
 * @returns {Array} Attachment entries for this message
 */
export function getMessageAttachments(messageId) {
  if (!manifest?.entries) {
    return [];
  }
  return manifest.entries.filter((entry) => entry.message_id === messageId);
}

/**
 * Load and decrypt a blob by hash
 *
 * @param {string} hash - SHA-256 hash (hex)
 * @param {Uint8Array} dek - Data encryption key
 * @param {Uint8Array} exportId - Export ID bytes
 * @returns {Promise<Uint8Array>} Decrypted blob data
 */
export async function loadBlob(hash, dek, exportId) {
  const epoch = attachmentEpoch;
  const normalizedHash = normalizeBlobHash(hash);

  // Check cache
  const cached = blobCache.get(normalizedHash);
  if (cached) {
    updateLru(normalizedHash);
    return cached.data;
  }

  const inFlight = blobLoadPromises.get(normalizedHash);
  if (inFlight?.epoch === epoch) {
    return inFlight.promise;
  }

  let loadPromise;
  loadPromise = withAttachmentLoad(epoch, async (signal) => {
    // Fetch encrypted blob
    const response = await fetch(`./blobs/${normalizedHash}.bin`, { signal });
    if (!response.ok) {
      throw new Error(`Blob not found: ${normalizedHash}`);
    }

    const ciphertext = await readAttachmentCiphertext(
      response, CACHE_CONFIG.MAX_SIZE_BYTES + AES_GCM_TAG_BYTES, epoch, signal,
    );

    // Derive nonce using HKDF
    const nonce = await deriveBlobNonce(normalizedHash);

    // Import DEK for decryption
    const dekKey = await crypto.subtle.importKey("raw", dek, { name: "AES-GCM" }, false, [
      "decrypt",
    ]);

    // Build AAD: export_id || hash_bytes
    const hashBytes = hexToBytes(normalizedHash);
    const aad = new Uint8Array(exportId.length + hashBytes.length);
    aad.set(exportId);
    aad.set(hashBytes, exportId.length);

    ensureActiveLoad(epoch, signal);

    // Decrypt
    const plaintext = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: nonce,
        additionalData: aad,
      },
      dekKey,
      ciphertext,
    );

    const data = new Uint8Array(plaintext);

    try {
      ensureActiveLoad(epoch, signal);
      // Cache the result only after both authentication and admission checks.
      cacheBlob(normalizedHash, data);
    } catch (error) {
      data.fill(0);
      throw error;
    }

    return data;
  }).finally(() => {
    const current = blobLoadPromises.get(normalizedHash);
    if (current?.epoch === epoch && current.promise === loadPromise) {
      blobLoadPromises.delete(normalizedHash);
    }
  });

  blobLoadPromises.set(normalizedHash, { epoch, promise: loadPromise });
  return loadPromise;
}

/**
 * Load a blob and return as an object URL for display
 *
 * @param {string} hash - SHA-256 hash (hex)
 * @param {string} mimeType - MIME type for the blob
 * @param {Uint8Array} dek - Data encryption key
 * @param {Uint8Array} exportId - Export ID bytes
 * @returns {Promise<string>} Object URL
 */
export async function loadBlobAsUrl(hash, mimeType, dek, exportId) {
  const epoch = attachmentEpoch;
  const normalizedHash = normalizeBlobHash(hash);
  const normalizedMimeType = normalizeAttachmentMimeType(mimeType);
  const urlCacheKey = blobUrlCacheKey(normalizedHash, normalizedMimeType);

  // Check if we already have an object URL
  const cached = blobCache.get(normalizedHash);
  const cachedUrl = cached?.objectUrls.get(normalizedMimeType);
  if (cachedUrl) {
    updateLru(normalizedHash);
    return cachedUrl;
  }

  const inFlight = blobUrlPromises.get(urlCacheKey);
  if (inFlight?.epoch === epoch) {
    return inFlight.promise;
  }

  let urlPromise;
  urlPromise = (async () => {
    // Load the blob data
    const data = await loadBlob(normalizedHash, dek, exportId);

    const cachedEntry = blobCache.get(normalizedHash);
    const existingUrl = cachedEntry?.objectUrls.get(normalizedMimeType);
    if (existingUrl) {
      updateLru(normalizedHash);
      return existingUrl;
    }

    // Create object URL
    const blob = new Blob([data], { type: normalizedMimeType });
    const url = URL.createObjectURL(blob);

    if (!isCurrentEpoch(epoch)) {
      URL.revokeObjectURL(url);
      throw createInvalidationError();
    }

    const cacheEntry = blobCache.get(normalizedHash);
    if (!cacheEntry) {
      URL.revokeObjectURL(url);
      throw createAttachmentError(
        "Attachment cache entry missing after blob load",
        "ATTACHMENT_CACHE_INCONSISTENT",
      );
    }

    cacheEntry.objectUrls.set(normalizedMimeType, url);
    updateLru(normalizedHash);
    return url;
  })().finally(() => {
    const current = blobUrlPromises.get(urlCacheKey);
    if (current?.epoch === epoch && current.promise === urlPromise) {
      blobUrlPromises.delete(urlCacheKey);
    }
  });

  blobUrlPromises.set(urlCacheKey, { epoch, promise: urlPromise });
  return urlPromise;
}

/**
 * Derive a 12-byte nonce from an identifier using HKDF-SHA256
 *
 * Must match Rust's derive_blob_nonce function:
 * - salt: BLOB_NONCE_DOMAIN ("cass-blob-nonce-v1")
 * - ikm: identifier bytes
 * - info: "nonce"
 * - output: 12 bytes
 */
async function deriveBlobNonce(identifier) {
  const encoder = new TextEncoder();
  const salt = encoder.encode(BLOB_NONCE_DOMAIN);
  const ikm = encoder.encode(identifier);
  const info = encoder.encode("nonce");

  // Import IKM as HKDF key material
  const baseKey = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);

  // Derive 96 bits (12 bytes) using HKDF
  const nonceBits = await crypto.subtle.deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: salt,
      info: info,
    },
    baseKey,
    96, // 12 bytes * 8 bits
  );

  return new Uint8Array(nonceBits);
}

/**
 * Convert hex string to Uint8Array
 */
function hexToBytes(hex) {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return bytes;
}

function normalizeBlobHash(hash) {
  if (typeof hash !== "string") {
    throw new Error("Attachment hash must be a string");
  }

  const normalized = hash.trim().toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(normalized)) {
    throw new Error("Attachment hash must be 64 hex characters");
  }

  return normalized;
}

function normalizeAttachmentMimeType(mimeType) {
  const normalized = typeof mimeType === "string" ? mimeType.trim() : String(mimeType ?? "").trim();

  if (!normalized || /[\0\r\n]/.test(normalized)) {
    throw new Error("Attachment MIME type must be non-empty without control characters");
  }

  return normalized;
}

function blobUrlCacheKey(hash, mimeType) {
  return `${hash}\0${mimeType}`;
}

function validateManifest(rawManifest) {
  if (!rawManifest || typeof rawManifest !== "object" || Array.isArray(rawManifest)) {
    throw new Error("Attachment manifest must be an object");
  }
  if (!Array.isArray(rawManifest.entries)) {
    throw new Error("Attachment manifest entries must be an array");
  }
  if (
    rawManifest.total_size_bytes !== null &&
    rawManifest.total_size_bytes !== undefined &&
    (!Number.isSafeInteger(rawManifest.total_size_bytes) || rawManifest.total_size_bytes < 0)
  ) {
    throw new Error("Attachment manifest total_size_bytes must be a non-negative integer");
  }

  return {
    ...rawManifest,
    entries: rawManifest.entries.map((entry, index) => validateManifestEntry(entry, index)),
  };
}

function validateManifestEntry(entry, index) {
  if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
    throw new Error(`Attachment entry ${index} must be an object`);
  }
  if (
    typeof entry.filename !== "string" ||
    entry.filename.length === 0 ||
    entry.filename.includes("\0")
  ) {
    throw new Error(`Attachment entry ${index} has an invalid filename`);
  }
  if (
    typeof entry.mime_type !== "string" ||
    entry.mime_type.trim().length === 0 ||
    /[\0\r\n]/.test(entry.mime_type)
  ) {
    throw new Error(`Attachment entry ${index} has an invalid MIME type`);
  }
  if (!Number.isSafeInteger(entry.size_bytes) || entry.size_bytes < 0) {
    throw new Error(`Attachment entry ${index} has an invalid size`);
  }
  if (!Number.isSafeInteger(entry.message_id) || entry.message_id < 0) {
    throw new Error(`Attachment entry ${index} has an invalid message ID`);
  }

  return {
    ...entry,
    hash: normalizeBlobHash(entry.hash),
    mime_type: entry.mime_type.trim(),
  };
}

/**
 * Cache a blob with LRU eviction
 */
function cacheBlob(hash, data) {
  if (data.byteLength > CACHE_CONFIG.MAX_SIZE_BYTES) {
    throw createAttachmentError("Attachment exceeds the browser cache limit", "ATTACHMENT_TOO_LARGE");
  }
  if (blobCache.has(hash)) {
    updateLru(hash);
    return;
  }

  // Check if we need to evict
  while (
    blobCache.size >= CACHE_CONFIG.MAX_ENTRIES ||
    cacheSize + data.length > CACHE_CONFIG.MAX_SIZE_BYTES
  ) {
    if (lruOrder.length === 0) break;
    evictOldest();
  }

  // Add to cache
  blobCache.set(hash, {
    data,
    objectUrls: new Map(),
    size: data.length,
  });
  cacheSize += data.length;
  lruOrder.push(hash);
}

/**
 * Update LRU order for a hash
 */
function updateLru(hash) {
  const idx = lruOrder.indexOf(hash);
  if (idx > -1) {
    lruOrder.splice(idx, 1);
    lruOrder.push(hash);
  }
}

/**
 * Evict the oldest cache entry
 */
function evictOldest() {
  const hash = lruOrder.shift();
  if (!hash) return;

  const entry = blobCache.get(hash);
  if (entry) {
    // Revoke object URLs if present
    for (const objectUrl of entry.objectUrls.values()) {
      URL.revokeObjectURL(objectUrl);
    }
    cacheSize -= entry.size;
    blobCache.delete(hash);
  }
}

/**
 * Clear the blob cache
 */
export function clearCache() {
  for (const entry of blobCache.values()) {
    for (const objectUrl of entry.objectUrls.values()) {
      URL.revokeObjectURL(objectUrl);
    }
  }
  blobCache.clear();
  lruOrder.length = 0;
  cacheSize = 0;
}

/**
 * Reset the attachment system (for re-auth)
 */
export function reset() {
  attachmentEpoch += 1;
  const error = createInvalidationError();
  for (const pending of pendingLoads.splice(0)) pending.reject(error);
  for (const controller of activeLoads) controller.abort(error);
  for (const observer of attachmentObservers) observer.disconnect();
  attachmentObservers.clear();
  for (const entry of blobCache.values()) entry.data.fill(0);
  clearCache();
  manifest = null;
  isManifestLoaded = false;
  manifestLoadPromise = null;
  blobLoadPromises.clear();
  blobUrlPromises.clear();
  manifestLoadEpoch = attachmentEpoch;
}

/**
 * Get cache statistics
 * @returns {object} Cache stats
 */
export function getCacheStats() {
  return {
    entries: blobCache.size,
    sizeBytes: cacheSize,
    maxEntries: CACHE_CONFIG.MAX_ENTRIES,
    maxSizeBytes: CACHE_CONFIG.MAX_SIZE_BYTES,
  };
}

/**
 * Render an attachment element for display
 *
 * @param {object} entry - Attachment entry from manifest
 * @param {Uint8Array} dek - Data encryption key
 * @param {Uint8Array} exportId - Export ID bytes
 * @returns {HTMLElement} DOM element for the attachment
 */
export function createAttachmentElement(entry, dek, exportId) {
  const container = document.createElement("div");
  container.className = "attachment";
  container.dataset.hash = entry.hash;
  container.dataset.mimeType = entry.mime_type;

  // Determine type and create appropriate element
  if (entry.mime_type.startsWith("image/")) {
    return createImageAttachment(entry, dek, exportId);
  } else if (entry.mime_type === "application/pdf") {
    return createPdfAttachment(entry, dek, exportId);
  } else {
    return createDownloadAttachment(entry, dek, exportId);
  }
}

/**
 * Create an image attachment element with lazy loading
 */
function createImageAttachment(entry, dek, exportId) {
  const epoch = attachmentEpoch;
  const container = document.createElement("figure");
  container.className = "attachment attachment-image";

  // Create placeholder
  const placeholder = document.createElement("div");
  placeholder.className = "attachment-placeholder";
  placeholder.innerHTML = `
        <span class="attachment-icon">🖼️</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
    `;

  // Create loading state
  const loading = document.createElement("div");
  loading.className = "attachment-loading hidden";
  loading.innerHTML = '<div class="spinner"></div>';

  // Create image element (hidden initially)
  const img = document.createElement("img");
  img.className = "attachment-img hidden";
  img.alt = entry.filename;

  // Create caption
  const caption = document.createElement("figcaption");
  caption.className = "attachment-caption";
  caption.textContent = entry.filename;

  container.appendChild(placeholder);
  container.appendChild(loading);
  container.appendChild(img);
  container.appendChild(caption);

  // Set up lazy loading with IntersectionObserver
  const observer = new IntersectionObserver(
    async (observerEntries) => {
      const [observerEntry] = observerEntries;
      if (observerEntry?.isIntersecting && isCurrentEpoch(epoch)) {
        observer.disconnect();
        attachmentObservers.delete(observer);
        await loadImageAttachment(
          container,
          img,
          observerEntry.target.dataset.hash,
          observerEntry.target.dataset.mimeType,
          dek,
          exportId,
          placeholder,
          loading,
        );
      }
    },
    { rootMargin: "100px" },
  );

  container.dataset.hash = entry.hash;
  container.dataset.mimeType = entry.mime_type;
  attachmentObservers.add(observer);
  observer.observe(container);

  // Also allow click to load
  placeholder.addEventListener("click", async () => {
    if (!isCurrentEpoch(epoch)) return;
    observer.disconnect();
    attachmentObservers.delete(observer);
    await loadImageAttachment(
      container,
      img,
      entry.hash,
      entry.mime_type,
      dek,
      exportId,
      placeholder,
      loading,
    );
  });

  return container;
}

/**
 * Load an image attachment
 */
async function loadImageAttachment(
  container,
  img,
  hash,
  mimeType,
  dek,
  exportId,
  placeholder,
  loading,
) {
  const epoch = attachmentEpoch;
  try {
    placeholder.classList.add("hidden");
    loading.classList.remove("hidden");

    const url = await loadBlobAsUrl(hash, mimeType, dek, exportId);
    await waitForImageLoad(img, url);
    if (!isCurrentEpoch(epoch)) return;

    loading.classList.add("hidden");
    img.classList.remove("hidden");
    container.classList.add("loaded");
  } catch (error) {
    if (!isCurrentEpoch(epoch) || error?.code === "ATTACHMENT_REQUEST_INVALIDATED") {
      return;
    }
    console.error("[Attachments] Failed to load image:", error);
    loading.classList.add("hidden");
    placeholder.classList.remove("hidden");
    placeholder.innerHTML = `
            <span class="attachment-icon">⚠️</span>
            <span class="attachment-error">${escapeHtml(attachmentFailureMessage(error, "Failed to load"))}</span>
        `;
  }
}

function waitForImageLoad(img, url) {
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      img.onload = null;
      img.onerror = null;
    };
    const handleLoad = () => {
      cleanup();
      resolve();
    };
    const handleError = () => {
      cleanup();
      reject(new Error("Image failed to load"));
    };

    img.onload = handleLoad;
    img.onerror = handleError;
    img.src = url;

    if (img.complete && (!("naturalWidth" in img) || img.naturalWidth > 0)) {
      handleLoad();
    }
  });
}

/**
 * Create a PDF attachment element
 */
function createPdfAttachment(entry, dek, exportId) {
  const epoch = attachmentEpoch;
  const container = document.createElement("div");
  container.className = "attachment attachment-pdf";

  container.innerHTML = `
        <span class="attachment-icon">📄</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
        <button class="attachment-download" type="button">Download</button>
    `;

  const downloadBtn = container.querySelector(".attachment-download");
  downloadBtn.addEventListener("click", async () => {
    if (!isCurrentEpoch(epoch)) return;
    await downloadAttachment(entry, dek, exportId);
  });

  return container;
}

/**
 * Create a generic download attachment element
 */
function createDownloadAttachment(entry, dek, exportId) {
  const epoch = attachmentEpoch;
  const container = document.createElement("div");
  container.className = "attachment attachment-file";

  container.innerHTML = `
        <span class="attachment-icon">📎</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
        <button class="attachment-download" type="button">Download</button>
    `;

  const downloadBtn = container.querySelector(".attachment-download");
  downloadBtn.addEventListener("click", async () => {
    if (!isCurrentEpoch(epoch)) return;
    await downloadAttachment(entry, dek, exportId);
  });

  return container;
}

/**
 * Download an attachment
 */
async function downloadAttachment(entry, dek, exportId) {
  const epoch = attachmentEpoch;
  try {
    const url = await loadBlobAsUrl(entry.hash, entry.mime_type, dek, exportId);
    if (!isCurrentEpoch(epoch)) return;

    // Create download link
    const a = document.createElement("a");
    a.href = url;
    a.download = entry.filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
  } catch (error) {
    if (!isCurrentEpoch(epoch) || error?.code === "ATTACHMENT_REQUEST_INVALIDATED") {
      return;
    }
    console.error("[Attachments] Failed to download:", error);
    alert(attachmentFailureMessage(error, "Failed to download attachment"));
  }
}

/**
 * Escape HTML special characters
 */
function escapeHtml(text) {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}

/**
 * Format file size for display
 */
function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// Export default
export default {
  initAttachments,
  hasAttachments,
  getManifest,
  getMessageAttachments,
  loadBlob,
  loadBlobAsUrl,
  createAttachmentElement,
  clearCache,
  reset,
  getCacheStats,
};
