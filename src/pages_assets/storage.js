/**
 * cass Archive Viewer - Storage Abstraction Module
 *
 * Provides a unified interface for different storage backends:
 *   - memory: In-memory only (most secure, lost on page close)
 *   - session: sessionStorage (cleared when tab closes)
 *   - local: localStorage (persists across sessions)
 *   - opfs: Origin Private File System (persistent, largest capacity)
 *
 * Security model:
 *   - Default is memory-only for maximum security
 *   - User must explicitly opt-in to persistent storage
 *   - Clear functions available for all storage types
 */

// Storage modes
export const StorageMode = {
    MEMORY: 'memory',
    SESSION: 'session',
    LOCAL: 'local',
    OPFS: 'opfs',
};

// Storage keys (prefixed to avoid collisions)
const STORAGE_PREFIX = 'cass-archive-';
const ALL_ARCHIVE_DATA_PREFIX_RE = /^cass-archive-[0-9a-f]{8}-data-/;
const ALL_ARCHIVE_PREF_PREFIX_RE = /^cass-archive-[0-9a-f]{8}-pref-/;
const LEGACY_PREF_KEYS = {
    MODE: `${STORAGE_PREFIX}storage-mode`,
    OPFS_ENABLED: `${STORAGE_PREFIX}opfs-enabled`,
    LAST_UNLOCK: `${STORAGE_PREFIX}last-unlock`,
    DB_CACHED: `${STORAGE_PREFIX}db-cached`,
};
const KEYS = {
    get MODE() {
        return `${getArchivePreferencePrefix()}storage-mode`;
    },
    get OPFS_ENABLED() {
        return `${getArchivePreferencePrefix()}opfs-enabled`;
    },
    THEME: `${STORAGE_PREFIX}theme`,
    get LAST_UNLOCK() {
        return `${getArchivePreferencePrefix()}last-unlock`;
    },
    get DB_CACHED() {
        return `${getArchivePreferencePrefix()}db-cached`;
    },
};
const LEGACY_OPFS_DB_FILES = [
    'cass-archive.sqlite3',
    'cass-archive.sqlite3-wal',
    'cass-archive.sqlite3-shm',
    'cass-archive.db',
    'cass-archive.db-wal',
    'cass-archive.db-shm',
];
const LEGACY_SESSION_KEYS = [
    'cass_session_dek',
    'cass_session_expiry',
    'cass_unlocked',
];
const ALL_ARCHIVE_SESSION_KEY_RE = /^cass_(?:session_(?:dek|expiry)|unlocked)_[0-9a-f]{8}$/;
const ALL_ARCHIVE_TOFU_KEY_RE = /^cass_fingerprint_v2_[0-9a-f]{8}$/;

// In-memory storage (fallback and default)
const memoryStore = new Map();

// Current storage mode
let currentMode = StorageMode.MEMORY;

// OPFS directory handle (cached)
let opfsRoot = null;

function tryGetSessionStorage() {
    try {
        if (typeof sessionStorage !== 'undefined') {
            return sessionStorage;
        }
    } catch (error) {
        // Ignore unavailable storage backends.
    }
    return null;
}

function tryGetLocalStorage() {
    try {
        if (typeof localStorage !== 'undefined') {
            return localStorage;
        }
    } catch (error) {
        // Ignore unavailable storage backends.
    }
    return null;
}

function hashScopeId(input) {
    let hash = 0x811c9dc5;
    for (let i = 0; i < input.length; i++) {
        hash ^= input.charCodeAt(i);
        hash = Math.imul(hash, 0x01000193) >>> 0;
    }
    return hash.toString(16).padStart(8, '0');
}

export function getArchiveScopeUrl() {
    try {
        return new URL('./', window.location.href).href;
    } catch (error) {
        const href = typeof window?.location?.href === 'string'
            ? window.location.href
            : 'unknown';
        return href.split('#')[0].split('?')[0];
    }
}

export function getArchiveScopeId() {
    return hashScopeId(getArchiveScopeUrl());
}

