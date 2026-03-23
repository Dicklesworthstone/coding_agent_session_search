/**
 * cass Archive Authentication Module
 *
 * Handles password and QR code authentication for encrypted archives.
 * CSP-safe: No inline event handlers, no eval.
 */

import { createStrengthMeter } from './password-strength.js';
import { StorageMode, getArchiveScopeId, getStoredMode, isOpfsEnabled } from './storage.js';
import { SESSION_CONFIG } from './session.js';

// State
let config = null;
let worker = null;
let qrScanner = null;
let strengthMeter = null;
let isUnencryptedArchive = false;
let tofuStatus = { valid: true, isFirstVisit: true };
let unlockInFlight = false;
let decryptInFlight = false;
let activeUnlockRequestId = null;
let activeDecryptRequestId = null;
let nextWorkerRequestId = 1;
let activeAppInitToken = 0;
const LEGACY_SESSION_KEYS = {
    DEK: 'cass_session_dek',
    EXPIRY: 'cass_session_expiry',
    UNLOCKED: 'cass_unlocked',
};

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
        tofuStatus = await displayFingerprint();
    } catch (error) {
        showError('Failed to load archive configuration. The archive may be corrupted.');
        console.error('Config load error:', error);
        return;
    }

    if (config?.encrypted === false) {
        clearStoredSession();
        window.cassSession = null;
        setupUnencryptedMode();
        enableForm();
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
        disableForm();
        return;
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
    // Password unlock: use form submit as the single entry point.
    // Separate click/keypress handlers can fire duplicate unlock requests.
    document.getElementById('auth-form')?.addEventListener('submit', handleUnlockClick);

    // Toggle password visibility
    elements.togglePassword?.addEventListener('click', togglePasswordVisibility);

    // QR scanner
    elements.qrBtn?.addEventListener('click', openQrScanner);
    elements.qrCancelBtn?.addEventListener('click', closeQrScanner);

    // Fingerprint help tooltip
    elements.fingerprintHelp?.addEventListener('click', toggleFingerprintTooltip);

    // Lock button (re-lock archive)
    elements.lockBtn?.addEventListener('click', handleLockButtonClick);
    window.addEventListener('cass:lock', handleExternalLockEvent);
    window.addEventListener('cass:session-mode-change', (event) => {
        const mode = event?.detail?.mode;
        if (mode === StorageMode.MEMORY) {
            clearStoredSession();
            return;
        }

        if (window.cassSession?.dek) {
            persistSession(window.cassSession.dek);
        }
    });

    // Escape key to close QR scanner
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape' && !elements.qrScanner?.classList.contains('hidden')) {
            closeQrScanner();
        }
    });
}

function allocateWorkerRequestId() {
    const requestId = nextWorkerRequestId;
    nextWorkerRequestId += 1;
    return requestId;
}

function beginAppInitAttempt() {
    activeAppInitToken += 1;
    return activeAppInitToken;
}

function isCurrentAppInitToken(token) {
    return token === activeAppInitToken;
}

function invalidateAppInitAttempt() {
    activeAppInitToken += 1;
}

