/**
 * cass Archive Session Management
 *
 * Handles session lifecycle, key storage, and activity monitoring.
 * Balances security with usability by supporting multiple storage options.
 */

import { getArchiveScopeId } from './storage.js';

// Session configuration
export const SESSION_CONFIG = {
    // Default session duration: 4 hours
    DEFAULT_DURATION_MS: 4 * 60 * 60 * 1000,

    // Warning before expiry: 5 minutes
    WARNING_BEFORE_MS: 5 * 60 * 1000,

    // Idle timeout for activity-based extension: 15 minutes
    IDLE_TIMEOUT_MS: 15 * 60 * 1000,

    // Storage options
    STORAGE_MEMORY: 'memory',       // Most secure, lost on refresh
    STORAGE_SESSION: 'session',     // Survives refresh, not tabs
    STORAGE_LOCAL: 'local',         // Persists across sessions (least secure)

    // Storage key bases
    KEY_SESSION_TOKEN: 'cass_session',
    KEY_EXPIRY: 'cass_expiry',
    KEY_STORAGE_PREF: 'cass_storage_pref',
};

const SESSION_DEK_BYTES = 32;
// Browser timers use a signed 32-bit delay. Larger values may fire almost
// immediately instead of enforcing the intended expiry.
const MAX_TIMER_DELAY_MS = 2_147_483_647;
const VALID_STORAGE_MODES = new Set([
    SESSION_CONFIG.STORAGE_MEMORY,
    SESSION_CONFIG.STORAGE_SESSION,
    SESSION_CONFIG.STORAGE_LOCAL,
]);

function validateDuration(value, label = 'Session duration') {
    if (
        !Number.isSafeInteger(value)
        || value <= 0
        || value > MAX_TIMER_DELAY_MS
    ) {
        throw new RangeError(`${label} must be a positive safe integer no greater than ${MAX_TIMER_DELAY_MS}`);
    }
    return value;
}

function calculateExpiry(now, duration) {
    validateDuration(duration);
    const expiry = now + duration;
    if (!Number.isSafeInteger(expiry) || expiry <= now) {
        throw new RangeError('Session expiry is outside the safe timestamp range');
    }
    return expiry;
}

function parseStoredExpiry(value) {
    if (typeof value !== 'string' || !/^[1-9][0-9]*$/.test(value)) {
        return null;
    }
    const expiry = Number(value);
    return Number.isSafeInteger(expiry) ? expiry : null;
}

function isUsableFutureExpiry(expiry, now) {
    return Number.isSafeInteger(expiry)
        && expiry > now
        && expiry - now <= MAX_TIMER_DELAY_MS;
}

function validateDek(dek) {
    if (!(dek instanceof Uint8Array) || dek.byteLength !== SESSION_DEK_BYTES) {
        throw new TypeError(`Session DEK must be exactly ${SESSION_DEK_BYTES} bytes`);
    }
    return dek;
}

function writeAndVerify(storage, key, value) {
    storage.setItem(key, value);
    if (storage.getItem(key) !== value) {
        throw new Error(`Persistent session write verification failed for ${key}`);
    }
}

function getScopedSessionKeys() {
    const scopeId = getArchiveScopeId();
    return {
        TOKEN: `${SESSION_CONFIG.KEY_SESSION_TOKEN}_${scopeId}`,
        EXPIRY: `${SESSION_CONFIG.KEY_EXPIRY}_${scopeId}`,
        STORAGE_PREF: `${SESSION_CONFIG.KEY_STORAGE_PREF}_${scopeId}`,
    };
}

function encodeBytes(bytes) {
    return btoa(String.fromCharCode(...bytes));
}

function decodeBytes(base64) {
    return Uint8Array.from(atob(base64), (char) => char.charCodeAt(0));
}

