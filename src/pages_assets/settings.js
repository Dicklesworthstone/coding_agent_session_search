/**
 * cass Archive Viewer - Settings Panel Module
 *
 * Provides the settings UI for storage mode selection, OPFS opt-in,
 * cache management, and session controls.
 *
 * Security model:
 *   - Memory mode is default (most secure)
 *   - Clear warnings about persistence tradeoffs
 *   - Easy cache clearing and session reset
 */

import {
    StorageMode,
    StorageKeys,
    getStorageMode,
    setStorageMode,
    isOPFSAvailable,
    isOpfsEnabled,
    setOpfsEnabled,
    clearCurrentStorage,
    clearOPFS,
    clearAllStorage,
    clearServiceWorkerCache,
    unregisterServiceWorker,
    getStorageStats,
    formatBytes,
} from './storage.js';

// Module state
let settingsContainer = null;
let onSessionReset = null;
let settingsRenderEpoch = 0;

function getEffectiveStorageMode() {
    const mode = getStorageMode();
    if (mode === StorageMode.OPFS) {
        return StorageMode.MEMORY;
    }
    return mode;
}

/**
 * Initialize settings module
 * @param {HTMLElement} container - Container element for settings panel
 * @param {Object} options - Configuration options
 * @param {Function} options.onSessionReset - Callback when session is reset
 */
export function initSettings(container, options = {}) {
    settingsRenderEpoch += 1;
    settingsContainer = container;
    onSessionReset = options.onSessionReset || null;

    // Initial render
    render();
}

/**
 * Render the settings panel
 */
