/**
 * cass Archive Cross-Origin Isolation Detector
 *
 * Detects and handles the two-load pattern required for SharedArrayBuffer:
 * - First load: Service Worker installs but COOP/COEP headers not yet applied
 * - Second load: Cross-origin isolated, SharedArrayBuffer available
 *
 * Provides graceful UX for each state:
 * - SW_INSTALLING: Show loading UI while SW installs
 * - NEEDS_RELOAD: Prompt user to reload for full functionality
 * - READY: Proceed to authentication
 * - DEGRADED: Continue with limited functionality
 */

// COI States
export const COI_STATE = {
    SW_INSTALLING: 'SW_INSTALLING',
    NEEDS_RELOAD: 'NEEDS_RELOAD',
    READY: 'READY',
    DEGRADED: 'DEGRADED',
};

/**
 * Check if we're cross-origin isolated
 * @returns {boolean}
 */
export function isCrossOriginIsolated() {
    return window.crossOriginIsolated === true;
}

/**
 * Check if Service Worker is installed and controlling
 * @returns {Promise<boolean>}
 */
export async function isServiceWorkerActive() {
    if (!('serviceWorker' in navigator)) return false;

    try {
        const registration = await navigator.serviceWorker.getRegistration();
        return registration?.active != null;
    } catch {
        return false;
    }
}

/**
 * Check if Service Worker is supported
 * @returns {boolean}
 */
export function isServiceWorkerSupported() {
    return 'serviceWorker' in navigator;
}

/**
 * Check if SharedArrayBuffer is available (definitive test for COOP/COEP)
 * @returns {boolean}
 */
export function isSharedArrayBufferAvailable() {
    try {
        new SharedArrayBuffer(1);
        return true;
    } catch {
        return false;
    }
}

/**
 * Determine current COI state
 * @returns {Promise<string>} One of COI_STATE values
 */
export async function getCOIState() {
    // If SW not supported, we're in degraded mode
    if (!isServiceWorkerSupported()) {
        console.log('[COI] Service Workers not supported - degraded mode');
        return COI_STATE.DEGRADED;
    }

    const swActive = await isServiceWorkerActive();
    const coiEnabled = isCrossOriginIsolated();
    const sabAvailable = isSharedArrayBufferAvailable();

    console.log('[COI] State check:', { swActive, coiEnabled, sabAvailable });

    if (!swActive) {
        // SW not yet active - still installing
        return COI_STATE.SW_INSTALLING;
    }

    if (!coiEnabled || !sabAvailable) {
        // SW active but COI not yet enabled - needs reload
        return COI_STATE.NEEDS_RELOAD;
    }

    // Fully ready
    return COI_STATE.READY;
}

/**
 * Get recommended Argon2 configuration based on COI availability
 * @returns {Object} Configuration object
 */
export function getArgon2Config() {
    if (isSharedArrayBufferAvailable()) {
        return {
            parallelism: 4,   // Use all lanes for multi-threaded
            mode: 'wasm-mt',  // Multi-threaded WASM
            expectedTime: '1-3s',
        };
    } else {
        return {
            parallelism: 1,   // Single-threaded fallback
            mode: 'wasm-st',  // Single-threaded WASM
            expectedTime: '3-9s',
        };
    }
}

/**
 * Show installing UI while SW is being set up
 * @param {HTMLElement} container - Container to render into
 */
export function showInstallingUI(container) {
    container.innerHTML = `
        <div class="coi-status installing">
            <div class="coi-spinner"></div>
            <h3>Setting up secure environment...</h3>
            <p class="coi-detail">Installing security enhancements for fast, secure decryption</p>
        </div>
    `;
    container.classList.remove('hidden');
}

/**
 * Show reload required UI when COI needs a page refresh
 * @param {HTMLElement} container - Container to render into
 * @param {Function} [onReload] - Optional callback before reload
 */
export function showReloadRequiredUI(container, onReload = null) {
    container.innerHTML = `
        <div class="coi-status needs-reload">
            <div class="coi-icon">&#x1F504;</div>
            <h3>One-time Setup Required</h3>
            <p>To enable fast, secure decryption, please reload the page.</p>
            <button id="coi-reload-btn" class="btn btn-primary coi-reload-btn">
                Reload Now
            </button>
            <details class="coi-details">
                <summary>Why is this needed?</summary>
                <p>
                    Modern browsers require special security headers for
                    hardware-accelerated encryption. After reloading, the
                    archive will:
                </p>
                <ul>
                    <li>Decrypt 3-5x faster using parallel processing</li>
                    <li>Support offline access</li>
                    <li>Use enhanced memory protection</li>
                </ul>
                <p class="coi-note">You only need to do this once per browser session.</p>
            </details>
        </div>
    `;
    container.classList.remove('hidden');

    const reloadBtn = document.getElementById('coi-reload-btn');
    if (reloadBtn) {
        reloadBtn.addEventListener('click', () => {
            if (onReload) {
                onReload();
            }
            window.location.reload();
        });
    }
}