function getPersistentStorages() {
    const storages = [];
    let accessFailed = false;

    try {
        if (typeof sessionStorage !== 'undefined') {
            storages.push({ name: 'sessionStorage', storage: sessionStorage });
        }
    } catch (error) {
        console.warn('[Session] Could not access sessionStorage during cleanup:', error);
        accessFailed = true;
    }

    try {
        if (typeof localStorage !== 'undefined') {
            storages.push({ name: 'localStorage', storage: localStorage });
        }
    } catch (error) {
        console.warn('[Session] Could not access localStorage during cleanup:', error);
        accessFailed = true;
    }

    return { storages, accessFailed };
}

/**
 * In-memory storage fallback
 */
class MemoryStorage {
    constructor() {
        this.data = new Map();
    }

    getItem(key) {
        return this.data.get(key) || null;
    }

    setItem(key, value) {
        this.data.set(key, value);
    }

    removeItem(key) {
        this.data.delete(key);
    }

    clear() {
        this.data.clear();
    }
}

/**
 * Session Manager
 *
 * Manages the session lifecycle, including key storage, expiry, and cleanup.
 */
export class SessionManager {
    constructor(options = {}) {
        this.duration = validateDuration(
            options.duration ?? SESSION_CONFIG.DEFAULT_DURATION_MS
        );
        this.storage = options.storage ?? SESSION_CONFIG.STORAGE_SESSION;
        if (!VALID_STORAGE_MODES.has(this.storage)) {
            throw new TypeError(`Unknown session storage mode: ${this.storage}`);
        }
        this.onExpired = options.onExpired || (() => {});
        this.onWarning = options.onWarning || (() => {});

        this.dek = null;              // Current DEK (in memory)
        this.expiryTs = 0;            // Current session expiry timestamp
        this.persistent = false;      // Whether the session is persisted in storage
        this.persistenceStorage = null; // Actual backend used for persisted state
        this.expiryTimeout = null;    // Expiry timer
        this.warningTimeout = null;   // Warning timer
        this.memoryStorage = new MemoryStorage();
        this.cleanupHandlersInstalled = false;

        // Bind methods for event handlers
        this.handleVisibilityChange = this.handleVisibilityChange.bind(this);
        this.handleBeforeUnload = this.handleBeforeUnload.bind(this);
    }

    /**
     * Start a new session with the derived DEK
     * @param {Uint8Array} dek - The Data Encryption Key
     * @param {boolean} rememberMe - Whether to persist the session
     */
    async startSession(dek, rememberMe = false) {
        validateDek(dek);
        const copiedActiveDek = this.dek === dek;
        const nextDek = copiedActiveDek ? new Uint8Array(dek) : dek;
        const expiry = calculateExpiry(Date.now(), this.duration);

        // Replacing a session must not leave the prior key or timers alive,
        // even if cleanup or persistence of the replacement later fails.
        this.clearActiveState();
        if (!this.clearStorage()) {
            if (copiedActiveDek) {
                nextDek.fill(0);
            }
            throw new Error('Previous persisted session data could not be fully cleared');
        }

        const storage = this.getStorage();
        const persistent = rememberMe === true && storage !== this.memoryStorage;

        if (persistent) {
            const sessionKeys = getScopedSessionKeys();
            try {
                const encodedDek = encodeBytes(nextDek);
                const encodedExpiry = expiry.toString();
                // Write expiry first and the token last as the commit marker.
                writeAndVerify(storage, sessionKeys.EXPIRY, encodedExpiry);
                writeAndVerify(storage, sessionKeys.TOKEN, encodedDek);
            } catch (error) {
                if (copiedActiveDek) {
                    nextDek.fill(0);
                }
                if (!this.clearStorage()) {
                    console.warn('[Session] Failed session start left persisted data that could not be fully cleared');
                }
                throw new Error('Failed to persist the new session', { cause: error });
            }
        }

        if (!isUsableFutureExpiry(expiry, Date.now())) {
            if (copiedActiveDek) {
                nextDek.fill(0);
            }
            if (!this.clearStorage()) {
                console.warn('[Session] Expired session start left persisted data that could not be fully cleared');
            }
            throw new Error('Session expired before it could be started');
        }

        try {
            this.setTimers(expiry);
        } catch (error) {
            if (copiedActiveDek) {
                nextDek.fill(0);
            }
            if (!this.clearStorage()) {
                console.warn('[Session] Unschedulable session start left persisted data that could not be fully cleared');
            }
            throw new Error('Failed to schedule the new session expiry', { cause: error });
        }

        // Publish active state only after persistent writes have committed.
        this.dek = nextDek;
        this.expiryTs = expiry;
        this.persistent = persistent;
        this.persistenceStorage = persistent ? storage : null;

        // Set up cleanup handlers
        this.setupCleanupHandlers();

        console.log(`[Session] Started, expires at ${new Date(expiry).toISOString()}`);
    }

