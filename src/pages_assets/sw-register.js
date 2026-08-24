/**
 * cass Archive Service Worker Registration
 *
 * Handles service worker registration, update detection, and status monitoring.
 */

// Registration state
let registration = null;
let updateAvailable = false;
const DEFAULT_SW_MESSAGE_TIMEOUT_MS = 3000;
const DEFAULT_SW_ACTIVATION_TIMEOUT_MS = 30_000;
const watchedRegistrations = new WeakSet();
let controllerChangeListenerInstalled = false;
const ARCHIVE_SCOPE_URL = new URL('./', import.meta.url).href;
const SERVICE_WORKER_URL = new URL('./sw.js', import.meta.url).href;

function getCurrentScopeUrl() {
    return ARCHIVE_SCOPE_URL;
}

function hasExactScope(candidate) {
    return candidate?.scope === getCurrentScopeUrl();
}

async function resolveRegistration() {
    if (!('serviceWorker' in navigator)) {
        registration = null;
        return null;
    }

    try {
        const registrations = await navigator.serviceWorker.getRegistrations();
        registration = registrations.find(hasExactScope) || null;
    } catch (error) {
        console.warn('[SW] Failed to resolve registration:', error);
        registration = null;
        throw new Error('Failed to enumerate service worker registrations', { cause: error });
    }

    return registration;
}

async function waitForExactRegistrationActivation(
    candidateRegistration,
    { timeoutMs = DEFAULT_SW_ACTIVATION_TIMEOUT_MS } = {}
) {
    if (!hasExactScope(candidateRegistration)) {
        throw new Error('Service worker registered with an unexpected scope');
    }
    if (candidateRegistration.active?.state === 'activated') {
        return;
    }

    const candidateWorker = candidateRegistration.installing
        || candidateRegistration.waiting
        || candidateRegistration.active;
    if (!candidateWorker) {
        throw new Error('Archive service worker registration has no worker');
    }

    await new Promise((resolve, reject) => {
        let settled = false;
        let timeoutId = null;
        const finish = (error = null) => {
            if (settled) {
                return;
            }
            settled = true;
            candidateWorker.removeEventListener('statechange', handleStateChange);
            if (timeoutId !== null) {
                clearTimeout(timeoutId);
            }
            if (error) {
                reject(error);
            } else {
                resolve();
            }
        };
        const handleStateChange = () => {
            if (candidateWorker.state === 'activated') {
                finish();
            } else if (candidateWorker.state === 'redundant') {
                finish(new Error('Archive service worker installation failed'));
            }
        };

        candidateWorker.addEventListener('statechange', handleStateChange);
        timeoutId = setTimeout(() => {
            finish(new Error('Timed out waiting for archive service worker activation'));
        }, timeoutMs);
        handleStateChange();
    });
}

async function postMessageWithReply(message, { timeoutMs = DEFAULT_SW_MESSAGE_TIMEOUT_MS } = {}) {
    const currentRegistration = await resolveRegistration();
    const activeWorker = currentRegistration?.active?.state === 'activated'
        ? currentRegistration.active
        : null;
    if (!activeWorker) {
        return null;
    }

    return new Promise((resolve) => {
        const channel = new MessageChannel();
        let settled = false;
        const finish = (value) => {
            if (settled) {
                return;
            }
            settled = true;
            clearTimeout(timeoutId);
            channel.port1.onmessage = null;
            try {
                channel.port1.close();
            } catch {
                // The browser may already have detached the reply port.
            }
            try {
                channel.port2.close();
            } catch {
                // Transferring the request port may make this local handle unusable.
            }
            resolve(value);
        };
        const timeoutId = setTimeout(() => {
            console.warn('[SW] Timed out waiting for controller reply:', message.type);
            finish(null);
        }, timeoutMs);

        channel.port1.onmessage = (event) => {
            finish(event.data ?? null);
        };

        try {
            activeWorker.postMessage(message, [channel.port2]);
        } catch (error) {
            console.warn('[SW] Failed to post message to controller:', message.type, error);
            finish(null);
        }
    });
}

