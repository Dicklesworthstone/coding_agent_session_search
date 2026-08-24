/**
 * cass Archive Viewer - Storage Abstraction Module
 *
 * Provides a unified interface for active storage backends:
 *   - memory: In-memory only (most secure, lost on page close)
 *   - session: sessionStorage (cleared when tab closes)
 *   - local: localStorage (persists across sessions)
 * Legacy OPFS database residue can be inspected and cleared, but OPFS is not
 * an active storage mode and decrypted databases always stay in memory.
 *
 * Security model:
 *   - Default is memory-only for maximum security
 *   - Session/local persistence requires explicit user selection
 *   - Decrypted database bytes are never persisted to OPFS
 *   - Clear functions available for all storage types
 */

// Storage modes
export const StorageMode = {
    MEMORY: 'memory',
    SESSION: 'session',
    LOCAL: 'local',
};
const LEGACY_OPFS_MODE = 'opfs';

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
const LEGACY_SESSION_MANAGER_KEYS = [
    'cass_session',
    'cass_expiry',
    'cass_storage_pref',
];
const ALL_ARCHIVE_SESSION_KEY_RE = /^cass_(?:session_(?:dek|expiry)|unlocked)_[0-9a-f]{8}$/;
const ALL_SESSION_MANAGER_KEY_RE = /^cass_(?:session|expiry|storage_pref)_[0-9a-f]{8}$/;
const ALL_ARCHIVE_TOFU_KEY_RE = /^cass_fingerprint_v2_[0-9a-f]{8}$/;

// In-memory storage (fallback and default)
const memoryStore = new Map();

// Current storage mode
let currentMode = StorageMode.MEMORY;

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
        // This module ships at the archive root. Derive identity from the
        // asset URL, not the current document URL, which may be a nested route.
        return new URL('./', import.meta.url).href;
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
        ...LEGACY_SESSION_MANAGER_KEYS,
        `cass_session_dek_${scopeId}`,
        `cass_session_expiry_${scopeId}`,
        `cass_unlocked_${scopeId}`,
        `cass_session_${scopeId}`,
        `cass_expiry_${scopeId}`,
        `cass_storage_pref_${scopeId}`,
    ]);
}

function isArchiveSessionKey(name) {
    return (
        LEGACY_SESSION_KEYS.includes(name)
        || LEGACY_SESSION_MANAGER_KEYS.includes(name)
        || ALL_ARCHIVE_SESSION_KEY_RE.test(name)
        || ALL_SESSION_MANAGER_KEY_RE.test(name)
    );
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

function getArchiveOpfsDbFiles() {
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

    currentMode = getStoredMode();
    clearLegacyOpfsPreferences();
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
 * Set storage mode
 * @param {string} mode - One of StorageMode values
 * @param {boolean} migrate - Whether to migrate existing data
 */
export async function setStorageMode(mode, migrate = false) {
    if (!Object.values(StorageMode).includes(mode)) {
        throw new Error(`Invalid storage mode: ${mode}`);
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
                if (serialized === null) {
                    serialized = memoryStore.get(fullKey);
                }
            } catch (e) {
                serialized = memoryStore.get(fullKey);
            }
            break;

        case StorageMode.LOCAL:
            try {
                serialized = localStorage.getItem(fullKey);
                if (serialized === null) {
                    serialized = memoryStore.get(fullKey);
                }
            } catch (e) {
                serialized = memoryStore.get(fullKey);
            }
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

    }
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

    }

    // Write to destination
    const oldMode = currentMode;
    currentMode = toMode;

    try {
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
    } finally {
        currentMode = oldMode;
    }

    console.log('[Storage] Migrated', keys.length, 'items');
}

function removeMapEntriesWithPrefix(map, prefix) {
    for (const key of [...map.keys()]) {
        if (key.startsWith(prefix)) {
            map.delete(key);
        }
    }
}