    /**
     * Attempt to restore a previous session
     * @returns {Uint8Array|null} The DEK if restored, null otherwise
     */
    async restoreSession() {
        // A restore attempt supersedes any active in-memory session. Never let
        // an old key survive a failed or successful replacement attempt.
        this.clearActiveState();
        const storage = this.getStorage();
        const sessionKeys = getScopedSessionKeys();
        let restoredDek = null;

        try {
            const token = storage.getItem(sessionKeys.TOKEN);
            const expiry = parseStoredExpiry(storage.getItem(sessionKeys.EXPIRY));
            const now = Date.now();
            if (!token || !isUsableFutureExpiry(expiry, now)) {
                throw new Error('Persisted session has a missing or invalid expiry');
            }

            restoredDek = decodeBytes(token);
            validateDek(restoredDek);
            this.dek = restoredDek;
            this.expiryTs = expiry;
            this.persistent = storage !== this.memoryStorage;
            this.persistenceStorage = this.persistent ? storage : null;

            // Reset timers with remaining time
            this.setTimers(expiry);
            this.setupCleanupHandlers();

            console.log(`[Session] Restored, expires at ${new Date(expiry).toISOString()}`);
            return restoredDek;
        } catch (error) {
            console.error('[Session] Failed to restore:', error);
            if (restoredDek && this.dek !== restoredDek) {
                restoredDek.fill(0);
            }
            this.clearActiveState();
            if (!this.clearStorage()) {
                console.warn('[Session] Unrestorable persisted session data could not be fully cleared');
            }
            return null;
        }
    }

    /**
     * End the current session and cleanup
     * @returns {boolean} Whether all persisted session keys were removed
     */
    endSession() {
        console.log('[Session] Ending session');
        this.clearActiveState();

        let storageCleared = false;
        try {
            // Clear storage
            storageCleared = this.clearStorage();
        } finally {
            // Listener teardown must not depend on Web Storage availability.
            this.removeCleanupHandlers();
        }

        if (!storageCleared) {
            console.warn('[Session] Session ended, but persisted session data could not be fully cleared');
        }
        return storageCleared;
    }

    /**
     * Clear in-memory session state without touching persistent keys.
     */
    clearActiveState() {
        if (this.dek) {
            this.dek.fill(0);
            this.dek = null;
        }
        this.clearTimers();
        this.expiryTs = 0;
        this.persistent = false;
        this.persistenceStorage = null;
        this.removeCleanupHandlers();
    }

    /**
     * Extend the current session
     * @param {number} additionalMs - Additional time in milliseconds
     * @returns {boolean} Whether the extension was successful
     */
    extendSession(additionalMs = null) {
        if (!this.dek) {
            console.warn('[Session] No active session to extend');
            return false;
        }

        const extension = additionalMs ?? this.duration;
        try {
            validateDuration(extension, 'Session extension');
        } catch (error) {
            console.warn('[Session] Refusing invalid extension:', error);
            return false;
        }

        const now = Date.now();
        if (!isUsableFutureExpiry(this.expiryTs, now)) {
            console.warn('[Session] Refusing to extend an expired or invalid session');
            this.endSession();
            this.onExpired();
            return false;
        }

        let newExpiry;
        try {
            newExpiry = calculateExpiry(this.expiryTs, extension);
            if (!isUsableFutureExpiry(newExpiry, now)) {
                throw new RangeError('Extended session exceeds the safe timer horizon');
            }
        } catch (error) {
            console.warn('[Session] Refusing unsafe extension:', error);
            return false;
        }

        try {
            // Persist to the backend that actually committed this session,
            // then publish the new in-memory expiry and timer together.
            if (this.persistent) {
                if (!this.persistenceStorage) {
                    throw new Error('Persistent session has no committed storage backend');
                }
                const sessionKeys = getScopedSessionKeys();
                writeAndVerify(this.persistenceStorage, sessionKeys.EXPIRY, newExpiry.toString());
            }

            this.setTimers(newExpiry);
        } catch (error) {
            console.error('[Session] Failed to extend session:', error);
            // A failed or unverifiable storage write has an ambiguous durable
            // result. Fail closed rather than allowing memory and disk expiry
            // state to diverge and resurrect a longer session on reload.
            this.endSession();
            return false;
        }

        this.expiryTs = newExpiry;

        console.log(`[Session] Extended to ${new Date(newExpiry).toISOString()}`);
        return true;
    }