function getArchivePreferencePrefix() {
    return `${STORAGE_PREFIX}${getArchiveScopeId()}-pref-`;
}

function getArchiveDataPrefix() {
    return `${STORAGE_PREFIX}${getArchiveScopeId()}-data-`;
}

function getArchiveDataKey(key) {
    return `${getArchiveDataPrefix()}${key}`;
}

function isArchiveDataEntryName(name) {
    return ALL_ARCHIVE_DATA_PREFIX_RE.test(name);
}

function isArchivePreferenceKey(name) {
    return ALL_ARCHIVE_PREF_PREFIX_RE.test(name);
}

function getCurrentArchiveSessionKeys() {
    const scopeId = getArchiveScopeId();
    return new Set([
        ...LEGACY_SESSION_KEYS,
        `cass_session_dek_${scopeId}`,
        `cass_session_expiry_${scopeId}`,
        `cass_unlocked_${scopeId}`,
    ]);
}

function isArchiveSessionKey(name) {
    return LEGACY_SESSION_KEYS.includes(name) || ALL_ARCHIVE_SESSION_KEY_RE.test(name);
}

function getCurrentArchiveTofuKey() {
    return `cass_fingerprint_v2_${getArchiveScopeId()}`;
}

function isArchiveTofuKey(name) {
    return ALL_ARCHIVE_TOFU_KEY_RE.test(name);
}

function getServiceWorkerCachePrefix() {
    return `cass-archive-${getArchiveScopeId()}-`;
}

export function getArchiveOpfsDbFiles() {
    const scopeId = getArchiveScopeId();
    return [
        `cass-archive-${scopeId}.sqlite3`,
        `cass-archive-${scopeId}.sqlite3-wal`,
        `cass-archive-${scopeId}.sqlite3-shm`,
        `cass-archive-${scopeId}.db`,
        `cass-archive-${scopeId}.db-wal`,
        `cass-archive-${scopeId}.db-shm`,
    ];
}

export function getArchiveOpfsPrimaryDbName() {
    return getArchiveOpfsDbFiles()[0];
}

function isCassOpfsDbFile(name) {
    return (
        LEGACY_OPFS_DB_FILES.includes(name)
        || /^cass-archive-[0-9a-f]{8}\.(?:sqlite3|db)(?:-(?:wal|shm))?$/.test(name)
    );
}

/**
 * Initialize storage module
 * Loads saved storage mode preference
 */
export async function initStorage() {
    console.log('[Storage] Initializing...');

    const savedMode = getStoredMode();
    currentMode = savedMode;
    if (currentMode === StorageMode.OPFS) {
        if (!isOpfsEnabled()) {
            setOpfsEnabled(true);
        }
        currentMode = StorageMode.MEMORY;
        try {
            localStorage.setItem(KEYS.MODE, StorageMode.MEMORY);
        } catch (e) {
            // Ignore
        }
    }
    console.log('[Storage] Restored mode:', currentMode);

    return currentMode;
}

/**
 * Get current storage mode
 */
export function getStorageMode() {
    return currentMode;
}

/**
 * Get the stored storage mode preference
 */
export function getStoredMode() {
    try {
        const savedMode = localStorage.getItem(KEYS.MODE);
        if (savedMode && Object.values(StorageMode).includes(savedMode)) {
            return savedMode;
        }
    } catch (e) {
        // Ignore
    }
    return StorageMode.MEMORY;
}

/**
 * Check if OPFS persistence is enabled by user
 */
export function isOpfsEnabled() {
    try {
        return localStorage.getItem(KEYS.OPFS_ENABLED) === 'true';
    } catch (e) {
        return false;
    }
}

/**
 * Persist OPFS opt-in preference
 */
export function setOpfsEnabled(enabled) {
    try {
        if (enabled) {
            localStorage.setItem(KEYS.OPFS_ENABLED, 'true');
        } else {
            localStorage.removeItem(KEYS.OPFS_ENABLED);
        }
    } catch (e) {
        // Ignore
    }
    return enabled;
}