function removeStorageEntries(storage, predicate) {
    const keys = [];
    let cleared = true;
    let entryCount;

    try {
        entryCount = storage.length;
    } catch (error) {
        console.warn('[Storage] Could not enumerate browser storage during clear:', error);
        return false;
    }
    if (!Number.isSafeInteger(entryCount) || entryCount < 0) {
        console.warn('[Storage] Browser storage reported an invalid entry count during clear');
        return false;
    }

    for (let i = 0; i < entryCount; i++) {
        try {
            const key = storage.key(i);
            if (key && predicate(key)) {
                keys.push(key);
            }
        } catch (error) {
            // Keep scanning the remaining slots. A single inaccessible entry
            // must not prevent best-effort cleanup of every other key.
            console.warn('[Storage] Could not inspect browser storage entry during clear:', error);
            cleared = false;
        }
    }

    for (const key of keys) {
        try {
            storage.removeItem(key);
            if (storage.getItem(key) !== null) {
                cleared = false;
            }
        } catch (error) {
            console.warn('[Storage] Could not remove browser storage entry during clear:', error);
            cleared = false;
        }
    }

    // Re-enumerate after deletion. This catches a failed/no-op removeItem(),
    // unexpected storage mutation while clearing, and entries missed during
    // the initial snapshot instead of reporting a false success.
    try {
        const remainingCount = storage.length;
        if (!Number.isSafeInteger(remainingCount) || remainingCount < 0) {
            return false;
        }
        for (let i = 0; i < remainingCount; i++) {
            const key = storage.key(i);
            if (key && predicate(key)) {
                cleared = false;
            }
        }
    } catch (error) {
        console.warn('[Storage] Could not verify browser storage cleanup:', error);
        cleared = false;
    }

    return cleared;
}

function removeStorageKeys(storage, keys) {
    let cleared = true;

    for (const key of keys) {
        try {
            storage.removeItem(key);
            if (storage.getItem(key) !== null) {
                cleared = false;
            }
        } catch (error) {
            console.warn('[Storage] Could not remove browser storage key during clear:', error);
            cleared = false;
        }
    }

    return cleared;
}

function clearLegacyOpfsPreferences(options = {}) {
    const { allArchives = false } = options;
    const storage = tryGetLocalStorage();
    if (!storage) {
        return false;
    }

    if (allArchives) {
        return removeStorageEntries(storage, (key) => {
            if (key === LEGACY_PREF_KEYS.OPFS_ENABLED) {
                return true;
            }
            if (key === LEGACY_PREF_KEYS.MODE) {
                return storage.getItem(key) === LEGACY_OPFS_MODE;
            }
            if (!isArchivePreferenceKey(key)) {
                return false;
            }
            return key.endsWith('-opfs-enabled')
                || (key.endsWith('-storage-mode') && storage.getItem(key) === LEGACY_OPFS_MODE);
        });
    }

    const keys = new Set([KEYS.OPFS_ENABLED, LEGACY_PREF_KEYS.OPFS_ENABLED]);
    try {
        if (storage.getItem(KEYS.MODE) === LEGACY_OPFS_MODE) {
            keys.add(KEYS.MODE);
        }
        if (storage.getItem(LEGACY_PREF_KEYS.MODE) === LEGACY_OPFS_MODE) {
            keys.add(LEGACY_PREF_KEYS.MODE);
        }
    } catch (error) {
        console.warn('[Storage] Could not inspect legacy OPFS preferences:', error);
        return false;
    }
    return removeStorageKeys(storage, keys);
}

function clearCurrentArchivePreferenceKeys(options = {}) {
    const { includeLegacy = false } = options;
    const storage = tryGetLocalStorage();
    if (!storage) {
        return false;
    }

    const keys = [KEYS.MODE, KEYS.OPFS_ENABLED, KEYS.LAST_UNLOCK, KEYS.DB_CACHED];
    if (includeLegacy) {
        keys.push(...Object.values(LEGACY_PREF_KEYS));
    }

    return removeStorageKeys(storage, new Set(keys));
}

function clearCurrentArchiveSessionState(currentSessionKeys, currentTofuKey) {
    let cleared = true;
    const sessionStorageBackend = tryGetSessionStorage();
    if (sessionStorageBackend) {
        const sessionCleared = removeStorageKeys(sessionStorageBackend, currentSessionKeys);
        cleared = sessionCleared && cleared;
    } else {
        cleared = false;
    }

    const localStorageBackend = tryGetLocalStorage();
    if (localStorageBackend) {
        const localCleared = removeStorageKeys(
            localStorageBackend,
            new Set([...currentSessionKeys, currentTofuKey])
        );
        cleared = localCleared && cleared;
    } else {
        cleared = false;
    }

    return cleared;
}

/**
 * Clear all cass storage in current mode
 */