export async function render() {
    if (!settingsContainer) return;

    const epoch = settingsRenderEpoch;
    const targetContainer = settingsContainer;

    const currentMode = getEffectiveStorageMode();
    const opfsAvailable = isOPFSAvailable();
    const opfsEnabled = opfsAvailable && isOpfsEnabled();
    const stats = await getStorageStats();

    if (
        epoch !== settingsRenderEpoch
        || settingsContainer !== targetContainer
        || !targetContainer?.isConnected
    ) {
        return;
    }

    targetContainer.innerHTML = `
        <div class="panel settings-panel">
            <header class="panel-header">
                <h2>Settings</h2>
            </header>
            <div class="panel-content">
                <!-- Storage Mode Section -->
                <section class="settings-section">
                    <h3>Session Storage Mode</h3>
                    <p class="settings-description">
                        Control how your session data is stored. More persistent options
                        may improve performance but reduce security.
                    </p>

                    <div class="setting-item storage-mode-selector">
                        <div class="radio-group">
                            <label class="radio-option ${currentMode === StorageMode.MEMORY ? 'selected' : ''}">
                                <input type="radio" name="storage-mode" value="memory"
                                    ${currentMode === StorageMode.MEMORY ? 'checked' : ''}>
                                <span class="radio-label">
                                    <strong>Memory Only</strong>
                                    <span class="radio-badge badge-secure">Most Secure</span>
                                </span>
                                <span class="radio-description">
                                    Data cleared when page closes. Best for sensitive archives.
                                </span>
                            </label>

                            <label class="radio-option ${currentMode === StorageMode.SESSION ? 'selected' : ''}">
                                <input type="radio" name="storage-mode" value="session"
                                    ${currentMode === StorageMode.SESSION ? 'checked' : ''}>
                                <span class="radio-label">
                                    <strong>Session Storage</strong>
                                </span>
                                <span class="radio-description">
                                    Survives page refresh, cleared when tab closes.
                                </span>
                            </label>

                            <label class="radio-option ${currentMode === StorageMode.LOCAL ? 'selected' : ''}">
                                <input type="radio" name="storage-mode" value="local"
                                    ${currentMode === StorageMode.LOCAL ? 'checked' : ''}>
                                <span class="radio-label">
                                    <strong>Local Storage</strong>
                                    <span class="radio-badge badge-warning">Less Secure</span>
                                </span>
                                <span class="radio-description">
                                    Persists across sessions. Only use on trusted devices.
                                </span>
                            </label>
                        </div>
                    </div>
                </section>

                <!-- OPFS Section -->
                <section class="settings-section">
                    <h3>Database Caching (OPFS)</h3>
                    ${opfsAvailable ? `
                        <p class="settings-description">
                            Cache the decrypted database locally for faster browsing after unlock.
                            This cache stays in this browser profile until you clear it.
                        </p>

                        <div class="setting-item">
                            <label class="toggle-switch">
                                <input type="checkbox" id="opfs-toggle"
                                    ${opfsEnabled ? 'checked' : ''}>
                                <span class="toggle-slider"></span>
                                <span class="toggle-label">Remember on this device</span>
                            </label>
                        </div>

                        ${opfsEnabled ? `
                            <div class="settings-warning">
                                <span class="warning-icon">⚠️</span>
                                <span>Database is cached locally. Clear cache when done on shared devices.</span>
                            </div>
                        ` : ''}
                    ` : `
                        <p class="settings-description">
                            Your browser does not support OPFS (Origin Private File System).
                            The database will be decrypted fresh on each visit.
                        </p>
                    `}
                </section>

                <!-- Cache Management Section -->
                <section class="settings-section">
                    <h3>Cache Management</h3>

                    <div class="cache-stats">
                        <h4>Current Usage</h4>
                        <div class="stats-grid">
                            <div class="stat-item">
                                <span class="stat-label">Memory</span>
                                <span class="stat-value">${stats.memory.items} items (${formatBytes(stats.memory.bytes)})</span>
                            </div>
                            <div class="stat-item">
                                <span class="stat-label">Session</span>
                                <span class="stat-value">${stats.session.items} items (${formatBytes(stats.session.bytes)})</span>
                            </div>
                            <div class="stat-item">
                                <span class="stat-label">Local</span>
                                <span class="stat-value">${stats.local.items} items (${formatBytes(stats.local.bytes)})</span>
                            </div>
                            ${opfsAvailable ? `
                                <div class="stat-item">
                                    <span class="stat-label">OPFS</span>
                                    <span class="stat-value">${stats.opfs.items} items (${formatBytes(stats.opfs.bytes)})</span>
                                </div>
                                <div class="stat-item">
                                    <span class="stat-label">OPFS DB</span>
                                    <span class="stat-value">${formatBytes(stats.opfs.dbBytes || 0)} (${stats.opfs.dbFiles.length} files)</span>
                                </div>
                            ` : ''}
                            ${stats.quota ? `
                                <div class="stat-item stat-quota">
                                    <span class="stat-label">Storage Quota</span>
                                    <span class="stat-value">${formatBytes(stats.quota.usage || 0)} / ${formatBytes(stats.quota.quota || 0)}</span>
                                </div>
                            ` : ''}
                        </div>
                    </div>

                    <div class="cache-actions">
                        <button type="button" class="btn btn-secondary" id="clear-current-cache-btn">
                            Clear Current Storage
                        </button>
                        <button type="button" class="btn btn-secondary" id="clear-opfs-btn" ${!opfsAvailable ? 'disabled' : ''}>
                            Clear OPFS Cache
                        </button>
                        <button type="button" class="btn btn-secondary" id="clear-sw-cache-btn">
                            Clear Service Worker Cache
                        </button>
                        <button type="button" class="btn btn-danger" id="clear-all-btn">
                            Clear All Data
                        </button>
                    </div>
                </section>

                <!-- Session Controls Section -->
                <section class="settings-section">
                    <h3>Session Controls</h3>

                    <div class="setting-item">
                        <button type="button" class="btn btn-warning" id="lock-session-btn">
                            Lock Session
                        </button>
                        <p class="setting-description">
                            Forget the decryption key. You'll need to enter your password again.
                        </p>
                    </div>

                    <div class="setting-item">
                        <button type="button" class="btn btn-danger" id="reset-session-btn">
                            Reset Everything
                        </button>
                        <p class="setting-description">
                            Clear all data and unregister service workers. Like a fresh install.
                        </p>
                    </div>
                </section>

                <!-- Display Section -->
                <section class="settings-section">
                    <h3>Display</h3>
                    <div class="setting-item">
                        <label for="theme-select">Theme</label>
                        <select id="theme-select" class="settings-select">
                            <option value="auto">Auto (System)</option>
                            <option value="light">Light</option>
                            <option value="dark">Dark</option>
                        </select>
                    </div>
                </section>

                <!-- About Section -->
                <section class="settings-section">
                    <h3>About</h3>
                    <p class="settings-info">
                        <strong>cass Archive Viewer</strong><br>
                        <small>Viewing exported conversations from cass (coding agent session search)</small>
                    </p>
                    <p class="settings-info">
                        <small>
                            All data is encrypted with AES-256-GCM. Your password never leaves this browser.
                        </small>
                    </p>
                </section>
            </div>
        </div>
    `;

    // Set up event handlers
    setupEventHandlers(targetContainer);
}

