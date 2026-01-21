/**
 * Password Strength Validation Module
 *
 * Provides real-time password strength validation with visual feedback.
 * Algorithm matches the Rust implementation in src/pages/password.rs.
 *
 * @module password-strength
 */

/**
 * Password strength levels.
 * @typedef {'weak' | 'fair' | 'good' | 'strong'} StrengthLevel
 */

/**
 * Password validation result.
 * @typedef {Object} ValidationResult
 * @property {StrengthLevel} strength - Overall strength level
 * @property {number} score - Computed score (0-7)
 * @property {number} entropyBits - Estimated entropy in bits
 * @property {string[]} suggestions - List of improvement suggestions
 * @property {Object} checks - Individual requirement checks
 * @property {boolean} checks.hasLowercase
 * @property {boolean} checks.hasUppercase
 * @property {boolean} checks.hasDigit
 * @property {boolean} checks.hasSpecial
 * @property {number} checks.length
 * @property {boolean} checks.meetsMinLength
 */

/**
 * Validate a password and return strength assessment with suggestions.
 *
 * Algorithm:
 * 1. Check for presence of lowercase, uppercase, digits, and special characters
 * 2. Compute length score: 0 (0-7), 1 (8-11), 2 (12-15), 3 (16+)
 * 3. Sum all criteria to get score (0-7)
 * 4. Map score to strength level
 *
 * @param {string} password - The password to validate
 * @returns {ValidationResult} Validation result with strength and suggestions
 */
export function validatePassword(password) {
    const length = password.length;
    const hasUpper = /[A-Z]/.test(password);
    const hasLower = /[a-z]/.test(password);
    const hasDigit = /[0-9]/.test(password);
    const hasSpecial = /[^a-zA-Z0-9]/.test(password);

    // Length scoring (0-3 points) - matches Rust implementation
    let lengthScore;
    if (length < 8) {
        lengthScore = 0;
    } else if (length < 12) {
        lengthScore = 1;
    } else if (length < 16) {
        lengthScore = 2;
    } else {
        lengthScore = 3;
    }

    // Total score (0-7)
    const score = lengthScore
        + (hasUpper ? 1 : 0)
        + (hasLower ? 1 : 0)
        + (hasDigit ? 1 : 0)
        + (hasSpecial ? 1 : 0);

    // Collect improvement suggestions
    const suggestions = [];
    if (length < 12) {
        suggestions.push("Use at least 12 characters");
    }
    if (!hasUpper) {
        suggestions.push("Add uppercase letters");
    }
    if (!hasLower) {
        suggestions.push("Add lowercase letters");
    }
    if (!hasDigit) {
        suggestions.push("Add numbers");
    }
    if (!hasSpecial) {
        suggestions.push("Add special characters (!@#$%^&*)");
    }

    // Map score to strength - matches Rust implementation exactly
    let strength;
    if (score <= 2) {
        strength = 'weak';
    } else if (score <= 4) {
        strength = 'fair';
    } else if (score <= 6) {
        strength = 'good';
    } else {
        strength = 'strong';
    }

    // Calculate entropy bits for consistency with Rust
    const entropyBits = estimateEntropy(password);

    return {
        strength,
        score,
        entropyBits,
        suggestions,
        checks: {
            hasLowercase: hasLower,
            hasUppercase: hasUpper,
            hasDigit: hasDigit,
            hasSpecial: hasSpecial,
            length: length,
            meetsMinLength: length >= 12,
        },
    };
}

/**
 * Calculate password entropy using character class analysis.
 * Mirrors the algorithm in password.rs::estimate_entropy.
 *
 * @param {string} password - The password to analyze
 * @returns {number} Estimated entropy in bits
 */
function estimateEntropy(password) {
    if (!password || password.length === 0) {
        return 0.0;
    }

    const hasLower = /[a-z]/.test(password);
    const hasUpper = /[A-Z]/.test(password);
    const hasDigit = /[0-9]/.test(password);
    const hasSpecial = /[^a-zA-Z0-9]/.test(password);

    let poolSize = 0;
    if (hasLower) poolSize += 26;
    if (hasUpper) poolSize += 26;
    if (hasDigit) poolSize += 10;
    if (hasSpecial) poolSize += 32;

    if (poolSize === 0) {
        poolSize = 26; // Assume lowercase if nothing else
    }

    const bitsPerChar = Math.log2(poolSize);
    return bitsPerChar * password.length;
}