export async function clearCurrentStorage() {
    console.log('[Storage] Clearing current storage:', currentMode);
    const archiveDataPrefix = getArchiveDataPrefix();
    const currentSessionKeys = getCurrentArchiveSessionKeys();
    const currentTofuKey = getCurrentArchiveTofuKey();
    let cleared = clearCurrentArchiveSessionState(currentSessionKeys, currentTofuKey);

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
                    const storageCleared = removeStorageEntries(
                        storage,
                        (key) => key.startsWith(archiveDataPrefix)
                    );
                    cleared = storageCleared && cleared;
                } else {
                    cleared = false;
                }
            }
            break;

        case StorageMode.LOCAL:
            {
                const storage = tryGetLocalStorage();
                if (storage) {
                    const storageCleared = removeStorageEntries(
                        storage,
                        (key) => key.startsWith(archiveDataPrefix)
                    );
                    cleared = storageCleared && cleared;
                } else {
                    cleared = false;
                }
            }
            break;

    }

    return cleared;
}

/**
 * Clear OPFS storage
 */
export async function clearOPFS(options = {}) {
    const { allArchives = false } = options;
    const preferencesCleared = clearLegacyOpfsPreferences({ allArchives });

    if (!isOPFSAvailable()) {
        return preferencesCleared;
    }

    try {
        let cleared = true;
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
                cleared = false;
            }
        }

        // Verify the postcondition instead of assuming removeEntry() did what
        // it promised. Another same-origin context may also have recreated an
        // archive file while cleanup was in progress.
        try {
            for await (const entry of root.keys()) {
                const shouldDeleteData = allArchives
                    ? isArchiveDataEntryName(entry)
                    : entry.startsWith(archiveDataPrefix);
                const shouldDeleteDb = allArchives
                    ? isCassOpfsDbFile(entry)
                    : currentArchiveDbFiles.has(entry) || LEGACY_OPFS_DB_FILES.includes(entry);
                if (shouldDeleteData || shouldDeleteDb) {
                    cleared = false;
                }
            }
        } catch (e) {
            console.warn('[Storage] Could not verify OPFS cleanup:', e);
            cleared = false;
        }

        if (cleared) {
            console.log('[Storage] OPFS cleared:', entries.length, 'entries');
        } else {
            console.warn('[Storage] Some OPFS data could not be fully cleared');
        }
        return cleared && preferencesCleared;
    } catch (e) {
        console.error('[Storage] OPFS clear failed:', e);
        return false;
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
    let sessionCleared = true;
    let localCleared = true;

    // Clear memory
    if (allArchives) {
        removeMapEntriesWithPrefix(memoryStore, STORAGE_PREFIX);
    } else {
        removeMapEntriesWithPrefix(memoryStore, archiveDataPrefix);
    }

    // Clear sessionStorage. Treat an inaccessible backend as uncleared: data
    // may still exist even though this document cannot currently inspect it.
    const sessionStorageBackend = tryGetSessionStorage();
    if (sessionStorageBackend) {
        if (allArchives) {
            sessionCleared = removeStorageEntries(sessionStorageBackend, (key) =>
                key.startsWith(STORAGE_PREFIX) || isArchiveSessionKey(key)
            );
        } else {
            const archiveEntriesCleared = removeStorageEntries(
                sessionStorageBackend,
                (key) => key.startsWith(archiveDataPrefix)
            );
            const sessionKeysCleared = removeStorageKeys(
                sessionStorageBackend,
                currentSessionKeys
            );
            sessionCleared = archiveEntriesCleared && sessionKeysCleared;
        }
    } else {
        sessionCleared = false;
    }

    // Clear localStorage
    const localStorageBackend = tryGetLocalStorage();
    if (localStorageBackend) {
        if (allArchives) {
            localCleared = removeStorageEntries(localStorageBackend, (key) =>
                key.startsWith(STORAGE_PREFIX)
                && (isArchiveDataEntryName(key) || isArchivePreferenceKey(key) || Object.values(LEGACY_PREF_KEYS).includes(key))
                || isArchiveSessionKey(key)
                || isArchiveTofuKey(key)
            );
        } else {
            const archiveEntriesCleared = removeStorageEntries(
                localStorageBackend,
                (key) => key.startsWith(archiveDataPrefix)
            );
            const sessionKeysCleared = removeStorageKeys(
                localStorageBackend,
                new Set([...currentSessionKeys, currentTofuKey])
            );
            const preferencesCleared = clearCurrentArchivePreferenceKeys({ includeLegacy: true });
            localCleared = archiveEntriesCleared && sessionKeysCleared && preferencesCleared;
        }
    } else {
        localCleared = false;
    }

    // Clear OPFS
    const opfsCleared = await clearOPFS({ allArchives });

    const cleared = sessionCleared && localCleared && opfsCleared;
    if (cleared) {
        console.log('[Storage] All storage cleared');
    } else {
        console.warn('[Storage] Some storage could not be fully cleared');
    }
    return cleared;
}

