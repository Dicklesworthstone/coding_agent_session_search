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
const BLOB_NONCE_DOMAIN = 'cass-blob-nonce-v1';

// Cache configuration
const CACHE_CONFIG = {
    MAX_ENTRIES: 50,          // Maximum cached blobs
    MAX_SIZE_BYTES: 50 * 1024 * 1024, // 50 MB max cache size
};

// Module state
let manifest = null;
let isManifestLoaded = false;
let manifestLoadPromise = null;

// Blob cache: hash -> { data: Uint8Array, objectUrl: string|null, size: number }
const blobCache = new Map();
let cacheSize = 0;

// LRU tracking
const lruOrder = [];

/**
 * Initialize the attachment system
 * Fetches and decrypts the manifest if attachments are present
 *
 * @param {Uint8Array} dek - Data encryption key
 * @param {Uint8Array} exportId - Export ID bytes
 * @returns {Promise<object|null>} Manifest or null if no attachments
 */
export async function initAttachments(dek, exportId) {
    if (isManifestLoaded) {
        return manifest;
    }

    if (manifestLoadPromise) {
        return manifestLoadPromise;
    }

    manifestLoadPromise = loadManifest(dek, exportId);

    try {
        manifest = await manifestLoadPromise;
        isManifestLoaded = true;
        return manifest;
    } catch (error) {
        console.warn('[Attachments] No attachments found or manifest failed:', error.message);
        manifest = null;
        isManifestLoaded = true;
        return null;
    } finally {
        manifestLoadPromise = null;
    }
}

/**
 * Load and decrypt the manifest
 */