/**
 * Set storage mode
 * @param {string} mode - One of StorageMode values
 * @param {boolean} migrate - Whether to migrate existing data
 */
export async function setStorageMode(mode, migrate = false) {
    if (!Object.values(StorageMode).includes(mode)) {
        throw new Error(`Invalid storage mode: ${mode}`);
    }

    if (mode === StorageMode.OPFS) {
        if (!isOpfsEnabled()) {
            setOpfsEnabled(true);
        }
        mode = StorageMode.MEMORY;
    }

    const oldMode = currentMode;

    // Migrate data if requested
    if (migrate && oldMode !== mode) {
        await migrateStorage(oldMode, mode);
    }

    currentMode = mode;

    // Save mode preference (in localStorage so it persists)
    try {
        localStorage.setItem(KEYS.MODE, mode);
    } catch (e) {
        console.warn('[Storage] Could not save mode preference');
    }

    console.log('[Storage] Mode changed:', oldMode, '->', mode);
    return mode;
}

/**
 * Check if OPFS is available
 */
export function isOPFSAvailable() {
    return 'storage' in navigator && 'getDirectory' in navigator.storage;
}

/**
 * Initialize OPFS
 */
async function initOPFS() {
    if (!isOPFSAvailable()) {
        throw new Error('OPFS not available in this browser');
    }

    opfsRoot = await navigator.storage.getDirectory();
    console.log('[Storage] OPFS initialized');
    return opfsRoot;
}

/**
 * Get OPFS directory handle
 */
export async function getOPFSRoot() {
    if (!opfsRoot) {
        await initOPFS();
    }
    return opfsRoot;
}

/**
 * Store a value
 * @param {string} key - Storage key
 * @param {*} value - Value to store (will be JSON serialized)
 */
export async function setItem(key, value) {
    const fullKey = getArchiveDataKey(key);
    const serialized = JSON.stringify(value);

    switch (currentMode) {
        case StorageMode.MEMORY:
            memoryStore.set(fullKey, serialized);
            break;

        case StorageMode.SESSION:
            try {
                sessionStorage.setItem(fullKey, serialized);
            } catch (e) {
                console.warn('[Storage] sessionStorage write failed:', e);
                memoryStore.set(fullKey, serialized);
            }
            break;

        case StorageMode.LOCAL:
            try {
                localStorage.setItem(fullKey, serialized);
            } catch (e) {
                console.warn('[Storage] localStorage write failed:', e);
                memoryStore.set(fullKey, serialized);
            }
            break;

        case StorageMode.OPFS:
            await writeOPFSFile(fullKey, serialized);
            break;
    }
}

/**
 * Get a value
 * @param {string} key - Storage key
 * @param {*} defaultValue - Default value if not found
 */
export async function getItem(key, defaultValue = null) {
    const fullKey = getArchiveDataKey(key);
    let serialized = null;

    switch (currentMode) {
        case StorageMode.MEMORY:
            serialized = memoryStore.get(fullKey);
            break;

        case StorageMode.SESSION:
            try {
                serialized = sessionStorage.getItem(fullKey);
            } catch (e) {
                serialized = memoryStore.get(fullKey);
            }
            break;

        case StorageMode.LOCAL:
            try {
                serialized = localStorage.getItem(fullKey);
            } catch (e) {
                serialized = memoryStore.get(fullKey);
            }
            break;

        case StorageMode.OPFS:
            serialized = await readOPFSFile(fullKey);
            break;
    }

    if (serialized === null || serialized === undefined) {
        return defaultValue;
    }

    try {
        return JSON.parse(serialized);
    } catch (e) {
        return serialized;
    }
}

/**
 * Remove a value
 * @param {string} key - Storage key
 */
export async function removeItem(key) {
    const fullKey = getArchiveDataKey(key);

    switch (currentMode) {
        case StorageMode.MEMORY:
            memoryStore.delete(fullKey);
            break;

        case StorageMode.SESSION:
            try {
                sessionStorage.removeItem(fullKey);
            } catch (e) {
                // Ignore
            }
            memoryStore.delete(fullKey);
            break;

        case StorageMode.LOCAL:
            try {
                localStorage.removeItem(fullKey);
            } catch (e) {
                // Ignore
            }
            memoryStore.delete(fullKey);
            break;

        case StorageMode.OPFS:
            await deleteOPFSFile(fullKey);
            break;
    }
}