/**
 * Set up settings event handlers
 */
function setupEventHandlers(root) {
    // Storage mode radio buttons
    const modeRadios = root.querySelectorAll('input[name="storage-mode"]');
    modeRadios.forEach(radio => {
        radio.addEventListener('change', handleStorageModeChange);
    });

    // OPFS toggle
    const opfsToggle = root.querySelector('#opfs-toggle');
    if (opfsToggle) {
        opfsToggle.addEventListener('change', handleOPFSToggle);
    }

    // Clear current storage
    const clearCurrentBtn = root.querySelector('#clear-current-cache-btn');
    if (clearCurrentBtn) {
        clearCurrentBtn.addEventListener('click', handleClearCurrentStorage);
    }

    // Clear OPFS
    const clearOPFSBtn = root.querySelector('#clear-opfs-btn');
    if (clearOPFSBtn) {
        clearOPFSBtn.addEventListener('click', handleClearOPFS);
    }

    // Clear SW cache
    const clearSWBtn = root.querySelector('#clear-sw-cache-btn');
    if (clearSWBtn) {
        clearSWBtn.addEventListener('click', handleClearSWCache);
    }

    // Clear all
    const clearAllBtn = root.querySelector('#clear-all-btn');
    if (clearAllBtn) {
        clearAllBtn.addEventListener('click', handleClearAll);
    }

    // Lock session
    const lockBtn = root.querySelector('#lock-session-btn');
    if (lockBtn) {
        lockBtn.addEventListener('click', handleLockSession);
    }

    // Reset session
    const resetBtn = root.querySelector('#reset-session-btn');
    if (resetBtn) {
        resetBtn.addEventListener('click', handleResetSession);
    }

    // Theme select
    const themeSelect = root.querySelector('#theme-select');
    if (themeSelect) {
        // Load saved theme
        let savedTheme = 'auto';
        try {
            savedTheme = localStorage.getItem(StorageKeys.THEME) || 'auto';
        } catch (e) {
            // Ignore storage errors
        }
        themeSelect.value = savedTheme;
        applyTheme(savedTheme);

        themeSelect.addEventListener('change', (e) => {
            const theme = e.target.value;
            try {
                localStorage.setItem(StorageKeys.THEME, theme);
            } catch (err) {
                // Ignore storage errors
            }
            applyTheme(theme);
            showNotification('Theme updated', 'success');
        });
    }
}

/**
 * Handle storage mode change
 */
async function handleStorageModeChange(e) {
    const newMode = e.target.value;
    const currentMode = getEffectiveStorageMode();

    if (newMode === currentMode) return;

    // Warn about security implications
    if (newMode === StorageMode.LOCAL) {
        const confirmed = confirm(
            'Warning: Local Storage persists data even after closing the browser.\n\n' +
            'Only use this on personal, trusted devices.\n\n' +
            'Continue?'
        );
        if (!confirmed) {
            // Reset radio to current mode
            const currentRadio = settingsContainer.querySelector(`input[name="storage-mode"][value="${currentMode}"]`);
            if (currentRadio) currentRadio.checked = true;
            return;
        }
    }

    try {
        await setStorageMode(newMode);
        window.dispatchEvent(new CustomEvent('cass:session-mode-change', { detail: { mode: newMode } }));
        showNotification(`Storage mode changed to ${newMode}`, 'success');
        await render();
    } catch (err) {
        console.error('[Settings] Failed to change storage mode:', err);
        showNotification('Failed to change storage mode', 'error');
    }
}