    /**
     * Get the current DEK
     * @returns {Uint8Array|null}
     */
    getDek() {
        return this.dek;
    }

    /**
     * Check if a session is active
     * @returns {boolean}
     */
    isActive() {
        return this.dek !== null;
    }

    /**
     * Get remaining session time in milliseconds
     * @returns {number}
     */
    getRemainingTime() {
        return Math.max(0, this.expiryTs - Date.now());
    }

    /**
     * Set expiry and warning timers
     */
    setTimers(expiry) {
        const now = Date.now();
        if (!isUsableFutureExpiry(expiry, now)) {
            throw new RangeError('Cannot schedule an expired or unsafe session expiry');
        }

        this.clearTimers();
        const remaining = expiry - now;

        this.expiryTimeout = setTimeout(() => {
            this.endSession();
            this.onExpired();
        }, remaining);

        // Warning timer
        const warningTime = remaining - SESSION_CONFIG.WARNING_BEFORE_MS;
        if (warningTime > 0) {
            this.warningTimeout = setTimeout(() => {
                this.onWarning(SESSION_CONFIG.WARNING_BEFORE_MS);
            }, warningTime);
        }
    }

    /**
     * Clear all timers
     */
    clearTimers() {
        if (this.expiryTimeout !== null) {
            clearTimeout(this.expiryTimeout);
            this.expiryTimeout = null;
        }
        if (this.warningTimeout !== null) {
            clearTimeout(this.warningTimeout);
            this.warningTimeout = null;
        }
    }

    /**
     * Get the appropriate storage based on preference
     */
    getStorage() {
        switch (this.storage) {
            case SESSION_CONFIG.STORAGE_LOCAL:
                try {
                    if (typeof localStorage !== 'undefined') {
                        return localStorage;
                    }
                } catch (error) {
                    // Fall back to memory-only storage below.
                }
                return this.memoryStorage;
            case SESSION_CONFIG.STORAGE_SESSION:
                try {
                    if (typeof sessionStorage !== 'undefined') {
                        return sessionStorage;
                    }
                } catch (error) {
                    // Fall back to memory-only storage below.
                }
                return this.memoryStorage;
            case SESSION_CONFIG.STORAGE_MEMORY:
            default:
                return this.memoryStorage;
        }
    }

    /**
     * Clear all session data from storage
     * @returns {boolean} Whether every session key was removed from every accessible backend
     */
    clearStorage() {
        const sessionKeys = getScopedSessionKeys();
        const keys = new Set([
            sessionKeys.TOKEN,
            sessionKeys.EXPIRY,
            // Clear sensitive keys written by pre-scope viewer versions too.
            SESSION_CONFIG.KEY_SESSION_TOKEN,
            SESSION_CONFIG.KEY_EXPIRY,
        ]);
        let cleared = true;

        for (const key of keys) {
            this.memoryStorage.removeItem(key);
            if (this.memoryStorage.getItem(key) !== null) {
                cleared = false;
            }
        }

        const { storages, accessFailed } = getPersistentStorages();
        cleared = !accessFailed && cleared;
        for (const { name, storage } of storages) {
            for (const key of keys) {
                try {
                    storage.removeItem(key);
                    if (storage.getItem(key) !== null) {
                        cleared = false;
                    }
                } catch (error) {
                    console.warn(`[Session] Could not clear ${key} from ${name}:`, error);
                    cleared = false;
                }
            }
        }

        return cleared;
    }