function waitForControllerChange({ timeoutMs = DEFAULT_SW_MESSAGE_TIMEOUT_MS } = {}) {
    return new Promise((resolve) => {
        let settled = false;
        const finish = (controllerChanged) => {
            if (settled) {
                return;
            }
            settled = true;
            clearTimeout(timeoutId);
            navigator.serviceWorker.removeEventListener('controllerchange', handleControllerChange);
            resolve(controllerChanged);
        };
        const handleControllerChange = () => finish(true);
        const timeoutId = setTimeout(() => {
            console.warn('[SW] Timed out waiting for controller change');
            finish(false);
        }, timeoutMs);

        navigator.serviceWorker.addEventListener('controllerchange', handleControllerChange);
    });
}

/**
 * Register the service worker
 * @returns {Promise<ServiceWorkerRegistration|null>}
 */
export async function registerServiceWorker() {
    if (!('serviceWorker' in navigator)) {
        console.warn('[SW] Service Workers not supported');
        return null;
    }

    try {
        registration = await navigator.serviceWorker.register(SERVICE_WORKER_URL, {
            scope: ARCHIVE_SCOPE_URL,
        });

        console.log('[SW] Registered, scope:', registration.scope);

        // Set up update listener
        setupUpdateListener(registration);
        noticeWaitingUpdate(registration);

        // Wait for this exact registration, not the global ready promise. The
        // latter may resolve to a broader parent-scope worker.
        await waitForExactRegistrationActivation(registration);
        noticeWaitingUpdate(registration);
        console.log('[SW] Ready');

        // Check if we already have SharedArrayBuffer support
        if (hasSharedArrayBuffer()) {
            console.log('[SW] SharedArrayBuffer available');
        } else {
            console.warn('[SW] SharedArrayBuffer not available - reload may be needed');
        }

        return registration;
    } catch (error) {
        console.error('[SW] Registration failed:', error);
        throw error;
    }
}

/**
 * Check if SharedArrayBuffer is available
 * (indicates COOP/COEP headers are working)
 * @returns {boolean}
 */
export function hasSharedArrayBuffer() {
    try {
        new SharedArrayBuffer(1);
        return true;
    } catch {
        return false;
    }
}

/**
 * Set up listener for service worker updates
 */
function setupUpdateListener(reg) {
    if (watchedRegistrations.has(reg)) {
        return;
    }
    watchedRegistrations.add(reg);

    reg.addEventListener('updatefound', () => {
        const newWorker = reg.installing;

        if (!newWorker) return;

        newWorker.addEventListener('statechange', () => {
            if (newWorker.state === 'installed') {
                if (reg.active && reg.active !== newWorker) {
                    // New version available
                    console.log('[SW] Update available');
                    updateAvailable = true;
                    showUpdateNotification();
                } else {
                    // First install
                    console.log('[SW] First install complete');
                }
            }
        });
    });

    // Listen for controller change (after skipWaiting)
    if (!controllerChangeListenerInstalled) {
        navigator.serviceWorker.addEventListener('controllerchange', () => {
            console.log('[SW] Controller changed');
            // Could auto-reload here, but better to let user decide
        });
        controllerChangeListenerInstalled = true;
    }
}

function noticeWaitingUpdate(reg) {
    if (!reg?.waiting || !reg.active) {
        return;
    }
    console.log('[SW] Waiting update available');
    updateAvailable = true;
    showUpdateNotification();
}

/**
 * Show update notification banner
 */