/**
 * Handle OPFS toggle
 */
async function handleOPFSToggle(e) {
    const enabled = e.target.checked;

    if (enabled) {
        const confirmed = confirm(
            'Enable database caching?\n\n' +
            'The decrypted database will be stored locally for faster loading.\n' +
            'Remember to clear the cache when done on shared devices.'
        );

        if (!confirmed) {
            e.target.checked = false;
            return;
        }

        try {
            setOpfsEnabled(true);
            showNotification('OPFS caching enabled', 'success');
        } catch (err) {
            console.error('[Settings] Failed to enable OPFS:', err);
            e.target.checked = false;
            showNotification('Failed to enable OPFS caching', 'error');
        }
    } else {
        // Switching away from OPFS - clear it first
        try {
            const opfsCleared = await clearOPFS();
            if (!opfsCleared) {
                showNotification('Failed to disable OPFS caching because cached files could not be fully cleared', 'error');
                await render();
                return;
            }
            setOpfsEnabled(false);
            showNotification('OPFS caching disabled and cleared', 'success');
        } catch (err) {
            console.error('[Settings] Failed to disable OPFS:', err);
            showNotification('Failed to disable OPFS caching', 'error');
        }
    }

    try {
        await render();
    } catch (err) {
        console.error('[Settings] Failed to refresh settings after OPFS toggle:', err);
        showNotification('Failed to refresh settings', 'error');
    }
}

/**
 * Handle clear current storage
 */
async function handleClearCurrentStorage() {
    const mode = getEffectiveStorageMode();
    const confirmed = confirm(`Clear all data in ${mode} storage?`);

    if (!confirmed) return;

    try {
        const storageCleared = await clearCurrentStorage();
        if (!storageCleared) {
            showNotification('Failed to clear current storage', 'error');
            return;
        }

        if (mode === StorageMode.MEMORY && onSessionReset) {
            onSessionReset('clear-current-storage');
            showNotification('Current memory storage cleared and session locked', 'success');
            return;
        }

        showNotification('Current storage cleared', 'success');
        await render();
    } catch (err) {
        console.error('[Settings] Failed to clear storage:', err);
        showNotification('Failed to clear storage', 'error');
    }
}

/**
 * Handle clear OPFS
 */
async function handleClearOPFS() {
    const confirmed = confirm(
        'Clear this archive\'s OPFS cache?\n\n' +
        'This archive\'s cached database will be deleted. You\'ll need to decrypt again on next visit.'
    );

    if (!confirmed) return;

    try {
        const opfsCleared = await clearOPFS();
        if (!opfsCleared) {
            showNotification('Failed to clear OPFS cache', 'error');
            return;
        }

        showNotification('OPFS cache cleared', 'success');
        await render();
    } catch (err) {
        console.error('[Settings] Failed to clear OPFS:', err);
        showNotification('Failed to clear OPFS', 'error');
    }
}

/**
 * Handle clear service worker cache
 */
async function handleClearSWCache() {
    const confirmed = confirm(
        'Clear this archive\'s Service Worker cache?\n\n' +
        'This archive\'s static assets will be re-downloaded on next visit.'
    );

    if (!confirmed) return;

    try {
        const cacheCleared = await clearServiceWorkerCache();
        if (!cacheCleared) {
            showNotification('Failed to clear Service Worker cache', 'error');
            return;
        }
        showNotification('Service Worker cache cleared', 'success');
    } catch (err) {
        console.error('[Settings] Failed to clear SW cache:', err);
        showNotification('Failed to clear SW cache', 'error');
    }
}

/**
 * Handle clear all data
 */