    /**
     * Set up cleanup handlers for page visibility and unload
     */
    setupCleanupHandlers() {
        if (this.cleanupHandlersInstalled) {
            return;
        }
        document.addEventListener('visibilitychange', this.handleVisibilityChange);
        window.addEventListener('beforeunload', this.handleBeforeUnload);
        this.cleanupHandlersInstalled = true;
    }

    /**
     * Remove cleanup handlers
     */
    removeCleanupHandlers() {
        if (!this.cleanupHandlersInstalled) {
            return;
        }
        document.removeEventListener('visibilitychange', this.handleVisibilityChange);
        window.removeEventListener('beforeunload', this.handleBeforeUnload);
        this.cleanupHandlersInstalled = false;
    }

    /**
     * Handle page visibility change
     */
    handleVisibilityChange() {
        if (document.hidden) {
            // Page is hidden - could be used to pause timers
            console.log('[Session] Page hidden');
        } else {
            // Page is visible - check session validity
            console.log('[Session] Page visible');
            const remaining = this.getRemainingTime();
            if (remaining <= 0 && this.dek) {
                this.endSession();
                this.onExpired();
            }
        }
    }

    /**
     * Handle page unload
     */
    handleBeforeUnload() {
        // Zeroize any session that was not actually committed to persistent
        // storage, including configured session/local modes whose backend was
        // unavailable or whose caller chose not to remember the session.
        if (!this.persistent && this.dek) {
            this.clearActiveState();
        }
    }
}

/**
 * Activity Monitor
 *
 * Extends session on user activity to prevent premature expiry.
 */
export class ActivityMonitor {
    constructor(sessionManager, options = {}) {
        this.session = sessionManager;
        this.idleTimeout = validateDuration(
            options.idleTimeout ?? SESSION_CONFIG.IDLE_TIMEOUT_MS,
            'Activity idle timeout'
        );
        this.lastActivity = Date.now();
        this.enabled = false;

        // Bind method for event handlers
        this.onActivity = this.onActivity.bind(this);
    }

    /**
     * Start monitoring user activity
     */
    start() {
        if (this.enabled) return;

        const events = ['mousedown', 'keydown', 'scroll', 'touchstart', 'mousemove'];
        events.forEach(event => {
            document.addEventListener(event, this.onActivity, { passive: true });
        });

        this.enabled = true;
        console.log('[Activity] Monitoring started');
    }

    /**
     * Stop monitoring user activity
     */
    stop() {
        if (!this.enabled) return;

        const events = ['mousedown', 'keydown', 'scroll', 'touchstart', 'mousemove'];
        events.forEach(event => {
            document.removeEventListener(event, this.onActivity);
        });

        this.enabled = false;
        console.log('[Activity] Monitoring stopped');
    }

    /**
     * Handle user activity
     */
    onActivity() {
        const now = Date.now();

        // Extend session if user was idle
        if (now - this.lastActivity > this.idleTimeout) {
            console.log('[Activity] User returned from idle, extending session');
            this.session.extendSession();
        }

        this.lastActivity = now;
    }

    /**
     * Get time since last activity
     */
    getIdleTime() {
        return Date.now() - this.lastActivity;
    }
}

/**
 * Create a default session manager with activity monitoring
 */
export function createSessionManager(options = {}) {
    const session = new SessionManager({
        duration: options.duration ?? SESSION_CONFIG.DEFAULT_DURATION_MS,
        storage: options.storage ?? SESSION_CONFIG.STORAGE_SESSION,
        onExpired: options.onExpired,
        onWarning: options.onWarning,
    });

    const activity = new ActivityMonitor(session, {
        idleTimeout: options.idleTimeout ?? SESSION_CONFIG.IDLE_TIMEOUT_MS,
    });

    return { session, activity };
}

// Export default instance
export default {
    SESSION_CONFIG,
    SessionManager,
    ActivityMonitor,
    createSessionManager,
};
