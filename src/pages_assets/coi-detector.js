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

let activeReloadController = null;

function hashScopeId(input) {
    let hash = 0x811c9dc5;
    for (let i = 0; i < input.length; i++) {
        hash ^= input.charCodeAt(i);
        hash = Math.imul(hash, 0x01000193) >>> 0;
    }
    return hash.toString(16).padStart(8, '0');
}

function getSetupCompleteKey() {
    try {
        return `cass-coi-setup-complete-${hashScopeId(new URL('./', window.location.href).href)}`;
    } catch {
        const href = typeof window?.location?.href === 'string'
            ? window.location.href
            : 'unknown';
        return `cass-coi-setup-complete-${hashScopeId(href.split('#')[0].split('?')[0])}`;
    }
}

/**
 * Check if COI setup has been completed before
 * @returns {boolean}
 */
export function isSetupComplete() {
    try {
        return localStorage.getItem(getSetupCompleteKey()) === 'true';
    } catch {
        return false;
    }
}

/**
 * Mark COI setup as complete
 */
export function markSetupComplete() {
    try {
        localStorage.setItem(getSetupCompleteKey(), 'true');
    } catch {
        // localStorage not available
    }
}

/**
 * Clear setup complete flag (for testing)
 */
export function clearSetupComplete() {
    try {
        localStorage.removeItem(getSetupCompleteKey());
    } catch {
        // localStorage not available
    }
}

async function getCurrentServiceWorkerRegistration() {
    if (!('serviceWorker' in navigator)) {
        return null;
    }

    try {
        return await navigator.serviceWorker.getRegistration();
    } catch {
        return null;
    }
}

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
    const registration = await getCurrentServiceWorkerRegistration();
    return registration?.active != null;
}

/**
 * Check if a service worker registration exists for this archive scope
 * @returns {Promise<boolean>}
 */
