/**
 * cass Archive Authentication Module
 *
 * Handles password and QR code authentication for encrypted archives.
 * CSP-safe: No inline event handlers, no eval.
 */

import { createStrengthMeter } from './password-strength.js';

// State
let config = null;
let worker = null;
let qrScanner = null;
let strengthMeter = null;

// DOM Elements
const elements = {
    authScreen: null,
    appScreen: null,
    passwordInput: null,
    unlockBtn: null,
    togglePassword: null,
    qrBtn: null,
    qrScanner: null,
    qrReader: null,
    qrCancelBtn: null,
    fingerprintValue: null,
    fingerprintHelp: null,
    fingerprintTooltip: null,
    authError: null,
    authProgress: null,
    progressFill: null,
    progressText: null,
    lockBtn: null,
};

/**
 * Initialize the authentication module
 */
async function init() {
    // Cache DOM elements
    cacheElements();

    // Set up event listeners
    setupEventListeners();

    // Load configuration
    try {
        config = await loadConfig();
        await displayFingerprint();
    } catch (error) {
        showError('Failed to load archive configuration. The archive may be corrupted.');
        console.error('Config load error:', error);
        return;
    }

    // Initialize crypto worker
    // Note: Using classic worker (not module) because crypto_worker.js uses importScripts()
    try {
        worker = new Worker('./crypto_worker.js');
        worker.onmessage = handleWorkerMessage;
        worker.onerror = handleWorkerError;
    } catch (error) {
        showError('Failed to initialize decryption worker. Your browser may not support Web Workers.');
        console.error('Worker init error:', error);
    }

    // Check for existing session
    checkExistingSession();

    // Initialize password strength meter
    if (elements.passwordInput && elements.strengthMeter) {
        strengthMeter = createStrengthMeter(elements.passwordInput, {
            meterContainer: elements.strengthMeter,
            labelElement: elements.strengthLabel,
            suggestionsList: elements.strengthSuggestions,
        });
    }

    // Enable form
    elements.unlockBtn.disabled = false;
    elements.passwordInput.disabled = false;
}

/**
 * Cache DOM element references
 */
function cacheElements() {
    elements.authScreen = document.getElementById('auth-screen');
    elements.appScreen = document.getElementById('app-screen');
    elements.passwordInput = document.getElementById('password');
    elements.unlockBtn = document.getElementById('unlock-btn');
    elements.togglePassword = document.getElementById('toggle-password');
    elements.qrBtn = document.getElementById('qr-btn');
    elements.qrScanner = document.getElementById('qr-scanner');
    elements.qrReader = document.getElementById('qr-reader');
    elements.qrCancelBtn = document.getElementById('qr-cancel-btn');
    elements.fingerprintValue = document.getElementById('fingerprint-value');
    elements.fingerprintHelp = document.getElementById('fingerprint-help');
    elements.fingerprintTooltip = document.getElementById('fingerprint-tooltip');
    elements.authError = document.getElementById('auth-error');
    elements.authProgress = document.getElementById('auth-progress');
    elements.progressFill = elements.authProgress?.querySelector('.progress-fill');
    elements.progressText = elements.authProgress?.querySelector('.progress-text');
    elements.lockBtn = document.getElementById('lock-btn');
    elements.strengthMeter = document.getElementById('strength-meter');
    elements.strengthLabel = document.getElementById('strength-label');
    elements.strengthSuggestions = document.getElementById('strength-suggestions');
}

/**
 * Set up event listeners (CSP-safe, no inline handlers)
 */