function isCurrentWorkerMessage(type, requestId) {
    if (requestId == null) {
        return true;
    }

    switch (type) {
        case 'UNLOCK_SUCCESS':
        case 'UNLOCK_FAILED':
            return requestId === activeUnlockRequestId;
        case 'DECRYPT_SUCCESS':
        case 'DECRYPT_FAILED':
        case 'DB_READY':
            return requestId === activeDecryptRequestId;
        case 'PROGRESS':
            return requestId === activeUnlockRequestId || requestId === activeDecryptRequestId;
        default:
            return true;
    }
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

function getSessionKeys() {
    const scopeId = getArchiveScopeId();
    return {
        DEK: `cass_session_dek_${scopeId}`,
        EXPIRY: `cass_session_expiry_${scopeId}`,
        UNLOCKED: `cass_unlocked_${scopeId}`,
    };
}

function getTofuKey() {
    // Scope TOFU to the archive location, not the archive's self-declared export_id.
    // Otherwise a full archive swap at the same URL looks like a first visit.
    return `cass_fingerprint_v2_${getArchiveScopeId()}`;
}

/**
 * Display integrity fingerprint with TOFU verification
 */
async function displayFingerprint() {
    try {
        // Try to load integrity.json if it exists
        const response = await fetch('./integrity.json');
        if (response.ok) {
            const integrity = await response.json();
            const fingerprint = await computeFingerprint(JSON.stringify(integrity));
            elements.fingerprintValue.textContent = fingerprint;

            // TOFU verification
            const result = await verifyTofu(fingerprint, getTofuKey());
            displayTofuStatus(result);
            return result;
        } else {
            // Fall back to config fingerprint
            const fingerprint = await computeFingerprint(JSON.stringify(config));
            elements.fingerprintValue.textContent = fingerprint;

            const result = await verifyTofu(fingerprint, getTofuKey());
            displayTofuStatus(result);
            return result;
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

        return { valid: true, isFirstVisit: true };
    }
}

function setupUnencryptedMode() {
    isUnencryptedArchive = true;

    const subtitle = document.querySelector('.auth-header .subtitle');
    if (subtitle) {
        subtitle.textContent = 'This archive is NOT encrypted. Anyone with access can read it.';
    }

    if (elements.passwordInput) {
        elements.passwordInput.required = false;
    }

    const passwordGroup = elements.passwordInput?.closest('.form-group');
    passwordGroup?.classList.add('hidden');

    const divider = document.querySelector('.auth-form .divider');
    divider?.classList.add('hidden');

    elements.qrBtn?.classList.add('hidden');
    elements.togglePassword?.classList.add('hidden');

    if (elements.unlockBtn) {
        const label = elements.unlockBtn.querySelector('.btn-text');
        if (label) {
            label.textContent = 'Open Archive';
        }
    }

    const warning = document.createElement('div');
    warning.className = 'tofu-warning-banner';

    const warningContent = document.createElement('div');
    warningContent.className = 'tofu-warning-content';

    const warningTitle = document.createElement('strong');
    warningTitle.textContent = 'Unencrypted archive';
    warningContent.appendChild(warningTitle);

    const warningBody = document.createElement('p');
    warningBody.textContent =
        'This export was generated WITHOUT encryption. Treat it as public data.';
    warningContent.appendChild(warningBody);

    warning.appendChild(warningContent);

    const authForm = document.querySelector('.auth-form');
    if (authForm) {
        authForm.parentNode.insertBefore(warning, authForm);
    } else {
        elements.authScreen?.appendChild(warning);
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
    const tofuKey = getTofuKey();
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
async function handleUnlockClick(event) {
    if (event) {
        event.preventDefault();
    }

    if (unlockInFlight || decryptInFlight) {
        return;
    }

    if (isUnencryptedArchive) {
        await transitionToAppUnencrypted();
        return;
    }

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
    unlockInFlight = true;
    activeUnlockRequestId = allocateWorkerRequestId();

    // Send unlock request to worker
    worker.postMessage({
        type: 'UNLOCK_PASSWORD',
        password: password,
        config: config,
        requestId: activeUnlockRequestId,
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
    if (unlockInFlight || decryptInFlight) {
        return;
    }

    closeQrScanner();

    hideError();
    showProgress('Deriving key from QR...');
    disableForm();
    unlockInFlight = true;
    activeUnlockRequestId = allocateWorkerRequestId();

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
        requestId: activeUnlockRequestId,
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

    if (!isCurrentWorkerMessage(type, data.requestId)) {
        console.debug('Ignoring stale worker message:', type, data.requestId);
        return;
    }

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
    const hadActiveSession =
        decryptInFlight
        || unlockInFlight
        || !!window.cassSession?.dek;
    invalidateAppInitAttempt();
    unlockInFlight = false;
    decryptInFlight = false;
    activeUnlockRequestId = null;
    activeDecryptRequestId = null;
    clearStoredSession();
    window.cassSession = null;
    hideProgress();
    enableForm();
    if (hadActiveSession) {
        elements.appScreen.classList.add('hidden');
        elements.authScreen.classList.remove('hidden');
        elements.passwordInput.value = '';
    }
    showError('An error occurred during decryption. Please try again.');
}

/**
 * Handle successful unlock
 */
function handleUnlockSuccess(data) {
    unlockInFlight = false;
    activeUnlockRequestId = null;
    hideProgress();

    // Store session key in memory
    window.cassSession = {
        dek: data.dek,
        config: config,
    };

    // Persist session based on selected storage mode
    persistSession(data.dek);

    // Transition to app
    transitionToApp();
}

/**
 * Handle failed unlock
 */
function handleUnlockFailed(data) {
    unlockInFlight = false;
    activeUnlockRequestId = null;
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
async function handleDecryptSuccess(data) {
    updateProgress('Database decrypted', 100);

    if (!data?.dbBytes) {
        invalidateAppInitAttempt();
        decryptInFlight = false;
        activeDecryptRequestId = null;
        hideProgress();
        showError('Decryption did not return a database payload');
        enableForm();
        elements.appScreen.classList.add('hidden');
        elements.authScreen.classList.remove('hidden');
        clearStoredSession();
        window.cassSession = null;
        return;
    }

    try {
        const dbModule = await import('./database.js');
        let dbBytes;
        if (data.dbBytes instanceof ArrayBuffer) {
            dbBytes = new Uint8Array(data.dbBytes);
        } else if (ArrayBuffer.isView(data.dbBytes)) {
            dbBytes = new Uint8Array(
                data.dbBytes.buffer,
                data.dbBytes.byteOffset,
                data.dbBytes.byteLength
            );
        } else {
            throw new Error('Invalid database payload');
        }
        await dbModule.initDatabase(dbBytes);
        const stats = dbModule.getStatistics();
        window.dispatchEvent(new CustomEvent('cass:db-ready', {
            detail: {
                conversationCount: stats.conversations || 0,
                messageCount: stats.messages || 0,
            },
        }));
        decryptInFlight = false;
        activeDecryptRequestId = null;
    } catch (error) {
        console.error('Failed to initialize database:', error);
        invalidateAppInitAttempt();
        decryptInFlight = false;
        activeDecryptRequestId = null;
        hideProgress();
        showError('Failed to initialize database');
        enableForm();
        elements.appScreen.classList.add('hidden');
        elements.authScreen.classList.remove('hidden');
        clearStoredSession();
        window.cassSession = null;
    }
}

/**
 * Handle failed decryption
 */
function handleDecryptFailed(data) {
    invalidateAppInitAttempt();
    decryptInFlight = false;
    activeDecryptRequestId = null;
    hideProgress();
    showError(`Decryption failed: ${data.error}`);
    enableForm();
    elements.appScreen.classList.add('hidden');
    elements.authScreen.classList.remove('hidden');
    clearStoredSession();
    window.cassSession = null;
    elements.passwordInput.value = '';
}

/**
 * Handle database ready
 */
function handleDatabaseReady(data) {
    decryptInFlight = false;
    activeDecryptRequestId = null;
    hideProgress();
    // The viewer.js module will handle database queries
    window.dispatchEvent(new CustomEvent('cass:db-ready', { detail: data }));
}

async function recoverFromAppInitFailure(message, error, initToken = activeAppInitToken) {
    if (!isCurrentAppInitToken(initToken)) {
        return;
    }
    invalidateAppInitAttempt();
    console.error(message, error);
    unlockInFlight = false;
    decryptInFlight = false;
    activeUnlockRequestId = null;
    activeDecryptRequestId = null;
    clearStoredSession();
    window.cassSession = null;
    await closeLiveDatabase();
    hideProgress();
    enableForm();
    elements.appScreen.classList.add('hidden');
    elements.authScreen.classList.remove('hidden');
    if (elements.passwordInput) {
        elements.passwordInput.value = '';
    }
    showError(message);
}

/**
 * Transition from auth screen to app screen
 */
function transitionToApp() {
    if (decryptInFlight) {
        return;
    }

    const appInitToken = beginAppInitAttempt();
    decryptInFlight = true;
    activeDecryptRequestId = allocateWorkerRequestId();
    elements.authScreen.classList.add('hidden');
    elements.appScreen.classList.remove('hidden');

    // Start decryption and database loading
    try {
        worker.postMessage({
            type: 'DECRYPT_DATABASE',
            dek: window.cassSession.dek,
            config: config,
            opfsEnabled: isOpfsEnabled(),
            requestId: activeDecryptRequestId,
        });
    } catch (error) {
        void recoverFromAppInitFailure('Failed to start archive decryption', error, appInitToken);
        return;
    }

    // Load viewer module
    void loadViewerModule(appInitToken).catch((error) => {
        void recoverFromAppInitFailure('Failed to load archive viewer', error, appInitToken);
    });
}

async function transitionToAppUnencrypted() {
    if (decryptInFlight) {
        return;
    }

    const appInitToken = beginAppInitAttempt();
    decryptInFlight = true;
    hideError();
    disableForm();

    elements.authScreen.classList.add('hidden');
    elements.appScreen.classList.remove('hidden');

    try {
        await loadViewerModule(appInitToken);
    } catch (error) {
        await recoverFromAppInitFailure('Failed to load archive viewer', error, appInitToken);
        return;
    }

    try {
        const didLoad = await loadUnencryptedDatabase(appInitToken);
        if (!didLoad || !isCurrentAppInitToken(appInitToken)) {
            return;
        }
        decryptInFlight = false;
    } catch (error) {
        await recoverFromAppInitFailure('Failed to load unencrypted database', error, appInitToken);
    }
}

async function loadUnencryptedDatabase(initToken = activeAppInitToken) {
    const payloadPath = getUnencryptedPayloadPath();
    const response = await fetch(payloadPath);
    if (!response.ok) {
        throw new Error(`Failed to load database: ${response.status}`);
    }

    const dbBytes = new Uint8Array(await response.arrayBuffer());
    if (!isCurrentAppInitToken(initToken)) {
        return false;
    }
    const dbModule = await import('./database.js');
    await dbModule.initDatabase(dbBytes);
    if (!isCurrentAppInitToken(initToken)) {
        await closeLiveDatabase();
        return false;
    }

    const stats = dbModule.getStatistics();
    window.dispatchEvent(new CustomEvent('cass:db-ready', {
        detail: {
            conversationCount: stats.conversations || 0,
            messageCount: stats.messages || 0,
        },
    }));
    return true;
}

function getUnencryptedPayloadPath() {
    const rawPath = config?.payload?.path;
    if (typeof rawPath === 'string' && rawPath.trim().length > 0) {
        return rawPath.startsWith('./') ? rawPath : `./${rawPath}`;
    }
    return './payload/data.db';
}

/**
 * Handle lock button click from the app header.
 */
function handleLockButtonClick(event) {
    if (event) {
        event.preventDefault();
    }
    void lockArchive({ broadcast: true, action: 'lock' });
}

/**
 * Handle lock requests emitted by other modules.
 */
function handleExternalLockEvent(event) {
    if (event?.detail?.source === 'auth') {
        return;
    }
    void lockArchive({
        broadcast: false,
        action: event?.detail?.action || 'lock',
    });
}

/**
 * Best-effort close of the decrypted browser database before re-locking.
 */
async function closeLiveDatabase() {
    try {
        const dbModule = await import('./database.js');
        dbModule.closeDatabase();
    } catch (error) {
        console.warn('Failed to close live database during lock:', error);
    }
}

/**
 * Lock the archive (return to auth screen)
 */
async function lockArchive(options = {}) {
    const { broadcast = false, action = 'lock' } = options;
    invalidateAppInitAttempt();
    unlockInFlight = false;
    decryptInFlight = false;
    activeUnlockRequestId = null;
    activeDecryptRequestId = null;

    // Clear session
    window.cassSession = null;
    clearStoredSession();

    // Tell worker to clear keys
    worker?.postMessage({ type: 'CLEAR_KEYS' });

    if (broadcast) {
        window.dispatchEvent(new CustomEvent('cass:lock', {
            detail: {
                action,
                source: 'auth',
            },
        }));
    }

    await closeLiveDatabase();

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
    if (tofuStatus?.valid === false) {
        clearStoredSession();
        return;
    }

    const restored = restoreSession();
    if (restored) {
        transitionToApp();
    }
}

function getPreferredSessionMode() {
    const savedMode = getStoredMode();
    if (
        savedMode === StorageMode.MEMORY
        || savedMode === StorageMode.SESSION
        || savedMode === StorageMode.LOCAL
    ) {
        return savedMode;
    }
    return StorageMode.MEMORY;
}

function getSessionStorage(mode) {
    try {
        if (mode === StorageMode.SESSION) {
            return sessionStorage;
        }
        if (mode === StorageMode.LOCAL) {
            return localStorage;
        }
    } catch (e) {
        // Ignore
    }
    return null;
}

function persistSession(dekBase64) {
    const mode = getPreferredSessionMode();
    // Remove stale copies from previously selected backends before persisting.
    clearStoredSession();

    const storage = getSessionStorage(mode);
    if (!storage) {
        return;
    }

    const expiry = Date.now() + SESSION_CONFIG.DEFAULT_DURATION_MS;
    const sessionKeys = getSessionKeys();
    try {
        storage.setItem(sessionKeys.DEK, dekBase64);
        storage.setItem(sessionKeys.EXPIRY, expiry.toString());
        storage.setItem(sessionKeys.UNLOCKED, 'true');
    } catch (e) {
        // Ignore write failures
    }
}

function restoreSession() {
    const mode = getPreferredSessionMode();
    const storage = getSessionStorage(mode);
    if (!storage || !config) {
        clearStoredSession();
        return false;
    }

    try {
        const sessionKeys = getSessionKeys();
        const unlocked = storage.getItem(sessionKeys.UNLOCKED);
        const dekStored = storage.getItem(sessionKeys.DEK);
        const expiry = parseInt(storage.getItem(sessionKeys.EXPIRY) || '0', 10);

        if (unlocked !== 'true' || !dekStored) {
            clearStoredSession();
            return false;
        }

        if (Date.now() > expiry) {
            clearStoredSession();
            return false;
        }

        window.cassSession = {
            dek: dekStored,
            config: config,
        };
        return true;
    } catch (e) {
        clearStoredSession();
        return false;
    }
}

function clearStoredSession() {
    const sessionKeys = getSessionKeys();
    for (const storage of [getSessionStorage(StorageMode.SESSION), getSessionStorage(StorageMode.LOCAL)]) {
        if (!storage) {
            continue;
        }
        try {
            for (const key of [
                sessionKeys.DEK,
                sessionKeys.EXPIRY,
                sessionKeys.UNLOCKED,
                LEGACY_SESSION_KEYS.DEK,
                LEGACY_SESSION_KEYS.EXPIRY,
                LEGACY_SESSION_KEYS.UNLOCKED,
            ]) {
                storage.removeItem(key);
            }
        } catch (e) {
            // Ignore
        }
    }
}

/**
 * Dynamically load the viewer module
 */
async function loadViewerModule(initToken = activeAppInitToken) {
    const module = await import('./viewer.js');
    if (!isCurrentAppInitToken(initToken)) {
        return;
    }
    module.init?.();
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