/**
 * Show degraded mode warning banner
 * Displayed when COI is not available but app can still function
 */
export function showDegradedModeWarning() {
    // Check if banner already exists
    if (document.querySelector('.coi-degraded-banner')) return;

    const banner = document.createElement('div');
    banner.className = 'coi-degraded-banner';
    banner.innerHTML = `
        <span class="coi-warning-icon">&#x26A0;&#xFE0F;</span>
        <span class="coi-warning-text">Running in compatibility mode - unlock may take longer</span>
        <button class="coi-dismiss-btn" aria-label="Dismiss">&#x2715;</button>
    `;

    const dismissBtn = banner.querySelector('.coi-dismiss-btn');
    if (dismissBtn) {
        dismissBtn.addEventListener('click', () => {
            banner.remove();
        });
    }

    document.body.prepend(banner);
}

/**
 * Hide COI status UI
 * @param {HTMLElement} container - Container to hide
 */
export function hideStatusUI(container) {
    container.classList.add('hidden');
    container.innerHTML = '';
}

/**
 * Initialize COI detection and handle states
 * @param {Object} options - Configuration options
 * @param {HTMLElement} options.statusContainer - Container for status UI
 * @param {HTMLElement} options.authContainer - Auth screen container
 * @param {Function} options.onReady - Callback when ready to proceed
 * @param {number} [options.maxWaitMs=5000] - Max time to wait for SW installation
 */
export async function initCOIDetection({
    statusContainer,
    authContainer,
    onReady,
    maxWaitMs = 5000,
}) {
    let state = await getCOIState();
    const startTime = Date.now();

    console.log('[COI] Initial state:', state);

    // Handle SW_INSTALLING state with timeout
    if (state === COI_STATE.SW_INSTALLING) {
        showInstallingUI(statusContainer);

        // Wait for SW to become active
        if ('serviceWorker' in navigator) {
            try {
                await Promise.race([
                    navigator.serviceWorker.ready,
                    new Promise((_, reject) =>
                        setTimeout(() => reject(new Error('SW timeout')), maxWaitMs)
                    ),
                ]);

                // Recheck state after SW is ready
                state = await getCOIState();
                console.log('[COI] State after SW ready:', state);
            } catch (error) {
                console.warn('[COI] SW wait timeout or error:', error.message);
                // Continue with current state
                state = await getCOIState();
            }
        }
    }

    // Handle final state
    switch (state) {
        case COI_STATE.READY:
            console.log('[COI] Ready - proceeding to auth');
            hideStatusUI(statusContainer);
            if (onReady) onReady();
            break;

        case COI_STATE.NEEDS_RELOAD:
            console.log('[COI] Needs reload - showing prompt');
            showReloadRequiredUI(statusContainer);
            // Hide auth screen while showing reload prompt
            if (authContainer) {
                authContainer.classList.add('hidden');
            }
            break;

        case COI_STATE.DEGRADED:
            console.log('[COI] Degraded mode - showing warning and proceeding');
            hideStatusUI(statusContainer);
            showDegradedModeWarning();
            if (onReady) onReady();
            break;

        case COI_STATE.SW_INSTALLING:
            // Still installing after timeout - check if we should show reload or proceed
            console.log('[COI] SW still installing - checking fallback');
            if (isSharedArrayBufferAvailable()) {
                // Already have SAB somehow (maybe browser feature)
                hideStatusUI(statusContainer);
                if (onReady) onReady();
            } else {
                // Show reload prompt as SW should be active soon
                showReloadRequiredUI(statusContainer);
                if (authContainer) {
                    authContainer.classList.add('hidden');
                }
            }
            break;
    }

    return state;
}

/**
 * Listen for SW activation and trigger recheck
 * @param {Function} callback - Called when SW activates
 */
export function onServiceWorkerActivated(callback) {
    if ('serviceWorker' in navigator) {
        navigator.serviceWorker.addEventListener('message', (event) => {
            if (event.data?.type === 'SW_ACTIVATED') {
                console.log('[COI] Received SW_ACTIVATED message');
                callback();
            }
        });

        navigator.serviceWorker.addEventListener('controllerchange', () => {
            console.log('[COI] Controller changed');
            callback();
        });
    }
}

// Export default
export default {
    COI_STATE,
    isCrossOriginIsolated,
    isServiceWorkerActive,
    isServiceWorkerSupported,
    isSharedArrayBufferAvailable,
    getCOIState,
    getArgon2Config,
    showInstallingUI,
    showReloadRequiredUI,
    showDegradedModeWarning,
    hideStatusUI,
    initCOIDetection,
    onServiceWorkerActivated,
};