/**
 * Write file to OPFS
 */
async function writeOPFSFile(filename, content) {
    try {
        const root = await getOPFSRoot();
        const fileHandle = await root.getFileHandle(filename, { create: true });
        const writable = await fileHandle.createWritable();
        await writable.write(content);
        await writable.close();
    } catch (e) {
        console.error('[Storage] OPFS write failed:', e);
        // Fallback to memory
        memoryStore.set(filename, content);
    }
}

/**
 * Read file from OPFS
 */
async function readOPFSFile(filename) {
    try {
        const root = await getOPFSRoot();
        const fileHandle = await root.getFileHandle(filename);
        const file = await fileHandle.getFile();
        return await file.text();
    } catch (e) {
        if (e.name !== 'NotFoundError') {
            console.warn('[Storage] OPFS read failed:', e);
        }
        return null;
    }
}

/**
 * Delete file from OPFS
 */
async function deleteOPFSFile(filename) {
    try {
        const root = await getOPFSRoot();
        await root.removeEntry(filename);
    } catch (e) {
        if (e.name !== 'NotFoundError') {
            console.warn('[Storage] OPFS delete failed:', e);
        }
    }
}

/**
 * Store binary data (for database file)
 * @param {string} key - Storage key
 * @param {ArrayBuffer|Uint8Array} data - Binary data
 */
export async function setBinaryItem(key, data) {
    const fullKey = getArchiveDataKey(key);

    if (currentMode === StorageMode.OPFS) {
        try {
            const root = await getOPFSRoot();
            const fileHandle = await root.getFileHandle(fullKey, { create: true });
            const writable = await fileHandle.createWritable();
            await writable.write(data);
            await writable.close();
            console.log('[Storage] Binary data written to OPFS:', fullKey);
            return true;
        } catch (e) {
            console.error('[Storage] OPFS binary write failed:', e);
            return false;
        }
    }

    // For non-OPFS modes, we can't efficiently store binary data
    // Log warning and return false
    console.warn('[Storage] Binary storage only supported in OPFS mode');
    return false;
}

/**
 * Get binary data
 * @param {string} key - Storage key
 */
export async function getBinaryItem(key) {
    const fullKey = getArchiveDataKey(key);

    if (currentMode === StorageMode.OPFS) {
        try {
            const root = await getOPFSRoot();
            const fileHandle = await root.getFileHandle(fullKey);
            const file = await fileHandle.getFile();
            return await file.arrayBuffer();
        } catch (e) {
            if (e.name !== 'NotFoundError') {
                console.warn('[Storage] OPFS binary read failed:', e);
            }
            return null;
        }
    }

    return null;
}

/**
 * Migrate data between storage modes
 */
async function migrateStorage(fromMode, toMode) {
    console.log('[Storage] Migrating from', fromMode, 'to', toMode);

    // Get all keys from source
    const archiveDataPrefix = getArchiveDataPrefix();
    const keys = [];
    const values = new Map();

    switch (fromMode) {
        case StorageMode.MEMORY:
            for (const [key, value] of memoryStore) {
                if (key.startsWith(archiveDataPrefix)) {
                    keys.push(key);
                    values.set(key, value);
                }
            }
            break;

        case StorageMode.SESSION:
            {
                const storage = tryGetSessionStorage();
                if (!storage) {
                    break;
                }
                for (let i = 0; i < storage.length; i++) {
                    const key = storage.key(i);
                    if (key && key.startsWith(archiveDataPrefix)) {
                        keys.push(key);
                        values.set(key, storage.getItem(key));
                    }
                }
            }
            break;

        case StorageMode.LOCAL:
            {
                const storage = tryGetLocalStorage();
                if (!storage) {
                    break;
                }
                for (let i = 0; i < storage.length; i++) {
                    const key = storage.key(i);
                    if (key && key.startsWith(archiveDataPrefix)) {
                        keys.push(key);
                        values.set(key, storage.getItem(key));
                    }
                }
            }
            break;

        case StorageMode.OPFS:
            // OPFS migration is more complex, skip for now
            console.log('[Storage] OPFS migration not implemented');
            return;
    }

    // Write to destination
    const oldMode = currentMode;
    currentMode = toMode;

    for (const key of keys) {
        const shortKey = key.slice(archiveDataPrefix.length);
        const value = values.get(key);
        if (value) {
            try {
                await setItem(shortKey, JSON.parse(value));
            } catch (e) {
                await setItem(shortKey, value);
            }
        }
    }

    currentMode = oldMode;
    console.log('[Storage] Migrated', keys.length, 'items');
}