function setupEventListeners() {
    // Password unlock
    elements.unlockBtn?.addEventListener('click', handleUnlockClick);

    // Enter key in password field
    elements.passwordInput?.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') {
            handleUnlockClick();
        }
    });

    // Toggle password visibility
    elements.togglePassword?.addEventListener('click', togglePasswordVisibility);

    // QR scanner
    elements.qrBtn?.addEventListener('click', openQrScanner);
    elements.qrCancelBtn?.addEventListener('click', closeQrScanner);

    // Fingerprint help tooltip
    elements.fingerprintHelp?.addEventListener('click', toggleFingerprintTooltip);

    // Lock button (re-lock archive)
    elements.lockBtn?.addEventListener('click', lockArchive);

    // Escape key to close QR scanner
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape' && !elements.qrScanner?.classList.contains('hidden')) {
            closeQrScanner();
        }
    });
}

/**
 * Load config.json from the archive
 */
async function loadConfig() {
    const response = await fetch('./config.json');
    if (!response.ok) {
        throw new Error(`Failed to load config: ${response.status}`);
    }
    return response.json();
}

/**
 * Display integrity fingerprint with TOFU verification
 */
async function displayFingerprint() {
    const tofuKey = `cass_fingerprint_${config?.export_id || 'default'}`;

    try {
        // Try to load integrity.json if it exists
        const response = await fetch('./integrity.json');
        if (response.ok) {
            const integrity = await response.json();
            const fingerprint = await computeFingerprint(JSON.stringify(integrity));
            elements.fingerprintValue.textContent = fingerprint;

            // TOFU verification
            const result = await verifyTofu(fingerprint, tofuKey);
            displayTofuStatus(result);
        } else {
            // Fall back to config fingerprint
            const fingerprint = await computeFingerprint(JSON.stringify(config));
            elements.fingerprintValue.textContent = fingerprint;

            const result = await verifyTofu(fingerprint, tofuKey);
            displayTofuStatus(result);
        }
    } catch (error) {
        // Use export_id as fallback fingerprint
        if (config?.export_id) {
            const bytes = base64ToBytes(config.export_id);
            const fingerprint = formatFingerprint(bytes.slice(0, 8));
            elements.fingerprintValue.textContent = fingerprint;
        } else {
            elements.fingerprintValue.textContent = 'unavailable';
        }
    }
}

/**
 * Verify fingerprint using TOFU (Trust On First Use)
 * Returns: { valid: true, isFirstVisit: boolean } or { valid: false, reason: string, previousFingerprint: string }
 */
async function verifyTofu(currentFingerprint, storageKey) {
    try {
        const storedFingerprint = localStorage.getItem(storageKey);

        if (!storedFingerprint) {
            // First visit - store fingerprint
            localStorage.setItem(storageKey, currentFingerprint);
            return { valid: true, isFirstVisit: true };
        }

        if (storedFingerprint === currentFingerprint) {
            // Fingerprint matches - all good
            return { valid: true, isFirstVisit: false };
        }

        // Fingerprint changed - TOFU violation!
        return {
            valid: false,
            reason: 'TOFU_VIOLATION',
            previousFingerprint: storedFingerprint,
            currentFingerprint: currentFingerprint
        };
    } catch (e) {
        // LocalStorage may be disabled
        console.warn('TOFU check unavailable:', e);
        return { valid: true, isFirstVisit: true };
    }
}

/**
 * Display TOFU verification status
 */
function displayTofuStatus(result) {
    const helpElement = elements.fingerprintHelp;
    if (!helpElement) return;

    if (!result.valid && result.reason === 'TOFU_VIOLATION') {
        // Show warning for fingerprint change
        helpElement.classList.add('tofu-warning');
        helpElement.textContent = '⚠️';
        helpElement.title = 'SECURITY WARNING: Archive fingerprint has changed since your last visit!\n' +
            `Previous: ${result.previousFingerprint}\n` +
            `Current: ${result.currentFingerprint}\n\n` +
            'If you did not expect this change, DO NOT enter your password.';

        // Also show a visible warning
        showTofuWarning(result);
    } else if (result.isFirstVisit) {
        helpElement.title = 'First visit - fingerprint stored for future verification';
    } else {
        helpElement.classList.add('tofu-verified');
        helpElement.title = 'Fingerprint verified - matches previous visit';
    }
}