export async function hasServiceWorkerRegistration() {
    return (await getCurrentServiceWorkerRegistration()) != null;
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
 * Show installing UI with progress steps
 * @param {HTMLElement} container - Container to render into
 */
export function showInstallingUI(container) {
    container.innerHTML = `
        <div class="coi-status installing">
            <div class="coi-header">
                <span class="coi-logo" aria-hidden="true">&#x1F510;</span>
                <h3>Setting Up Secure Environment</h3>
            </div>
            <p class="coi-detail">One-time setup for fast, secure decryption</p>

            <div class="coi-progress-steps">
                <div class="coi-step" id="coi-step-sw" data-status="loading">
                    <span class="coi-step-icon" aria-hidden="true">&#x23F3;</span>
                    <span class="coi-step-text">Installing security worker...</span>
                </div>
                <div class="coi-step" id="coi-step-headers" data-status="pending">
                    <span class="coi-step-icon" aria-hidden="true">&#x25CB;</span>
                    <span class="coi-step-text">Activating isolation headers...</span>
                </div>
            </div>
        </div>
    `;
    container.classList.remove('hidden');
}

/**
 * Update a progress step's status
 * @param {string} stepId - Step element ID
 * @param {'pending'|'loading'|'complete'|'error'} status - New status
 */
export function updateProgressStep(stepId, status) {
    const step = document.getElementById(stepId);
    if (!step) return;

    step.dataset.status = status;
    const icon = step.querySelector('.coi-step-icon');
    if (icon) {
        switch (status) {
            case 'loading':
                icon.innerHTML = '&#x23F3;'; // Hourglass
                break;
            case 'complete':
                icon.innerHTML = '&#x2705;'; // Check mark
                break;
            case 'error':
                icon.innerHTML = '&#x274C;'; // X mark
                break;
            default:
                icon.innerHTML = '&#x25CB;'; // Circle
        }
    }
}

/**
 * Show reload required UI with auto-countdown
 * @param {HTMLElement} container - Container to render into
 * @param {Object} [options] - Configuration options
 * @param {Function} [options.onReload] - Optional callback before reload
 * @param {number} [options.countdownSeconds=3] - Countdown duration
 * @param {boolean} [options.autoReload=true] - Whether to auto-reload
 */
export function showReloadRequiredUI(container, options = {}) {
    const { onReload = null, countdownSeconds = 3, autoReload = true } = options;

    if (activeReloadController) {
        activeReloadController.cancel();
        activeReloadController = null;
    }

    container.innerHTML = `
        <div class="coi-status needs-reload">
            <div class="coi-header">
                <span class="coi-logo" aria-hidden="true">&#x1F510;</span>
                <h3>Almost There!</h3>
            </div>

            <div class="coi-progress-steps">
                <div class="coi-step" data-status="complete">
                    <span class="coi-step-icon" aria-hidden="true">&#x2705;</span>
                    <span class="coi-step-text">Security worker installed</span>
                </div>
                <div class="coi-step" data-status="loading">
                    <span class="coi-step-icon" aria-hidden="true">&#x23F3;</span>
                    <span class="coi-step-text">Activating isolation headers...</span>
                </div>
            </div>

            <div class="coi-reload-section">
                <p class="coi-reload-message">One-time page reload required to enable optimal performance.</p>

                <div id="coi-countdown-wrapper" class="coi-countdown-wrapper ${autoReload ? '' : 'hidden'}">
                    <span class="coi-countdown-text">Reloading in </span>
                    <span id="coi-countdown-number" class="coi-countdown-number">${countdownSeconds}</span>
                    <span class="coi-countdown-text">...</span>
                </div>

                <div class="coi-reload-buttons">
                    <button id="coi-reload-btn" class="btn btn-primary coi-reload-btn">
                        Reload Now
                    </button>
                    <button id="coi-cancel-btn" class="btn btn-secondary coi-cancel-btn ${autoReload ? '' : 'hidden'}">
                        Cancel
                    </button>
                </div>
            </div>

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
                <p class="coi-note">This is a one-time setup per browser.</p>
            </details>
        </div>
    `;
    container.classList.remove('hidden');

    const reloadBtn = document.getElementById('coi-reload-btn');
    const cancelBtn = document.getElementById('coi-cancel-btn');
    const countdownWrapper = document.getElementById('coi-countdown-wrapper');
    const countdownNumber = document.getElementById('coi-countdown-number');

    let countdown = countdownSeconds;
    let timerId = null;

    const doReload = () => {
        if (timerId) {
            clearInterval(timerId);
            timerId = null;
        }
        if (activeReloadController === control) {
            activeReloadController = null;
        }
        if (onReload) {
            onReload();
        }
        window.location.reload();
    };

    const cancelCountdown = () => {
        if (timerId) {
            clearInterval(timerId);
            timerId = null;
        }
        if (countdownWrapper) {
            countdownWrapper.classList.add('hidden');
        }
        if (cancelBtn) {
            cancelBtn.classList.add('hidden');
        }
        if (activeReloadController === control) {
            activeReloadController = null;
        }
    };

    // Set up event listeners
    if (reloadBtn) {
        reloadBtn.addEventListener('click', doReload);
    }
    if (cancelBtn) {
        cancelBtn.addEventListener('click', cancelCountdown);
    }

    // Start countdown if auto-reload is enabled
    if (autoReload && countdownNumber) {
        timerId = setInterval(() => {
            countdown--;
            if (countdown <= 0) {
                doReload();
            } else {
                countdownNumber.textContent = countdown.toString();
            }
        }, 1000);
    }

    // Return control object for external management
    const control = {
        cancel: cancelCountdown,
        reload: doReload,
    };
    activeReloadController = control;
    return control;
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
    if (activeReloadController) {
        activeReloadController.cancel();
        activeReloadController = null;
    }
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
 * @param {boolean} [options.autoReload=true] - Whether to auto-reload when needed
 * @param {number} [options.countdownSeconds=3] - Countdown duration before auto-reload
 */
export async function initCOIDetection({
    statusContainer,
    authContainer,
    onReady,
    maxWaitMs = 5000,
    autoReload = true,
    countdownSeconds = 3,
}) {
    let state = await getCOIState();

    console.log('[COI] Initial state:', state);

    // If already set up and ready, skip the setup flow
    if (state === COI_STATE.READY && isSetupComplete()) {
        console.log('[COI] Setup already complete - fast path');
        hideStatusUI(statusContainer);
        if (onReady) onReady();
        return state;
    }

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

                // Update step status
                updateProgressStep('coi-step-sw', 'complete');
                updateProgressStep('coi-step-headers', 'loading');

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
            markSetupComplete();
            hideStatusUI(statusContainer);
            if (onReady) onReady();
            break;

        case COI_STATE.NEEDS_RELOAD:
            console.log('[COI] Needs reload - showing prompt');
            showReloadRequiredUI(statusContainer, {
                autoReload,
                countdownSeconds,
                onReload: () => console.log('[COI] Reloading...'),
            });
            // Hide auth screen while showing reload prompt
            if (authContainer) {
                authContainer.classList.add('hidden');
            }
            break;

        case COI_STATE.DEGRADED:
            console.log('[COI] Degraded mode - showing warning and proceeding');
            markSetupComplete(); // Still mark complete so we don't keep showing setup
            hideStatusUI(statusContainer);
            showDegradedModeWarning();
            if (onReady) onReady();
            break;

        case COI_STATE.SW_INSTALLING:
            // Still installing after timeout - check if we should show reload or proceed
            console.log('[COI] SW still installing - checking fallback');
            if (!await hasServiceWorkerRegistration()) {
                console.warn('[COI] No service worker registration found after waiting - degrading');
                hideStatusUI(statusContainer);
                showDegradedModeWarning();
                if (onReady) onReady();
                return COI_STATE.DEGRADED;
            }
            if (isSharedArrayBufferAvailable()) {
                // Already have SAB somehow (maybe browser feature)
                markSetupComplete();
                hideStatusUI(statusContainer);
                if (onReady) onReady();
            } else {
                // Show reload prompt as SW should be active soon
                showReloadRequiredUI(statusContainer, {
                    autoReload,
                    countdownSeconds,
                    onReload: () => console.log('[COI] Reloading...'),
                });
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
    hasServiceWorkerRegistration,
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
    updateProgressStep,
    isSetupComplete,
    markSetupComplete,
    clearSetupComplete,
};