function removeMapEntriesWithPrefix(map, prefix) {
    for (const key of [...map.keys()]) {
        if (key.startsWith(prefix)) {
            map.delete(key);
        }
    }
}

function removeStorageEntriesWithPrefix(storage, prefix) {
    const keys = [];
    for (let i = 0; i < storage.length; i++) {
        const key = storage.key(i);
        if (key && key.startsWith(prefix)) {
            keys.push(key);
        }
    }
    keys.forEach((key) => storage.removeItem(key));
}

function removeStorageEntries(storage, predicate) {
    const keys = [];
    for (let i = 0; i < storage.length; i++) {
        const key = storage.key(i);
        if (key && predicate(key)) {
            keys.push(key);
        }
    }
    keys.forEach((key) => storage.removeItem(key));
}

function clearCurrentArchivePreferenceKeys(options = {}) {
    const { includeLegacy = false } = options;

    try {
        localStorage.removeItem(KEYS.MODE);
        localStorage.removeItem(KEYS.OPFS_ENABLED);
        localStorage.removeItem(KEYS.LAST_UNLOCK);
        localStorage.removeItem(KEYS.DB_CACHED);
        if (includeLegacy) {
            Object.values(LEGACY_PREF_KEYS).forEach((key) => localStorage.removeItem(key));
        }
    } catch (e) {
        // Ignore
    }
}

/**
 * Clear all cass storage in current mode
 */
export async function clearCurrentStorage() {
    console.log('[Storage] Clearing current storage:', currentMode);
    const archiveDataPrefix = getArchiveDataPrefix();
    const currentSessionKeys = getCurrentArchiveSessionKeys();
    const currentTofuKey = getCurrentArchiveTofuKey();

    // Writes in session/local modes can fall back to memoryStore if the browser
    // rejects storage access. Clear that archive-scoped fallback copy too.
    removeMapEntriesWithPrefix(memoryStore, archiveDataPrefix);

    switch (currentMode) {
        case StorageMode.MEMORY:
            break;

        case StorageMode.SESSION:
            {
                const storage = tryGetSessionStorage();
                if (storage) {
                    removeStorageEntries(storage, (key) =>
                        key.startsWith(archiveDataPrefix) || currentSessionKeys.has(key)
                    );
                }
            }
            break;

        case StorageMode.LOCAL:
            {
                const storage = tryGetLocalStorage();
                if (storage) {
                    removeStorageEntries(storage, (key) =>
                        key.startsWith(archiveDataPrefix)
                        || currentSessionKeys.has(key)
                        || key === currentTofuKey
                    );
                }
            }
            break;

        case StorageMode.OPFS:
            await clearOPFS();
            break;
    }
}

/**
 * Clear OPFS storage
 */