async function handleClearAll() {
    const confirmed = confirm(
        'Clear all data for this archive?\n\n' +
        'This will clear:\n' +
        '- This archive\'s storage (memory, session, local, OPFS)\n' +
        '- This archive\'s Service Worker caches\n\n' +
        'This cannot be undone.'
    );

    if (!confirmed) return;

    try {
        const storageCleared = await clearAllStorage();
        await setStorageMode(StorageMode.MEMORY);
        setOpfsEnabled(false);
        window.dispatchEvent(new CustomEvent('cass:session-mode-change', { detail: { mode: StorageMode.MEMORY } }));
        if (onSessionReset) {
            onSessionReset('clear-all');
        }

        const cacheCleared = await clearServiceWorkerCache();
        if (!storageCleared || !cacheCleared) {
            const failedSteps = [];
            if (!storageCleared) {
                failedSteps.push('stored data');
            }
            if (!cacheCleared) {
                failedSteps.push('Service Worker cache');
            }

            showNotification(`Archive data cleared and session locked, but ${failedSteps.join(' and ')} could not be fully cleared`, 'error');
            return;
        }

        showNotification('All data cleared and session locked', 'success');
    } catch (err) {
        console.error('[Settings] Failed to clear all:', err);
        showNotification('Failed to clear all data', 'error');
    }
}

/**
 * Handle lock session
 */
function handleLockSession() {
    const confirmed = confirm(
        'Lock session?\n\n' +
        'The decryption key will be forgotten. You\'ll need to enter your password again.'
    );

    if (!confirmed) return;

    if (onSessionReset) {
        onSessionReset('lock');
    }

    showNotification('Session locked', 'success');
}

/**
 * Handle reset session
 */
async function handleResetSession() {
    const confirmed = confirm(
        'Reset this archive?\n\n' +
        'This will:\n' +
        '- Clear this archive\'s data\n' +
        '- Unregister this archive\'s Service Worker\n' +
        '- Reload the page\n\n' +
        'Are you sure?'
    );

    if (!confirmed) return;

    try {
        const storageCleared = await clearAllStorage();
        await setStorageMode(StorageMode.MEMORY);
        setOpfsEnabled(false);
        window.dispatchEvent(new CustomEvent('cass:session-mode-change', { detail: { mode: StorageMode.MEMORY } }));
        if (onSessionReset) {
            onSessionReset('reset');
        }

        const cacheCleared = await clearServiceWorkerCache();
        const swUnregistered = await unregisterServiceWorker();
        if (!storageCleared || !cacheCleared || !swUnregistered) {
            const failedSteps = [];
            if (!storageCleared) {
                failedSteps.push('stored data');
            }
            if (!cacheCleared) {
                failedSteps.push('Service Worker cache');
            }
            if (!swUnregistered) {
                failedSteps.push('Service Worker registration');
            }

            showNotification(`Archive data cleared and session locked, but ${failedSteps.join(' and ')} could not be fully reset`, 'error');
            return;
        }

        showNotification('Resetting...', 'success');

        // Reload after a brief delay
        setTimeout(() => {
            window.location.reload();
        }, 500);
    } catch (err) {
        console.error('[Settings] Failed to reset:', err);
        showNotification('Failed to reset', 'error');
    }
}

/**
 * Apply theme
 */
function applyTheme(theme) {
    const root = document.documentElement;

    if (theme === 'auto') {
        root.removeAttribute('data-theme');
    } else {
        root.setAttribute('data-theme', theme);
    }
}

/**
 * Show notification
 */
function showNotification(message, type = 'info') {
    // Check if there's a global notification function
    if (typeof window.showNotification === 'function') {
        window.showNotification(message, type);
        return;
    }

    // Fallback: create simple toast
    const toast = document.createElement('div');
    toast.className = `toast toast-${type}`;
    toast.textContent = message;

    document.body.appendChild(toast);

    // Animate in
    requestAnimationFrame(() => {
        toast.classList.add('show');
    });

    // Remove after delay
    setTimeout(() => {
        toast.classList.remove('show');
        setTimeout(() => toast.remove(), 300);
    }, 3000);
}

export function cleanupSettings() {
    settingsRenderEpoch += 1;
    settingsContainer = null;
    onSessionReset = null;
}

// Export module
export default {
    initSettings,
    render,
    cleanupSettings,
};