/**
 * Show TOFU violation warning banner
 */
function showTofuWarning(result) {
    // Create warning element if it doesn't exist
    let warning = document.getElementById('tofu-warning');
    if (!warning) {
        warning = document.createElement('div');
        warning.id = 'tofu-warning';
        warning.className = 'tofu-warning-banner';

        // Build DOM structure (without fingerprints to avoid XSS)
        warning.innerHTML = `
            <div class="tofu-warning-content">
                <strong>⚠️ Security Warning</strong>
                <p>The archive fingerprint has changed since your last visit.</p>
                <p class="tofu-fingerprints">
                    <span>Previous: <code id="tofu-prev-fp"></code></span>
                    <span>Current: <code id="tofu-curr-fp"></code></span>
                </p>
                <p>If you did not expect this change, <strong>DO NOT enter your password</strong>.</p>
                <div class="tofu-actions">
                    <button type="button" id="tofu-accept-btn" class="tofu-accept">I trust this change</button>
                    <button type="button" id="tofu-dismiss-btn" class="tofu-dismiss">Dismiss warning</button>
                </div>
            </div>
        `;

        // Set fingerprints safely using textContent (defense-in-depth)
        warning.querySelector('#tofu-prev-fp').textContent = result.previousFingerprint;
        warning.querySelector('#tofu-curr-fp').textContent = result.currentFingerprint;

        // Insert before auth form
        const authForm = document.querySelector('.auth-form');
        if (authForm) {
            authForm.parentNode.insertBefore(warning, authForm);
        } else {
            elements.authScreen?.appendChild(warning);
        }

        // Add event listeners
        document.getElementById('tofu-accept-btn')?.addEventListener('click', () => {
            acceptNewFingerprint(result.currentFingerprint);
            warning.remove();
        });

        document.getElementById('tofu-dismiss-btn')?.addEventListener('click', () => {
            warning.remove();
        });
    }
}

/**
 * Accept new fingerprint (user acknowledges the change)
 */
function acceptNewFingerprint(newFingerprint) {
    const tofuKey = `cass_fingerprint_${config?.export_id || 'default'}`;
    try {
        localStorage.setItem(tofuKey, newFingerprint);

        // Update UI
        const helpElement = elements.fingerprintHelp;
        if (helpElement) {
            helpElement.classList.remove('tofu-warning');
            helpElement.classList.add('tofu-verified');
            helpElement.title = 'Fingerprint updated - new fingerprint stored';
        }
    } catch (e) {
        console.warn('Failed to store new fingerprint:', e);
    }
}

/**
 * Compute SHA-256 fingerprint of data
 */
async function computeFingerprint(data) {
    const encoder = new TextEncoder();
    const dataBytes = encoder.encode(data);
    const hashBuffer = await crypto.subtle.digest('SHA-256', dataBytes);
    const hashArray = new Uint8Array(hashBuffer);
    return formatFingerprint(hashArray.slice(0, 8));
}

/**
 * Format bytes as colon-separated hex fingerprint
 */
function formatFingerprint(bytes) {
    return Array.from(bytes)
        .map(b => b.toString(16).padStart(2, '0'))
        .join(':');
}

/**
 * Handle unlock button click
 */
async function handleUnlockClick() {
    const password = elements.passwordInput.value.trim();

    if (!password) {
        showError('Please enter a password');
        elements.passwordInput.focus();
        return;
    }

    if (!worker) {
        showError('Decryption worker not initialized');
        return;
    }

    hideError();
    showProgress('Deriving key...');
    disableForm();

    // Send unlock request to worker
    worker.postMessage({
        type: 'UNLOCK_PASSWORD',
        password: password,
        config: config,
    });
}

/**
 * Toggle password visibility
 */
function togglePasswordVisibility() {
    const input = elements.passwordInput;
    const icon = elements.togglePassword.querySelector('.eye-icon');

    if (input.type === 'password') {
        input.type = 'text';
        icon.textContent = '🙈';
    } else {
        input.type = 'password';
        icon.textContent = '👁';
    }
}