function showUpdateNotification() {
    // Check if banner already exists
    if (document.querySelector('.sw-update-banner')) return;

    const banner = document.createElement('div');
    banner.className = 'sw-update-banner';
    banner.innerHTML = `
        <span>A new version is available.</span>
        <button class="sw-update-btn">Refresh</button>
        <button class="sw-dismiss-btn" aria-label="Dismiss">✕</button>
    `;

    // Style the banner
    Object.assign(banner.style, {
        position: 'fixed',
        top: '0',
        left: '0',
        right: '0',
        padding: '12px 16px',
        background: 'var(--color-primary, #3b82f6)',
        color: 'white',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        gap: '16px',
        zIndex: '10000',
        fontFamily: 'var(--font-sans, sans-serif)',
        fontSize: '14px',
    });

    const refreshBtn = banner.querySelector('.sw-update-btn');
    Object.assign(refreshBtn.style, {
        padding: '6px 16px',
        background: 'white',
        color: 'var(--color-primary, #3b82f6)',
        border: 'none',
        borderRadius: '4px',
        cursor: 'pointer',
        fontWeight: '500',
    });

    const dismissBtn = banner.querySelector('.sw-dismiss-btn');
    Object.assign(dismissBtn.style, {
        background: 'transparent',
        border: 'none',
        color: 'white',
        cursor: 'pointer',
        fontSize: '18px',
        padding: '4px',
    });

    // Event handlers
    refreshBtn.addEventListener('click', () => {
        void applyUpdate().catch((error) => {
            console.error('[SW] Failed to apply update:', error);
        });
    });

    dismissBtn.addEventListener('click', () => {
        banner.remove();
    });

    document.body.prepend(banner);
}

/**
 * Apply pending update
 */
export async function applyUpdate() {
    const currentRegistration = await resolveRegistration();
    if (!currentRegistration?.waiting) {
        throw new Error('No waiting archive update is available');
    }

    const waitingWorker = currentRegistration.waiting;
    const waitForActivation = waitForControllerChange();
    // Tell waiting service worker to skip waiting
    waitingWorker.postMessage({ type: 'SKIP_WAITING' });
    const controllerChanged = await waitForActivation;
    if (
        !controllerChanged
        || waitingWorker.state !== 'activated'
        || currentRegistration.active !== waitingWorker
    ) {
        throw new Error('Archive update did not become the active controller; the page was not reloaded');
    }

    // Reload the page
    window.location.reload();
}

/**
 * Check if an update is available
 * @returns {boolean}
 */
export function isUpdateAvailable() {
    return updateAvailable;
}

/**
 * Get the current service worker registration
 * @returns {Promise<ServiceWorkerRegistration|null>}
 */
export async function getRegistration() {
    return resolveRegistration();
}

/**
 * Unregister the service worker
 */
export async function unregisterServiceWorker() {
    if (!('serviceWorker' in navigator)) {
        registration = null;
        return true;
    }

    const currentRegistration = await resolveRegistration();
    if (!currentRegistration) {
        registration = null;
        return true;
    }

    const unregistered = await currentRegistration.unregister();
    if (unregistered) {
        registration = null;
        console.log('[SW] Unregistered');
        return true;
    }
    console.warn('[SW] Service Worker refused unregister request');
    return false;
}

/**
 * Clear the service worker cache
 */
export async function clearCache(options = {}) {
    const reply = await postMessageWithReply({ type: 'CLEAR_CACHE' }, options);
    if (reply?.type === 'CACHE_CLEARED') {
        console.log('[SW] Cache cleared');
        return true;
    }
    if (reply?.type === 'CACHE_CLEAR_FAILED') {
        console.warn('[SW] Cache clear failed:', reply.error);
    }
    return false;
}

/**
 * Get service worker version
 */
export async function getVersion(options = {}) {
    const reply = await postMessageWithReply({ type: 'GET_VERSION' }, options);
    return reply?.version ?? null;
}

// Export status checker
export const swStatus = {
    get isSupported() {
        return 'serviceWorker' in navigator;
    },
    get isRegistered() {
        return 'serviceWorker' in navigator && hasExactScope(registration);
    },
    get isActive() {
        return 'serviceWorker' in navigator
            && hasExactScope(registration)
            && registration.active?.state === 'activated';
    },
    get hasSharedArrayBuffer() {
        return hasSharedArrayBuffer();
    },
    get updateAvailable() {
        return updateAvailable;
    },
};

export default {
    registerServiceWorker,
    hasSharedArrayBuffer,
    applyUpdate,
    isUpdateAvailable,
    getRegistration,
    unregisterServiceWorker,
    clearCache,
    getVersion,
    swStatus,
};