async function loadManifest(dek, exportId) {
    const response = await fetch('./blobs/manifest.enc');
    if (!response.ok) {
        throw new Error('Manifest not found');
    }

    const ciphertext = new Uint8Array(await response.arrayBuffer());

    // Derive nonce using HKDF
    const nonce = await deriveBlobNonce('manifest');

    // Import DEK for decryption
    const dekKey = await crypto.subtle.importKey(
        'raw',
        dek,
        { name: 'AES-GCM' },
        false,
        ['decrypt']
    );

    // Decrypt with AAD = export_id only
    const plaintext = await crypto.subtle.decrypt(
        {
            name: 'AES-GCM',
            iv: nonce,
            additionalData: exportId,
        },
        dekKey,
        ciphertext
    );

    // Parse JSON manifest
    const decoder = new TextDecoder();
    const manifestJson = decoder.decode(plaintext);
    return JSON.parse(manifestJson);
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
    return manifest.entries.filter(entry => entry.message_id === messageId);
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
    // Check cache
    if (blobCache.has(hash)) {
        updateLru(hash);
        return blobCache.get(hash).data;
    }

    // Fetch encrypted blob
    const response = await fetch(`./blobs/${hash}.bin`);
    if (!response.ok) {
        throw new Error(`Blob not found: ${hash}`);
    }

    const ciphertext = new Uint8Array(await response.arrayBuffer());

    // Derive nonce using HKDF
    const nonce = await deriveBlobNonce(hash);

    // Import DEK for decryption
    const dekKey = await crypto.subtle.importKey(
        'raw',
        dek,
        { name: 'AES-GCM' },
        false,
        ['decrypt']
    );

    // Build AAD: export_id || hash_bytes
    const hashBytes = hexToBytes(hash);
    const aad = new Uint8Array(exportId.length + hashBytes.length);
    aad.set(exportId);
    aad.set(hashBytes, exportId.length);

    // Decrypt
    const plaintext = await crypto.subtle.decrypt(
        {
            name: 'AES-GCM',
            iv: nonce,
            additionalData: aad,
        },
        dekKey,
        ciphertext
    );

    const data = new Uint8Array(plaintext);

    // Cache the result
    cacheBlob(hash, data);

    return data;
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
    // Check if we already have an object URL
    const cached = blobCache.get(hash);
    if (cached?.objectUrl) {
        updateLru(hash);
        return cached.objectUrl;
    }

    // Load the blob data
    const data = await loadBlob(hash, dek, exportId);

    // Create object URL
    const blob = new Blob([data], { type: mimeType });
    const url = URL.createObjectURL(blob);

    // Update cache with URL
    if (blobCache.has(hash)) {
        blobCache.get(hash).objectUrl = url;
    }

    return url;
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
    const info = encoder.encode('nonce');

    // Import IKM as HKDF key material
    const baseKey = await crypto.subtle.importKey(
        'raw',
        ikm,
        'HKDF',
        false,
        ['deriveBits']
    );

    // Derive 96 bits (12 bytes) using HKDF
    const nonceBits = await crypto.subtle.deriveBits(
        {
            name: 'HKDF',
            hash: 'SHA-256',
            salt: salt,
            info: info,
        },
        baseKey,
        96 // 12 bytes * 8 bits
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

/**
 * Cache a blob with LRU eviction
 */
function cacheBlob(hash, data) {
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
        objectUrl: null,
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
        // Revoke object URL if present
        if (entry.objectUrl) {
            URL.revokeObjectURL(entry.objectUrl);
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
        if (entry.objectUrl) {
            URL.revokeObjectURL(entry.objectUrl);
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
    clearCache();
    manifest = null;
    isManifestLoaded = false;
    manifestLoadPromise = null;
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
    const container = document.createElement('div');
    container.className = 'attachment';
    container.dataset.hash = entry.hash;
    container.dataset.mimeType = entry.mime_type;

    // Determine type and create appropriate element
    if (entry.mime_type.startsWith('image/')) {
        return createImageAttachment(entry, dek, exportId);
    } else if (entry.mime_type === 'application/pdf') {
        return createPdfAttachment(entry, dek, exportId);
    } else {
        return createDownloadAttachment(entry, dek, exportId);
    }
}

/**
 * Create an image attachment element with lazy loading
 */
function createImageAttachment(entry, dek, exportId) {
    const container = document.createElement('figure');
    container.className = 'attachment attachment-image';

    // Create placeholder
    const placeholder = document.createElement('div');
    placeholder.className = 'attachment-placeholder';
    placeholder.innerHTML = `
        <span class="attachment-icon">🖼️</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
    `;

    // Create loading state
    const loading = document.createElement('div');
    loading.className = 'attachment-loading hidden';
    loading.innerHTML = '<div class="spinner"></div>';

    // Create image element (hidden initially)
    const img = document.createElement('img');
    img.className = 'attachment-img hidden';
    img.alt = entry.filename;

    // Create caption
    const caption = document.createElement('figcaption');
    caption.className = 'attachment-caption';
    caption.textContent = entry.filename;

    container.appendChild(placeholder);
    container.appendChild(loading);
    container.appendChild(img);
    container.appendChild(caption);

    // Set up lazy loading with IntersectionObserver
    const observer = new IntersectionObserver(async (entries) => {
        const [entry] = entries;
        if (entry.isIntersecting) {
            observer.disconnect();
            await loadImageAttachment(container, img, entry.target.dataset.hash, entry.target.dataset.mimeType, dek, exportId, placeholder, loading);
        }
    }, { rootMargin: '100px' });

    container.dataset.hash = entry.hash;
    container.dataset.mimeType = entry.mime_type;
    observer.observe(container);

    // Also allow click to load
    placeholder.addEventListener('click', async () => {
        observer.disconnect();
        await loadImageAttachment(container, img, entry.hash, entry.mime_type, dek, exportId, placeholder, loading);
    });

    return container;
}

/**
 * Load an image attachment
 */
async function loadImageAttachment(container, img, hash, mimeType, dek, exportId, placeholder, loading) {
    try {
        placeholder.classList.add('hidden');
        loading.classList.remove('hidden');

        const url = await loadBlobAsUrl(hash, mimeType, dek, exportId);
        img.src = url;

        await new Promise((resolve, reject) => {
            img.onload = resolve;
            img.onerror = reject;
        });

        loading.classList.add('hidden');
        img.classList.remove('hidden');
        container.classList.add('loaded');
    } catch (error) {
        console.error('[Attachments] Failed to load image:', error);
        loading.classList.add('hidden');
        placeholder.classList.remove('hidden');
        placeholder.innerHTML = `
            <span class="attachment-icon">⚠️</span>
            <span class="attachment-error">Failed to load</span>
        `;
    }
}

/**
 * Create a PDF attachment element
 */
function createPdfAttachment(entry, dek, exportId) {
    const container = document.createElement('div');
    container.className = 'attachment attachment-pdf';

    container.innerHTML = `
        <span class="attachment-icon">📄</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
        <button class="attachment-download" type="button">Download</button>
    `;

    const downloadBtn = container.querySelector('.attachment-download');
    downloadBtn.addEventListener('click', async () => {
        await downloadAttachment(entry, dek, exportId);
    });

    return container;
}

/**
 * Create a generic download attachment element
 */
function createDownloadAttachment(entry, dek, exportId) {
    const container = document.createElement('div');
    container.className = 'attachment attachment-file';

    container.innerHTML = `
        <span class="attachment-icon">📎</span>
        <span class="attachment-name">${escapeHtml(entry.filename)}</span>
        <span class="attachment-size">${formatSize(entry.size_bytes)}</span>
        <button class="attachment-download" type="button">Download</button>
    `;

    const downloadBtn = container.querySelector('.attachment-download');
    downloadBtn.addEventListener('click', async () => {
        await downloadAttachment(entry, dek, exportId);
    });

    return container;
}

/**
 * Download an attachment
 */
async function downloadAttachment(entry, dek, exportId) {
    try {
        const url = await loadBlobAsUrl(entry.hash, entry.mime_type, dek, exportId);

        // Create download link
        const a = document.createElement('a');
        a.href = url;
        a.download = entry.filename;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
    } catch (error) {
        console.error('[Attachments] Failed to download:', error);
        alert('Failed to download attachment');
    }
}

/**
 * Escape HTML special characters
 */
function escapeHtml(text) {
    const div = document.createElement('div');
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