/**
 * Toggle fingerprint tooltip
 */
function toggleFingerprintTooltip() {
    elements.fingerprintTooltip?.classList.toggle('hidden');
}

/**
 * Open QR code scanner
 */
async function openQrScanner() {
    elements.qrScanner.classList.remove('hidden');

    // Dynamically load QR scanner library if not loaded
    if (!window.Html5Qrcode) {
        try {
            // Try to load from vendor folder
            const script = document.createElement('script');
            script.src = './vendor/html5-qrcode.min.js';
            await new Promise((resolve, reject) => {
                script.onload = resolve;
                script.onerror = reject;
                document.head.appendChild(script);
            });
        } catch (error) {
            showError('Failed to load QR scanner library');
            closeQrScanner();
            return;
        }
    }

    try {
        qrScanner = new window.Html5Qrcode('qr-reader');
        await qrScanner.start(
            { facingMode: 'environment' },
            { fps: 10, qrbox: { width: 250, height: 250 } },
            handleQrSuccess,
            handleQrError
        );
    } catch (error) {
        console.error('QR scanner error:', error);
        if (error.name === 'NotAllowedError') {
            showError('Camera permission denied. Please allow camera access to scan QR codes.');
        } else {
            showError('Failed to start camera. Please enter password manually.');
        }
        closeQrScanner();
    }
}

/**
 * Close QR code scanner
 */
async function closeQrScanner() {
    if (qrScanner) {
        try {
            await qrScanner.stop();
        } catch (e) {
            // Ignore stop errors
        }
        qrScanner = null;
    }
    elements.qrScanner.classList.add('hidden');
}

/**
 * Handle successful QR code scan
 */
function handleQrSuccess(decodedText) {
    closeQrScanner();

    hideError();
    showProgress('Deriving key from QR...');
    disableForm();

    // Try to parse as JSON recovery data, or use raw text as recovery secret
    let recoverySecret;
    try {
        const data = JSON.parse(decodedText);
        recoverySecret = data.recovery_secret || data.secret || decodedText;
    } catch {
        recoverySecret = decodedText;
    }

    // Send unlock request to worker
    worker.postMessage({
        type: 'UNLOCK_RECOVERY',
        recoverySecret: recoverySecret,
        config: config,
    });
}

/**
 * Handle QR code scan error (called continuously during scanning)
 */
function handleQrError(error) {
    // Ignore "QR code not found" errors during scanning
    if (!error?.includes?.('QR code parse')) {
        console.debug('QR scan:', error);
    }
}

/**
 * Handle messages from crypto worker
 */
function handleWorkerMessage(event) {
    const { type, ...data } = event.data;

    switch (type) {
        case 'UNLOCK_SUCCESS':
            handleUnlockSuccess(data);
            break;

        case 'UNLOCK_FAILED':
            handleUnlockFailed(data);
            break;

        case 'PROGRESS':
            updateProgress(data.phase, data.percent);
            break;

        case 'DECRYPT_SUCCESS':
            handleDecryptSuccess(data);
            break;

        case 'DECRYPT_FAILED':
            handleDecryptFailed(data);
            break;

        case 'DB_READY':
            handleDatabaseReady(data);
            break;

        default:
            console.warn('Unknown worker message type:', type);
    }
}

/**
 * Handle worker errors
 */
function handleWorkerError(error) {
    console.error('Worker error:', error);
    hideProgress();
    enableForm();
    showError('An error occurred during decryption. Please try again.');
}

/**
 * Handle successful unlock
 */
function handleUnlockSuccess(data) {
    hideProgress();

    // Store session key in memory
    window.cassSession = {
        dek: data.dek,
        config: config,
    };

    // Optionally store in sessionStorage for page refresh persistence
    // (Less secure, but better UX)
    try {
        sessionStorage.setItem('cass_unlocked', 'true');
    } catch (e) {
        // SessionStorage may be disabled
    }

    // Transition to app
    transitionToApp();
}