export async function clearOPFS(options = {}) {
    const { allArchives = false } = options;

    if (!isOPFSAvailable()) {
        return;
    }

    try {
        const root = await navigator.storage.getDirectory();
        const currentArchiveDbFiles = new Set(getArchiveOpfsDbFiles());
        const archiveDataPrefix = getArchiveDataPrefix();

        // Iterate and delete all entries
        const entries = [];
        for await (const entry of root.keys()) {
            const shouldDeleteData = allArchives
                ? isArchiveDataEntryName(entry)
                : entry.startsWith(archiveDataPrefix);
            const shouldDeleteDb = allArchives
                ? isCassOpfsDbFile(entry)
                : currentArchiveDbFiles.has(entry) || LEGACY_OPFS_DB_FILES.includes(entry);
            if (shouldDeleteData || shouldDeleteDb) {
                entries.push(entry);
            }
        }

        for (const entry of entries) {
            try {
                await root.removeEntry(entry);
            } catch (e) {
                console.warn('[Storage] Failed to delete OPFS entry:', entry, e);
            }
        }

        console.log('[Storage] OPFS cleared:', entries.length, 'entries');
    } catch (e) {
        console.error('[Storage] OPFS clear failed:', e);
    }
}

/**
 * Clear all cass storage across all modes
 */
export async function clearAllStorage(options = {}) {
    const { allArchives = false } = options;

    console.log('[Storage] Clearing all storage');
    const archiveDataPrefix = getArchiveDataPrefix();
    const currentSessionKeys = getCurrentArchiveSessionKeys();
    const currentTofuKey = getCurrentArchiveTofuKey();

    // Clear memory
    if (allArchives) {
        removeMapEntriesWithPrefix(memoryStore, STORAGE_PREFIX);
    } else {
        removeMapEntriesWithPrefix(memoryStore, archiveDataPrefix);
    }

    // Clear sessionStorage
    try {
        if (allArchives) {
            removeStorageEntries(sessionStorage, (key) =>
                key.startsWith(STORAGE_PREFIX) || isArchiveSessionKey(key)
            );
        } else {
            removeStorageEntries(sessionStorage, (key) =>
                key.startsWith(archiveDataPrefix) || currentSessionKeys.has(key)
            );
        }
    } catch (e) {
        // Ignore
    }

    // Clear localStorage
    try {
        if (allArchives) {
            removeStorageEntries(localStorage, (key) =>
                key.startsWith(STORAGE_PREFIX)
                && (isArchiveDataEntryName(key) || isArchivePreferenceKey(key) || Object.values(LEGACY_PREF_KEYS).includes(key))
                || isArchiveSessionKey(key)
                || isArchiveTofuKey(key)
            );
        } else {
            removeStorageEntries(localStorage, (key) =>
                key.startsWith(archiveDataPrefix)
                || currentSessionKeys.has(key)
                || key === currentTofuKey
            );
            clearCurrentArchivePreferenceKeys({ includeLegacy: true });
        }
    } catch (e) {
        // Ignore
    }

    // Clear OPFS
    await clearOPFS({ allArchives });

    console.log('[Storage] All storage cleared');
}

/**
 * Clear Service Worker cache
 */
export async function clearServiceWorkerCache(options = {}) {
    const { allArchives = false } = options;

    if (!('caches' in window)) {
        console.log('[Storage] Cache API not available');
        return false;
    }

    try {
        const cacheNames = await caches.keys();
        const cachePrefix = getServiceWorkerCachePrefix();
        const cassNames = cacheNames.filter(
            (name) => allArchives
                ? name.startsWith('cass-archive-')
                : name.startsWith(cachePrefix)
        );

        await Promise.all(cassNames.map((name) => caches.delete(name)));

        console.log('[Storage] Service Worker caches cleared:', cassNames);
        return true;
    } catch (e) {
        console.error('[Storage] Failed to clear SW cache:', e);
        return false;
    }
}

/**
 * Unregister Service Worker
 */
export async function unregisterServiceWorker(options = {}) {
    const { allArchives = false } = options;

    if (!('serviceWorker' in navigator)) {
        return false;
    }

    try {
        const registrations = await navigator.serviceWorker.getRegistrations();
        const currentScope = getArchiveScopeUrl();
        const targets = registrations.filter((reg) => allArchives || reg.scope === currentScope);
        await Promise.all(targets.map((reg) => reg.unregister()));
        console.log('[Storage] Service Workers unregistered');
        return true;
    } catch (e) {
        console.error('[Storage] Failed to unregister SW:', e);
        return false;
    }
}

