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
    return [sessionStorage, localStorage].filter((storage) => typeof storage !== 'undefined');
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
        this.duration = options.duration || SESSION_CONFIG.DEFAULT_DURATION_MS;
        this.storage = options.storage || SESSION_CONFIG.STORAGE_SESSION;
        this.onExpired = options.onExpired || (() => {});
        this.onWarning = options.onWarning || (() => {});

        this.dek = null;              // Current DEK (in memory)
        this.expiryTs = 0;            // Current session expiry timestamp
        this.persistent = false;      // Whether the session is persisted in storage
        this.expiryTimeout = null;    // Expiry timer
        this.warningTimeout = null;   // Warning timer
        this.memoryStorage = new MemoryStorage();

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
        this.clearStorage();
        this.dek = dek;

        const expiry = Date.now() + this.duration;
        this.expiryTs = expiry;
        this.persistent = rememberMe && this.storage !== SESSION_CONFIG.STORAGE_MEMORY;

        if (this.persistent) {
            const storage = this.getStorage();
            const sessionKeys = getScopedSessionKeys();
            storage.setItem(sessionKeys.TOKEN, encodeBytes(dek));
            storage.setItem(sessionKeys.EXPIRY, expiry.toString());
        }

        // Set timers
        this.setTimers(expiry);

        // Set up cleanup handlers
        this.setupCleanupHandlers();

        console.log(`[Session] Started, expires at ${new Date(expiry).toISOString()}`);
    }

    /**
     * Attempt to restore a previous session
     * @returns {Uint8Array|null} The DEK if restored, null otherwise
     */
    async restoreSession() {
        const storage = this.getStorage();
        const sessionKeys = getScopedSessionKeys();
        const token = storage.getItem(sessionKeys.TOKEN);
        const expiry = parseInt(
            storage.getItem(sessionKeys.EXPIRY) || '0',
            10
        );

        if (!token || Date.now() > expiry) {
            console.log('[Session] No valid session to restore');
            this.clearStorage();
            return null;
        }

        try {
            const dek = decodeBytes(token);
            this.dek = dek;
            this.expiryTs = expiry;
            this.persistent = true;

            // Reset timers with remaining time
            this.setTimers(expiry);

            console.log(`[Session] Restored, expires at ${new Date(expiry).toISOString()}`);
            return dek;
        } catch (error) {
            console.error('[Session] Failed to restore:', error);
            this.clearStorage();
            return null;
        }
    }

    /**
     * End the current session and cleanup
     */
    endSession() {
        console.log('[Session] Ending session');

        // Clear DEK from memory (zeroize)
        if (this.dek) {
            this.dek.fill(0);
            this.dek = null;
        }

        // Clear timers
        this.clearTimers();
        this.expiryTs = 0;
        this.persistent = false;

        // Clear storage
        this.clearStorage();

        // Remove cleanup handlers
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

        const extension = additionalMs || this.duration;
        const storage = this.getStorage();

        // Calculate new expiry
        const sessionKeys = getScopedSessionKeys();
        const currentExpiry = this.expiryTs || parseInt(storage.getItem(sessionKeys.EXPIRY) || '0', 10);
        const newExpiry = Math.max(Date.now(), currentExpiry) + extension;
        this.expiryTs = newExpiry;

        // Update storage
        if (this.persistent) {
            storage.setItem(sessionKeys.EXPIRY, newExpiry.toString());
        }

        // Reset timers
        this.setTimers(newExpiry);

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
        this.clearTimers();

        const remaining = expiry - Date.now();

        // Expiry timer
        if (remaining > 0) {
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
    }

    /**
     * Clear all timers
     */
    clearTimers() {
        if (this.expiryTimeout) {
            clearTimeout(this.expiryTimeout);
            this.expiryTimeout = null;
        }
        if (this.warningTimeout) {
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
                return typeof localStorage !== 'undefined' ? localStorage : this.memoryStorage;
            case SESSION_CONFIG.STORAGE_SESSION:
                return typeof sessionStorage !== 'undefined' ? sessionStorage : this.memoryStorage;
            case SESSION_CONFIG.STORAGE_MEMORY:
            default:
                return this.memoryStorage;
        }
    }

    /**
     * Clear all session data from storage
     */
    clearStorage() {
        const sessionKeys = getScopedSessionKeys();
        for (const storage of getPersistentStorages()) {
            storage.removeItem(sessionKeys.TOKEN);
            storage.removeItem(sessionKeys.EXPIRY);
        }
    }

    /**
     * Set up cleanup handlers for page visibility and unload
     */
    setupCleanupHandlers() {
        document.addEventListener('visibilitychange', this.handleVisibilityChange);
        window.addEventListener('beforeunload', this.handleBeforeUnload);
    }

    /**
     * Remove cleanup handlers
     */
    removeCleanupHandlers() {
        document.removeEventListener('visibilitychange', this.handleVisibilityChange);
        window.removeEventListener('beforeunload', this.handleBeforeUnload);
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
        // Zeroize DEK on page unload for memory-only sessions
        if (this.storage === SESSION_CONFIG.STORAGE_MEMORY && this.dek) {
            this.dek.fill(0);
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
        this.idleTimeout = options.idleTimeout || SESSION_CONFIG.IDLE_TIMEOUT_MS;
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
        duration: options.duration || SESSION_CONFIG.DEFAULT_DURATION_MS,
        storage: options.storage || SESSION_CONFIG.STORAGE_SESSION,
        onExpired: options.onExpired,
        onWarning: options.onWarning,
    });

    const activity = new ActivityMonitor(session, {
        idleTimeout: options.idleTimeout || SESSION_CONFIG.IDLE_TIMEOUT_MS,
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