/**
 * Handle failed unlock
 */
function handleUnlockFailed(data) {
    hideProgress();
    enableForm();

    const message = data.error || 'Incorrect password or invalid recovery code';
    showError(message);

    // Clear password field
    elements.passwordInput.value = '';
    elements.passwordInput.focus();
}

/**
 * Handle successful decryption
 */
function handleDecryptSuccess(data) {
    updateProgress('Database decrypted', 100);
    // Database will be loaded next
}

/**
 * Handle failed decryption
 */
function handleDecryptFailed(data) {
    hideProgress();
    showError(`Decryption failed: ${data.error}`);
}

/**
 * Handle database ready
 */
function handleDatabaseReady(data) {
    hideProgress();
    // The viewer.js module will handle database queries
    window.dispatchEvent(new CustomEvent('cass:db-ready', { detail: data }));
}

/**
 * Transition from auth screen to app screen
 */
function transitionToApp() {
    elements.authScreen.classList.add('hidden');
    elements.appScreen.classList.remove('hidden');

    // Start decryption and database loading
    worker.postMessage({
        type: 'DECRYPT_DATABASE',
        dek: window.cassSession.dek,
        config: config,
    });

    // Load viewer module
    loadViewerModule();
}

/**
 * Lock the archive (return to auth screen)
 */
function lockArchive() {
    // Clear session
    window.cassSession = null;
    try {
        sessionStorage.removeItem('cass_unlocked');
    } catch (e) {
        // Ignore
    }

    // Tell worker to clear keys
    worker?.postMessage({ type: 'CLEAR_KEYS' });

    // Return to auth screen
    elements.appScreen.classList.add('hidden');
    elements.authScreen.classList.remove('hidden');

    // Reset form
    elements.passwordInput.value = '';
    enableForm();
    hideError();
    hideProgress();
}

/**
 * Check for existing session on page load
 */
function checkExistingSession() {
    try {
        const unlocked = sessionStorage.getItem('cass_unlocked');
        if (unlocked === 'true' && window.cassSession?.dek) {
            transitionToApp();
        }
    } catch (e) {
        // SessionStorage may be disabled
    }
}

/**
 * Dynamically load the viewer module
 */
async function loadViewerModule() {
    try {
        const module = await import('./viewer.js');
        module.init?.();
    } catch (error) {
        console.error('Failed to load viewer module:', error);
        // Viewer may not exist yet - that's OK for now
    }
}

/**
 * Show error message
 */
function showError(message) {
    const errorMsg = elements.authError.querySelector('.error-message');
    if (errorMsg) {
        errorMsg.textContent = message;
    }
    elements.authError.classList.remove('hidden');
}

/**
 * Hide error message
 */
function hideError() {
    elements.authError.classList.add('hidden');
}

/**
 * Show progress indicator
 */
function showProgress(text) {
    elements.progressText.textContent = text;
    elements.progressFill.style.width = '0%';
    elements.authProgress.classList.remove('hidden');
}

/**
 * Update progress indicator
 */
function updateProgress(phase, percent) {
    elements.progressText.textContent = phase;
    elements.progressFill.style.width = `${percent}%`;
}

/**
 * Hide progress indicator
 */
function hideProgress() {
    elements.authProgress.classList.add('hidden');
}

/**
 * Disable form inputs during processing
 */
function disableForm() {
    elements.passwordInput.disabled = true;
    elements.unlockBtn.disabled = true;
    elements.qrBtn.disabled = true;
}

/**
 * Enable form inputs
 */
function enableForm() {
    elements.passwordInput.disabled = false;
    elements.unlockBtn.disabled = false;
    elements.qrBtn.disabled = false;
}

/**
 * Decode base64 to Uint8Array
 */
function base64ToBytes(base64) {
    const binary = atob(base64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
}

// Initialize when DOM is ready
if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
} else {
    init();
}