/**
 * Get storage usage statistics
 */
export async function getStorageStats() {
    const stats = {
        mode: currentMode,
        memory: {
            items: 0,
            bytes: 0,
        },
        session: {
            items: 0,
            bytes: 0,
        },
        local: {
            items: 0,
            bytes: 0,
        },
        opfs: {
            items: 0,
            bytes: 0,
            dbBytes: 0,
            dbFiles: [],
            available: isOPFSAvailable(),
        },
        quota: null,
    };

    const archiveDataPrefix = getArchiveDataPrefix();
    const currentArchiveDbFiles = new Set(getArchiveOpfsDbFiles());

    // Count memory items
    for (const [key, value] of memoryStore) {
        if (key.startsWith(archiveDataPrefix)) {
            stats.memory.items++;
            stats.memory.bytes += key.length + (value?.length || 0);
        }
    }

    // Count sessionStorage
    try {
        for (let i = 0; i < sessionStorage.length; i++) {
            const key = sessionStorage.key(i);
            if (key && key.startsWith(archiveDataPrefix)) {
                stats.session.items++;
                const value = sessionStorage.getItem(key);
                stats.session.bytes += key.length + (value?.length || 0);
            }
        }
    } catch (e) {
        // Ignore
    }

    // Count localStorage
    try {
        for (let i = 0; i < localStorage.length; i++) {
            const key = localStorage.key(i);
            if (key && key.startsWith(archiveDataPrefix)) {
                stats.local.items++;
                const value = localStorage.getItem(key);
                stats.local.bytes += key.length + (value?.length || 0);
            }
        }
    } catch (e) {
        // Ignore
    }

    // Count OPFS
    if (isOPFSAvailable()) {
        try {
            const root = await navigator.storage.getDirectory();
            for await (const name of root.keys()) {
                if (name.startsWith(archiveDataPrefix) || currentArchiveDbFiles.has(name)) {
                    stats.opfs.items++;
                    try {
                        const handle = await root.getFileHandle(name);
                        const file = await handle.getFile();
                        stats.opfs.bytes += file.size;
                        if (currentArchiveDbFiles.has(name)) {
                            stats.opfs.dbBytes += file.size;
                            stats.opfs.dbFiles.push(name);
                        }
                    } catch (e) {
                        // Ignore individual file errors
                    }
                }
            }
        } catch (e) {
            console.warn('[Storage] OPFS stats failed:', e);
        }
    }

    // Get quota estimate
    if ('storage' in navigator && 'estimate' in navigator.storage) {
        try {
            stats.quota = await navigator.storage.estimate();
        } catch (e) {
            // Ignore
        }
    }

    return stats;
}

/**
 * Check if database is cached in OPFS
 */
export async function isDatabaseCached() {
    try {
        const root = await getOPFSRoot();
        for (const name of getArchiveOpfsDbFiles()) {
            try {
                await root.getFileHandle(name);
                return true;
            } catch (e) {
                // Try next name
            }
        }
        return false;
    } catch (e) {
        return false;
    }
}

/**
 * Format bytes for display
 */
export function formatBytes(bytes) {
    if (bytes === 0) return '0 B';

    const units = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    const size = bytes / Math.pow(1024, i);

    return size.toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

// Export storage keys for external use
export { KEYS as StorageKeys };

export default {
    StorageMode,
    StorageKeys: KEYS,
    initStorage,
    getStoredMode,
    getStorageMode,
    setStorageMode,
    isOPFSAvailable,
    isOpfsEnabled,
    setOpfsEnabled,
    getOPFSRoot,
    setItem,
    getItem,
    removeItem,
    setBinaryItem,
    getBinaryItem,
    clearCurrentStorage,
    clearOPFS,
    clearAllStorage,
    clearServiceWorkerCache,
    unregisterServiceWorker,
    getStorageStats,
    isDatabaseCached,
    formatBytes,
    getArchiveScopeUrl,
    getArchiveScopeId,
    getArchiveOpfsDbFiles,
    getArchiveOpfsPrimaryDbName,
};