/**
 * Clear Service Worker cache
 */
export async function clearServiceWorkerCache(options = {}) {
    const { allArchives = false } = options;

    if (!('caches' in window)) {
        console.log('[Storage] Cache API not available');
        return true;
    }

    try {
        const cacheNames = await caches.keys();
        const cachePrefix = getServiceWorkerCachePrefix();
        const cassNames = cacheNames.filter(
            (name) => allArchives
                ? name.startsWith('cass-archive-')
                : name.startsWith(cachePrefix)
        );

        const deleteResults = await Promise.allSettled(
            cassNames.map((name) => caches.delete(name))
        );
        for (let index = 0; index < deleteResults.length; index++) {
            const result = deleteResults[index];
            if (result.status === 'rejected') {
                console.warn('[Storage] Failed to delete Service Worker cache:', cassNames[index], result.reason);
            }
        }

        const remainingNames = await caches.keys();
        const remainingCassNames = remainingNames.filter(
            (name) => allArchives
                ? name.startsWith('cass-archive-')
                : name.startsWith(cachePrefix)
        );
        const cleared = remainingCassNames.length === 0;
        if (cleared) {
            console.log('[Storage] Service Worker caches cleared:', cassNames);
        } else {
            console.warn('[Storage] Some Service Worker caches could not be cleared:', remainingCassNames);
        }
        return cleared;
    } catch (e) {
        console.error('[Storage] Failed to clear SW cache:', e);
        return false;
    }
}

/**
 * Unregister Service Worker
 */
export async function unregisterServiceWorker() {
    if (!('serviceWorker' in navigator)) {
        return true;
    }

    try {
        const registrations = await navigator.serviceWorker.getRegistrations();
        const currentScope = getArchiveScopeUrl();
        // Registrations have no trustworthy application identifier. Keep reset
        // strictly scoped to this archive rather than letting an "all archives"
        // request unregister unrelated same-origin applications.
        const targets = registrations.filter((reg) => reg.scope === currentScope);
        const unregisterResults = await Promise.allSettled(
            targets.map((reg) => reg.unregister())
        );
        for (let index = 0; index < unregisterResults.length; index++) {
            const result = unregisterResults[index];
            if (result.status === 'rejected') {
                console.warn('[Storage] Failed to unregister Service Worker:', targets[index].scope, result.reason);
            }
        }

        const remainingRegistrations = await navigator.serviceWorker.getRegistrations();
        const remainingTargets = remainingRegistrations.filter(
            (reg) => reg.scope === currentScope
        );
        const unregistered = remainingTargets.length === 0;
        if (unregistered) {
            console.log('[Storage] Service Workers unregistered');
        } else {
            console.warn('[Storage] Some Service Workers could not be unregistered');
        }
        return unregistered;
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
                const isDatabaseResidue = currentArchiveDbFiles.has(name)
                    || LEGACY_OPFS_DB_FILES.includes(name);
                if (name.startsWith(archiveDataPrefix) || isDatabaseResidue) {
                    stats.opfs.items++;
                    if (isDatabaseResidue) {
                        // Detection is independent of whether metadata reads
                        // succeed; inaccessible residue must remain visible.
                        stats.opfs.dbFiles.push(name);
                    }
                    try {
                        const handle = await root.getFileHandle(name);
                        const file = await handle.getFile();
                        stats.opfs.bytes += file.size;
                        if (isDatabaseResidue) {
                            stats.opfs.dbBytes += file.size;
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
 * Format bytes for display
 */
export function formatBytes(bytes) {
    const value = Number(bytes);
    if (!Number.isFinite(value) || value <= 0) return '0 B';

    const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
    const i = Math.min(
        Math.floor(Math.log(value) / Math.log(1024)),
        units.length - 1
    );
    const size = value / Math.pow(1024, i);

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
    setItem,
    getItem,
    removeItem,
    clearCurrentStorage,
    clearOPFS,
    clearAllStorage,
    clearServiceWorkerCache,
    unregisterServiceWorker,
    getStorageStats,
    formatBytes,
    getArchiveScopeUrl,
    getArchiveScopeId,
};