/**
 * Get the color for a strength level.
 *
 * @param {StrengthLevel} strength - The strength level
 * @returns {string} CSS color value
 */
export function getStrengthColor(strength) {
    const colors = {
        weak: '#ef4444',    // Red
        fair: '#f59e0b',    // Amber/Yellow
        good: '#3b82f6',    // Blue
        strong: '#22c55e',  // Green
    };
    return colors[strength] || colors.weak;
}

/**
 * Get the percentage width for a strength level's progress bar.
 *
 * @param {StrengthLevel} strength - The strength level
 * @returns {number} Percentage (25, 50, 75, or 100)
 */
export function getStrengthPercent(strength) {
    const percents = {
        weak: 25,
        fair: 50,
        good: 75,
        strong: 100,
    };
    return percents[strength] || 25;
}

/**
 * Get a human-readable label for a strength level.
 *
 * @param {StrengthLevel} strength - The strength level
 * @returns {string} Capitalized label
 */
export function getStrengthLabel(strength) {
    return strength.charAt(0).toUpperCase() + strength.slice(1);
}

/**
 * Create a password strength meter component.
 *
 * @param {HTMLInputElement} passwordInput - The password input element
 * @param {Object} options - Configuration options
 * @param {HTMLElement} [options.meterContainer] - Container for the strength meter
 * @param {HTMLElement} [options.suggestionsList] - UL element for suggestions
 * @param {HTMLElement} [options.labelElement] - Element to display strength label
 * @param {Function} [options.onValidate] - Callback when validation runs
 * @returns {Object} Meter controller with update() and destroy() methods
 */
export function createStrengthMeter(passwordInput, options = {}) {
    const {
        meterContainer,
        suggestionsList,
        labelElement,
        onValidate,
    } = options;

    // Create meter bar if container provided but no existing bar
    let meterBar = null;
    if (meterContainer) {
        meterBar = meterContainer.querySelector('.strength-bar');
        if (!meterBar) {
            meterBar = document.createElement('div');
            meterBar.className = 'strength-bar';
            meterContainer.appendChild(meterBar);
        }
    }

    // Validation handler
    function handleInput() {
        const validation = validatePassword(passwordInput.value);
        update(validation);
        if (onValidate) {
            onValidate(validation);
        }
    }

    // Update UI with validation result
    function update(validation) {
        const { strength, suggestions } = validation;
        const color = getStrengthColor(strength);
        const percent = getStrengthPercent(strength);

        // Update meter bar
        if (meterBar) {
            meterBar.style.width = `${percent}%`;
            meterBar.style.backgroundColor = color;
            meterBar.dataset.strength = strength;
        }

        // Update label
        if (labelElement) {
            labelElement.textContent = getStrengthLabel(strength);
            labelElement.style.color = color;
        }

        // Update suggestions list
        if (suggestionsList) {
            suggestionsList.innerHTML = suggestions
                .map(s => `<li>${escapeHtml(s)}</li>`)
                .join('');
        }
    }

    // Attach event listener
    passwordInput.addEventListener('input', handleInput);

    // Return controller
    return {
        update: handleInput,
        destroy: () => {
            passwordInput.removeEventListener('input', handleInput);
        },
        getValidation: () => validatePassword(passwordInput.value),
    };
}

/**
 * Escape HTML special characters to prevent XSS.
 *
 * @param {string} str - String to escape
 * @returns {string} Escaped string
 */
function escapeHtml(str) {
    const escapeMap = {
        '&': '&amp;',
        '<': '&lt;',
        '>': '&gt;',
        '"': '&quot;',
        "'": '&#39;',
    };
    return str.replace(/[&<>"']/g, char => escapeMap[char]);
}

// Export for use as a module or direct inclusion
if (typeof window !== 'undefined') {
    window.PasswordStrength = {
        validatePassword,
        getStrengthColor,
        getStrengthPercent,
        getStrengthLabel,
        createStrengthMeter,
    };
}
