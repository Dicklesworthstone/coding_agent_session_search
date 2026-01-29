//! JavaScript generation for HTML export.
//!
//! Generates inline JavaScript for:
//! - Search functionality (text search with highlighting)
//! - Theme toggle (light/dark mode)
//! - Tool call expand/collapse
//! - Encryption/decryption (Web Crypto API)

use super::template::ExportOptions;
use tracing::debug;

/// Bundle of JavaScript for the template.
pub struct ScriptBundle {
    /// Inline JavaScript to include in the document
    pub inline_js: String,
}

/// Generate all JavaScript for the template.
pub fn generate_scripts(options: &ExportOptions) -> ScriptBundle {
    let mut scripts = Vec::new();

    // Core utilities
    scripts.push(generate_core_utils());

    // Search functionality
    if options.include_search {
        scripts.push(generate_search_js());
    }

    // Theme toggle
    if options.include_theme_toggle {
        scripts.push(generate_theme_js());
    }

    // Tool call toggle
    if options.show_tool_calls {
        scripts.push(generate_tool_toggle_js());
    }

    // Encryption/decryption
    if options.encrypt {
        scripts.push(generate_decryption_js());
    }

    // World-class UI/UX enhancements (always included)
    scripts.push(generate_world_class_js());

    // Initialize on load
    scripts.push(generate_init_js(options));

    let inline_js = scripts.join("\n\n");
    debug!(
        component = "scripts",
        operation = "generate",
        include_search = options.include_search,
        include_theme_toggle = options.include_theme_toggle,
        show_tool_calls = options.show_tool_calls,
        encrypt = options.encrypt,
        inline_bytes = inline_js.len(),
        "Generated inline scripts"
    );

    ScriptBundle { inline_js }
}

fn generate_core_utils() -> String {
    r#"// Core utilities
const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => document.querySelectorAll(sel);

// Toast notifications
const Toast = {
    container: null,

    init() {
        this.container = document.createElement('div');
        this.container.id = 'toast-container';
        this.container.style.cssText = 'position:fixed;bottom:1rem;right:1rem;z-index:9999;display:flex;flex-direction:column;gap:0.5rem;';
        document.body.appendChild(this.container);
    },

    show(message, type = 'info') {
        if (!this.container) this.init();
        const toast = document.createElement('div');
        toast.className = 'toast toast-' + type;
        toast.style.cssText = 'padding:0.75rem 1rem;background:var(--bg-surface);border:1px solid var(--border);border-radius:6px;color:var(--text-primary);box-shadow:0 4px 12px rgba(0,0,0,0.3);transform:translateX(100%);transition:transform 0.3s ease;';
        toast.textContent = message;
        this.container.appendChild(toast);
        requestAnimationFrame(() => toast.style.transform = 'translateX(0)');
        setTimeout(() => {
            toast.style.transform = 'translateX(100%)';
            setTimeout(() => toast.remove(), 300);
        }, 3000);
    }
};

// Copy to clipboard
async function copyToClipboard(text) {
    try {
        await navigator.clipboard.writeText(text);
        Toast.show('Copied to clipboard', 'success');
        return true;
    } catch (e) {
        // Fallback for older browsers
        const textarea = document.createElement('textarea');
        textarea.value = text;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        try {
            const ok = document.execCommand('copy');
            if (ok) {
                Toast.show('Copied to clipboard', 'success');
                return true;
            }
            Toast.show('Copy failed', 'error');
        } catch (e2) {
            Toast.show('Copy failed', 'error');
        }
        textarea.remove();
    }
    return false;
}

// Copy code block
async function copyCodeBlock(btn) {
    const pre = btn.closest('pre');
    const code = pre.querySelector('code');
    const ok = await copyToClipboard(code ? code.textContent : pre.textContent);
    if (ok) {
        btn.classList.add('copied');
        setTimeout(() => btn.classList.remove('copied'), 1500);
    }
}

// Print handler
function printConversation() {
    // Expand all collapsed sections before print
    $$('details, .tool-call').forEach(el => {
        if (el.tagName === 'DETAILS') el.open = true;
        else el.classList.add('expanded');
    });
    window.print();
}"#
        .to_string()
}

fn generate_search_js() -> String {
    r#"// Search functionality
const Search = {
    input: null,
    countEl: null,
    matches: [],
    currentIndex: -1,

    init() {
        this.input = $('#search-input');
        this.countEl = $('#search-count');
        if (!this.input) return;

        this.input.addEventListener('input', () => this.search());
        this.input.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') {
                e.preventDefault();
                if (e.shiftKey) {
                    this.prev();
                } else {
                    this.next();
                }
            } else if (e.key === 'Escape') {
                this.clear();
                this.input.blur();
            }
        });

        // Keyboard shortcut: Ctrl/Cmd + F for search
        document.addEventListener('keydown', (e) => {
            if ((e.ctrlKey || e.metaKey) && e.key === 'f') {
                e.preventDefault();
                this.input.focus();
                this.input.select();
            }
        });
    },

    search() {
        this.clearHighlights();
        const query = this.input.value.trim().toLowerCase();
        if (!query) {
            this.countEl.hidden = true;
            return;
        }

        this.matches = [];
        const messages = $$('.message-content');
        messages.forEach((el) => {
            const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
            let node;
            while ((node = walker.nextNode())) {
                const text = node.textContent.toLowerCase();
                let index = text.indexOf(query);
                while (index !== -1) {
                    this.matches.push({ node, index, length: query.length });
                    index = text.indexOf(query, index + 1);
                }
            }
        });

        this.highlightAll();
        this.updateCount();

        if (this.matches.length > 0) {
            this.currentIndex = 0;
            this.scrollToCurrent();
        }
    },

    highlightAll() {
        // Process in reverse to preserve indices
        for (let i = this.matches.length - 1; i >= 0; i--) {
            const match = this.matches[i];
            const range = document.createRange();
            try {
                range.setStart(match.node, match.index);
                range.setEnd(match.node, match.index + match.length);
                const span = document.createElement('span');
                span.className = 'search-highlight';
                span.dataset.matchIndex = i;
                range.surroundContents(span);
            } catch (e) {
                // Skip invalid ranges
            }
        }
    },

    clearHighlights() {
        $$('.search-highlight').forEach((el) => {
            const parent = el.parentNode;
            while (el.firstChild) {
                parent.insertBefore(el.firstChild, el);
            }
            parent.removeChild(el);
        });
        this.matches = [];
        this.currentIndex = -1;
    },

    updateCount() {
        if (this.matches.length > 0) {
            this.countEl.textContent = `${this.currentIndex + 1}/${this.matches.length}`;
            this.countEl.hidden = false;
        } else {
            this.countEl.textContent = 'No results';
            this.countEl.hidden = false;
        }
    },

    scrollToCurrent() {
        $$('.search-current').forEach((el) => el.classList.remove('search-current'));
        if (this.currentIndex >= 0 && this.currentIndex < this.matches.length) {
            const highlight = $(`[data-match-index="${this.currentIndex}"]`);
            if (highlight) {
                highlight.classList.add('search-current');
                highlight.scrollIntoView({ behavior: 'smooth', block: 'center' });
            }
        }
        this.updateCount();
    },

    next() {
        if (this.matches.length === 0) return;
        this.currentIndex = (this.currentIndex + 1) % this.matches.length;
        this.scrollToCurrent();
    },

    prev() {
        if (this.matches.length === 0) return;
        this.currentIndex = (this.currentIndex - 1 + this.matches.length) % this.matches.length;
        this.scrollToCurrent();
    },

    clear() {
        this.input.value = '';
        this.clearHighlights();
        this.countEl.hidden = true;
    }
};"#
    .to_string()
}

fn generate_theme_js() -> String {
    r#"// Theme toggle
const Theme = {
    toggle: null,

    init() {
        this.toggle = $('#theme-toggle');
        if (!this.toggle) return;

        // Load saved preference or system preference
        const saved = localStorage.getItem('cass-theme');
        const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
        const theme = saved || (prefersDark ? 'dark' : 'light');
        document.documentElement.setAttribute('data-theme', theme);

        this.toggle.addEventListener('click', () => this.toggleTheme());

        // Listen for system theme changes
        window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', (e) => {
            if (!localStorage.getItem('cass-theme')) {
                document.documentElement.setAttribute('data-theme', e.matches ? 'dark' : 'light');
            }
        });
    },

    toggleTheme() {
        const current = document.documentElement.getAttribute('data-theme');
        const next = current === 'dark' ? 'light' : 'dark';
        document.documentElement.setAttribute('data-theme', next);
        localStorage.setItem('cass-theme', next);
    }
};"#
    .to_string()
}

fn generate_tool_toggle_js() -> String {
    r#"// Tool call expand/collapse
const ToolCalls = {
    init() {
        $$('.tool-call-header').forEach((header) => {
            header.addEventListener('click', () => {
                const toolCall = header.closest('.tool-call');
                toolCall.classList.toggle('expanded');
            });
        });
    }
};

// Tool badge popover controller
const ToolPopovers = {
    activePopover: null,
    activeBadge: null,

    init() {
        this.initBadges();
        this.initOverflowBadges();
        this.initOutsideClick();
    },

    initBadges() {
        $$('.tool-badge:not(.tool-overflow)').forEach(badge => {
            // Helper to get popover - looks up fresh each time since popover may be built dynamically
            const getPopover = () => badge.querySelector('.tool-popover');

            // Show on hover (desktop)
            badge.addEventListener('mouseenter', () => this.show(badge, getPopover()));
            badge.addEventListener('mouseleave', () => this.hide(badge, getPopover()));

            // Show on focus (keyboard accessibility)
            badge.addEventListener('focus', () => this.show(badge, getPopover()));
            badge.addEventListener('blur', (e) => {
                // Don't hide if focus moves within the popover
                const popover = getPopover();
                if (!popover || !popover.contains(e.relatedTarget)) {
                    this.hide(badge, popover);
                }
            });

            // Toggle on click (mobile support)
            badge.addEventListener('click', (e) => {
                e.preventDefault();
                e.stopPropagation();
                this.toggle(badge, getPopover());
            });

            // Keyboard support
            badge.addEventListener('keydown', (e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    this.toggle(badge, getPopover());
                } else if (e.key === 'Escape') {
                    this.hide(badge, getPopover());
                    badge.focus();
                }
            });
        });
    },

    initOverflowBadges() {
        $$('.tool-overflow').forEach(btn => {
            // Store original text
            btn.dataset.originalText = btn.textContent.trim();

            btn.addEventListener('click', (e) => {
                e.preventDefault();
                e.stopPropagation();
                const container = btn.closest('.message-header-right');
                if (!container) return;

                const isExpanded = container.classList.toggle('expanded');
                btn.textContent = isExpanded ? 'Less' : btn.dataset.originalText;
                btn.setAttribute('aria-expanded', isExpanded);
            });
        });
    },

    initOutsideClick() {
        document.addEventListener('click', (e) => {
            if (!e.target.closest('.tool-badge')) {
                this.hideAll();
            }
        });
    },

    show(badge, popover) {
        if (!popover) {
            // Build popover from data attributes if not present
            popover = this.buildPopover(badge);
            if (!popover) return;
        }

        // Hide any other active popover first
        if (this.activeBadge && this.activeBadge !== badge) {
            this.hide(this.activeBadge, this.activePopover);
        }

        popover.classList.add('visible');
        badge.setAttribute('aria-expanded', 'true');
        this.position(badge, popover);

        this.activePopover = popover;
        this.activeBadge = badge;
    },

    hide(badge, popover) {
        if (popover) {
            popover.classList.remove('visible');
        }
        if (badge) {
            badge.setAttribute('aria-expanded', 'false');
        }
        if (this.activeBadge === badge) {
            this.activePopover = null;
            this.activeBadge = null;
        }
    },

    hideAll() {
        $$('.tool-popover.visible').forEach(p => {
            p.classList.remove('visible');
        });
        $$('.tool-badge[aria-expanded="true"]').forEach(b => {
            b.setAttribute('aria-expanded', 'false');
        });
        this.activePopover = null;
        this.activeBadge = null;
    },

    toggle(badge, popover) {
        const isVisible = popover && popover.classList.contains('visible');
        if (isVisible) {
            this.hide(badge, popover);
        } else {
            this.show(badge, popover);
        }
    },

    buildPopover(badge) {
        // Build a popover from data attributes if no inline popover exists
        const name = badge.dataset.toolName;
        const input = badge.dataset.toolInput;
        const output = badge.dataset.toolOutput;

        if (!name) return null;

        const popover = document.createElement('div');
        popover.className = 'tool-popover';
        popover.setAttribute('role', 'tooltip');

        let html = '<div class="tool-popover-header"><strong>' + this.escapeHtml(name) + '</strong></div>';

        if (input && input.trim()) {
            html += '<div class="tool-popover-section"><span class="tool-popover-label">Input</span><pre><code>' + this.escapeHtml(input) + '</code></pre></div>';
        }

        if (output && output.trim()) {
            html += '<div class="tool-popover-section"><span class="tool-popover-label">Output</span><pre><code>' + this.escapeHtml(output) + '</code></pre></div>';
        }

        popover.innerHTML = html;
        badge.appendChild(popover);
        return popover;
    },

    escapeHtml(text) {
        const div = document.createElement('div');
        div.textContent = text;
        return div.innerHTML;
    },

    position(badge, popover) {
        // Skip positioning on mobile - CSS handles bottom sheet style
        if (window.innerWidth < 768) {
            return;
        }

        // Use fixed positioning relative to viewport
        const badgeRect = badge.getBoundingClientRect();
        const viewportWidth = window.innerWidth;
        const viewportHeight = window.innerHeight;
        const margin = 8;

        // Measure popover dimensions (temporarily make visible for measurement)
        popover.style.visibility = 'hidden';
        popover.style.display = 'block';
        const popoverRect = popover.getBoundingClientRect();
        popover.style.display = '';
        popover.style.visibility = '';

        // Default: position below and align left edge with badge
        let top = badgeRect.bottom + margin;
        let left = badgeRect.left;

        // Flip up if would overflow bottom
        if (top + popoverRect.height > viewportHeight - margin) {
            top = badgeRect.top - popoverRect.height - margin;
            popover.classList.add('popover-above');
        } else {
            popover.classList.remove('popover-above');
        }

        // Flip to align right edge if would overflow right
        if (left + popoverRect.width > viewportWidth - margin) {
            left = Math.max(margin, badgeRect.right - popoverRect.width);
        }

        // Ensure not off left edge
        left = Math.max(margin, left);

        // Ensure not off top edge
        top = Math.max(margin, top);

        popover.style.top = top + 'px';
        popover.style.left = left + 'px';
    }
};"#
    .to_string()
}

fn generate_world_class_js() -> String {
    r#"// World-class UI/UX enhancements
const WorldClass = {
    scrollProgress: null,
    floatingNav: null,
    gradientMesh: null,
    lastScrollY: 0,
    ticking: false,
    currentMessageIndex: -1,
    messages: [],

    init() {
        this.messages = Array.from($$('.message'));
        this.scrollProgress = $('#scroll-progress');
        this.floatingNav = $('#floating-nav');
        this.initFloatingNav();
        this.initIntersectionObserver();
        this.initKeyboardNav();
        this.initMessageLinks();
        this.initScrollHandler();
        this.initShareButton();
    },

    initFloatingNav() {
        if (!this.floatingNav) return;

        const scrollTopBtn = $('#scroll-top');
        if (scrollTopBtn) {
            scrollTopBtn.onclick = () => {
                window.scrollTo({ top: 0, behavior: 'smooth' });
            };
        }
    },

    initScrollHandler() {
        const toolbar = $('.toolbar');
        let lastScrollY = window.scrollY;
        let scrollDirection = 'up';

        const updateScroll = () => {
            const scrollY = window.scrollY;
            const scrollHeight = document.documentElement.scrollHeight - window.innerHeight;
            const progress = scrollHeight > 0 ? (scrollY / scrollHeight) * 100 : 0;

            // Update progress bar
            if (this.scrollProgress) {
                this.scrollProgress.style.width = `${progress}%`;
            }

            // Show/hide floating nav
            if (this.floatingNav) {
                if (scrollY > 300) {
                    this.floatingNav.classList.add('visible');
                } else {
                    this.floatingNav.classList.remove('visible');
                }
            }

            // Mobile: hide toolbar on scroll down (only if wide enough scroll)
            if (toolbar && window.innerWidth < 768) {
                scrollDirection = scrollY > lastScrollY ? 'down' : 'up';
                if (scrollDirection === 'down' && scrollY > 200) {
                    toolbar.classList.add('toolbar-hidden');
                } else {
                    toolbar.classList.remove('toolbar-hidden');
                }
            }

            lastScrollY = scrollY;
            this.ticking = false;
        };

        window.addEventListener('scroll', () => {
            if (!this.ticking) {
                requestAnimationFrame(updateScroll);
                this.ticking = true;
            }
        }, { passive: true });
    },

    initIntersectionObserver() {
        if (!('IntersectionObserver' in window)) return;

        const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
        if (reduceMotion) {
            this.messages.forEach((msg) => {
                msg.style.opacity = '1';
                msg.style.transform = 'none';
                msg.classList.add('in-view');
            });
            return;
        }

        const observer = new IntersectionObserver((entries) => {
            entries.forEach(entry => {
                if (entry.isIntersecting) {
                    entry.target.classList.add('in-view');
                    observer.unobserve(entry.target);
                }
            });
        }, {
            threshold: 0.1,
            rootMargin: '0px 0px -50px 0px'
        });

        // Initially hide messages for animation
        // Must match CSS @keyframes messageReveal 'from' state exactly
        this.messages.forEach((msg, i) => {
            msg.style.opacity = '0';
            msg.style.transform = 'translateY(24px) scale(0.97)';
            setTimeout(() => observer.observe(msg), i * 30);
        });
    },

    initKeyboardNav() {
        document.addEventListener('keydown', (e) => {
            // Ignore if in input/textarea
            if (e.target.matches('input, textarea')) return;

            switch(e.key) {
                case 'j':
                    e.preventDefault();
                    this.navigateMessage(1);
                    break;
                case 'k':
                    e.preventDefault();
                    this.navigateMessage(-1);
                    break;
                case 'g':
                    e.preventDefault();
                    this.navigateToMessage(0);
                    break;
                case 'G':
                    e.preventDefault();
                    this.navigateToMessage(this.messages.length - 1);
                    break;
                case '/':
                    if (!e.ctrlKey && !e.metaKey) {
                        e.preventDefault();
                        const searchInput = $('#search-input');
                        if (searchInput) {
                            searchInput.focus();
                            searchInput.select();
                        }
                    }
                    break;
                case '?':
                    e.preventDefault();
                    this.showShortcutsHint();
                    break;
            }
        });
    },

    navigateMessage(direction) {
        const newIndex = Math.max(0, Math.min(this.messages.length - 1, this.currentMessageIndex + direction));
        this.navigateToMessage(newIndex);
    },

    navigateToMessage(index) {
        // Remove focus from current
        if (this.currentMessageIndex >= 0 && this.messages[this.currentMessageIndex]) {
            this.messages[this.currentMessageIndex].classList.remove('keyboard-focus');
        }

        this.currentMessageIndex = index;
        const msg = this.messages[index];
        if (msg) {
            msg.classList.add('keyboard-focus');
            msg.scrollIntoView({ behavior: 'smooth', block: 'center' });
        }
    },

    showShortcutsHint() {
        let hint = $('.shortcuts-hint');
        if (!hint) {
            hint = document.createElement('div');
            hint.className = 'shortcuts-hint';
            hint.innerHTML = '<kbd>j</kbd>/<kbd>k</kbd> navigate • <kbd>g</kbd> first • <kbd>G</kbd> last • <kbd>/</kbd> search • <kbd>?</kbd> help';
            document.body.appendChild(hint);
        }
        hint.classList.add('visible');
        setTimeout(() => hint.classList.remove('visible'), 3000);
    },

    initMessageLinks() {
        this.messages.forEach((msg, i) => {
            const btn = document.createElement('button');
            btn.className = 'message-link';
            btn.title = 'Copy link to message';
            btn.setAttribute('aria-label', 'Copy link to message');
            btn.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71"/></svg>';
            btn.onclick = (e) => {
                e.stopPropagation();
                const id = msg.id || `msg-${i}`;
                if (!msg.id) msg.id = id;
                const url = `${window.location.href.split('#')[0]}#${id}`;
                copyToClipboard(url);
                btn.classList.add('copied');
                setTimeout(() => btn.classList.remove('copied'), 1500);
            };
            msg.appendChild(btn);
        });
    },

    initShareButton() {
        if (!navigator.share) return;

        const toolbar = $('.toolbar');
        if (!toolbar) return;

        const shareBtn = document.createElement('button');
        shareBtn.className = 'toolbar-btn';
        shareBtn.title = 'Share';
        shareBtn.setAttribute('aria-label', 'Share');
        shareBtn.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M4 12v8a2 2 0 002 2h12a2 2 0 002-2v-8"/><polyline points="16,6 12,2 8,6"/><line x1="12" y1="2" x2="12" y2="15"/></svg>';
        shareBtn.onclick = async () => {
            try {
                await navigator.share({
                    title: document.title,
                    url: window.location.href
                });
            } catch (e) {
                if (e.name !== 'AbortError') {
                    Toast.show('Share failed', 'error');
                }
            }
        };
        toolbar.appendChild(shareBtn);
    }
};

// Touch ripple effect for mobile
function createRipple(event) {
    const button = event.currentTarget;
    const rect = button.getBoundingClientRect();
    const ripple = document.createElement('span');
    const size = Math.max(rect.width, rect.height);
    ripple.style.width = ripple.style.height = `${size}px`;
    ripple.style.left = `${event.clientX - rect.left - size/2}px`;
    ripple.style.top = `${event.clientY - rect.top - size/2}px`;
    ripple.className = 'ripple';
    button.appendChild(ripple);
    setTimeout(() => ripple.remove(), 600);
}

// Add ripple to touch devices
if ('ontouchstart' in window) {
    document.addEventListener('DOMContentLoaded', () => {
        $$('.toolbar button, .floating-btn').forEach(btn => {
            btn.addEventListener('touchstart', createRipple);
        });
    });
}"#
    .to_string()
}

fn generate_decryption_js() -> String {
    r#"// Decryption using Web Crypto API
const Crypto = {
    modal: null,
    form: null,
    errorEl: null,

    init() {
        this.modal = $('#password-modal');
        this.form = $('#password-form');
        this.errorEl = $('#decrypt-error');

        if (!this.modal || !this.form) return;

        this.form.addEventListener('submit', (e) => {
            e.preventDefault();
            this.decrypt();
        });
    },

    async decrypt() {
        const password = $('#password-input').value;
        if (!password) return;

        try {
            this.errorEl.hidden = true;

            // Get encrypted content
            const encryptedEl = $('#encrypted-content');
            if (!encryptedEl) throw new Error('No encrypted content found');

            const encryptedData = JSON.parse(encryptedEl.textContent);
            const { salt, iv, ciphertext, iterations } = encryptedData;
            if (!salt || !iv || !ciphertext || !Number.isInteger(iterations) || iterations <= 0) {
                throw new Error('Invalid encryption parameters');
            }

            // Derive key from password
            const enc = new TextEncoder();
            const keyMaterial = await crypto.subtle.importKey(
                'raw',
                enc.encode(password),
                'PBKDF2',
                false,
                ['deriveBits', 'deriveKey']
            );

            const key = await crypto.subtle.deriveKey(
                {
                    name: 'PBKDF2',
                    salt: this.base64ToBytes(salt),
                    iterations: iterations,
                    hash: 'SHA-256'
                },
                keyMaterial,
                { name: 'AES-GCM', length: 256 },
                false,
                ['decrypt']
            );

            // Decrypt
            const decrypted = await crypto.subtle.decrypt(
                {
                    name: 'AES-GCM',
                    iv: this.base64ToBytes(iv)
                },
                key,
                this.base64ToBytes(ciphertext)
            );

            // Replace content
            const dec = new TextDecoder();
            const plaintext = dec.decode(decrypted);
            const conversation = $('#conversation');
            conversation.innerHTML = plaintext;

            // Hide modal
            this.modal.hidden = true;
            this.form.reset();

            // Re-initialize tool calls and popovers
            if (typeof ToolCalls !== 'undefined') {
                ToolCalls.init();
            }
            if (typeof ToolPopovers !== 'undefined') {
                ToolPopovers.init();
            }

        } catch (e) {
            this.errorEl.textContent = 'Decryption failed. Wrong password?';
            this.errorEl.hidden = false;
        }
    },

    base64ToBytes(base64) {
        const binary = atob(base64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
            bytes[i] = binary.charCodeAt(i);
        }
        return bytes;
    }
};"#
    .to_string()
}

fn generate_init_js(options: &ExportOptions) -> String {
    let mut inits = Vec::new();

    if options.include_search {
        inits.push("Search.init();");
    }

    if options.include_theme_toggle {
        inits.push("Theme.init();");
    }

    if options.show_tool_calls {
        inits.push("ToolCalls.init();");
        inits.push("ToolPopovers.init();");
    }

    if options.encrypt {
        inits.push("Crypto.init();");
    }

    // World-class UI/UX enhancements (always init)
    inits.push("WorldClass.init();");

    // Always add code block copy buttons and print button handler
    inits.push(r#"// Add copy buttons to code blocks
    $$('pre code').forEach((code) => {
        const pre = code.parentNode;
        const btn = document.createElement('button');
        btn.className = 'copy-code-btn';
        btn.innerHTML = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1"/></svg>';
        btn.title = 'Copy code';
        btn.setAttribute('aria-label', 'Copy code');
        btn.onclick = () => copyCodeBlock(btn);
        btn.style.cssText = 'position:absolute;top:0.5rem;right:0.5rem;padding:0.25rem;background:var(--bg-surface);border:1px solid var(--border);border-radius:4px;color:var(--text-muted);cursor:pointer;transition:opacity 0.2s;';
        pre.style.position = 'relative';
        pre.appendChild(btn);
    });

    // Print button handler
    const printBtn = $('#print-btn');
    if (printBtn) printBtn.addEventListener('click', printConversation);

    // Global keyboard shortcut: Ctrl/Cmd + P for print
    document.addEventListener('keydown', (e) => {
        if ((e.ctrlKey || e.metaKey) && e.key === 'p') {
            e.preventDefault();
            printConversation();
        }
    });"#);

    format!(
        r#"// Initialize on DOM ready
document.addEventListener('DOMContentLoaded', () => {{
    {}
}});"#,
        inits.join("\n    ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_scripts_includes_search() {
        let opts = ExportOptions {
            include_search: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        assert!(bundle.inline_js.contains("const Search"));
        assert!(bundle.inline_js.contains("Search.init()"));
    }

    #[test]
    fn test_generate_scripts_excludes_search_when_disabled() {
        let opts = ExportOptions {
            include_search: false,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        assert!(!bundle.inline_js.contains("const Search"));
    }

    #[test]
    fn test_generate_scripts_includes_theme_toggle() {
        let opts = ExportOptions {
            include_theme_toggle: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        assert!(bundle.inline_js.contains("const Theme"));
        assert!(bundle.inline_js.contains("localStorage.getItem"));
    }

    #[test]
    fn test_generate_scripts_includes_encryption() {
        let opts = ExportOptions {
            encrypt: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        assert!(bundle.inline_js.contains("const Crypto"));
        assert!(bundle.inline_js.contains("crypto.subtle"));
    }

    #[test]
    fn test_generate_scripts_includes_toast_and_copy() {
        let opts = ExportOptions::default();
        let bundle = generate_scripts(&opts);

        // Toast notifications
        assert!(bundle.inline_js.contains("const Toast"));
        assert!(bundle.inline_js.contains("Toast.show"));

        // Copy to clipboard
        assert!(bundle.inline_js.contains("copyToClipboard"));
        assert!(bundle.inline_js.contains("navigator.clipboard"));

        // Fallback for older browsers
        assert!(bundle.inline_js.contains("execCommand"));
    }

    #[test]
    fn test_generate_scripts_includes_print_handler() {
        let opts = ExportOptions::default();
        let bundle = generate_scripts(&opts);

        assert!(bundle.inline_js.contains("printConversation"));
        assert!(bundle.inline_js.contains("window.print"));
    }

    #[test]
    fn test_generate_scripts_includes_keyboard_shortcuts() {
        let opts = ExportOptions {
            include_search: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        // Ctrl+F for search
        assert!(bundle.inline_js.contains("e.key === 'f'"));
        // Ctrl+P for print
        assert!(bundle.inline_js.contains("e.key === 'p'"));
        // Escape to clear
        assert!(bundle.inline_js.contains("'Escape'"));
    }

    #[test]
    fn test_generate_scripts_includes_copy_code_buttons() {
        let opts = ExportOptions::default();
        let bundle = generate_scripts(&opts);

        assert!(bundle.inline_js.contains("copy-code-btn"));
        assert!(bundle.inline_js.contains("copyCodeBlock"));
    }

    #[test]
    fn test_generate_scripts_includes_world_class_enhancements() {
        let opts = ExportOptions::default();
        let bundle = generate_scripts(&opts);

        // WorldClass object and initialization
        assert!(bundle.inline_js.contains("const WorldClass"));
        assert!(bundle.inline_js.contains("WorldClass.init()"));

        // Scroll progress indicator
        assert!(bundle.inline_js.contains("scroll-progress"));

        // Floating navigation
        assert!(bundle.inline_js.contains("initFloatingNav"));
        assert!(bundle.inline_js.contains("scroll-top"));

        // Keyboard navigation (vim-style j/k)
        assert!(bundle.inline_js.contains("initKeyboardNav"));
        assert!(bundle.inline_js.contains("case 'j':"));
        assert!(bundle.inline_js.contains("case 'k':"));

        // Message link copying
        assert!(bundle.inline_js.contains("initMessageLinks"));
        assert!(bundle.inline_js.contains("message-link"));

        // Intersection observer for animations
        assert!(bundle.inline_js.contains("IntersectionObserver"));
        assert!(bundle.inline_js.contains("in-view"));

        // Native share API support
        assert!(bundle.inline_js.contains("navigator.share"));

        // Touch ripple effect
        assert!(bundle.inline_js.contains("createRipple"));
    }

    #[test]
    fn test_world_class_keyboard_shortcuts() {
        let opts = ExportOptions::default();
        let bundle = generate_scripts(&opts);

        // Vim-style navigation
        assert!(bundle.inline_js.contains("navigateMessage(1)")); // j - next
        assert!(bundle.inline_js.contains("navigateMessage(-1)")); // k - previous

        // Jump to first/last (g/G)
        assert!(bundle.inline_js.contains("case 'g':"));

        // Search shortcut (/)
        assert!(bundle.inline_js.contains("case '/':"));

        // Help shortcut (?)
        assert!(bundle.inline_js.contains("case '?':"));
        assert!(bundle.inline_js.contains("showShortcutsHint"));
    }

    #[test]
    fn test_tool_popovers_functionality() {
        let opts = ExportOptions {
            show_tool_calls: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        // ToolPopovers object exists
        assert!(bundle.inline_js.contains("const ToolPopovers"));
        assert!(bundle.inline_js.contains("ToolPopovers.init()"));

        // Hover support (desktop)
        assert!(bundle.inline_js.contains("mouseenter"));
        assert!(bundle.inline_js.contains("mouseleave"));

        // Focus support (keyboard accessibility)
        assert!(bundle.inline_js.contains("addEventListener('focus'"));
        assert!(bundle.inline_js.contains("addEventListener('blur'"));

        // Click support (mobile/touch)
        assert!(
            bundle
                .inline_js
                .contains("this.toggle(badge, getPopover())")
        );

        // Escape key support
        assert!(bundle.inline_js.contains("e.key === 'Escape'"));

        // aria-expanded updates
        assert!(bundle.inline_js.contains("setAttribute('aria-expanded'"));

        // Viewport positioning
        assert!(bundle.inline_js.contains("getBoundingClientRect"));
        assert!(bundle.inline_js.contains("viewportWidth"));
        assert!(bundle.inline_js.contains("viewportHeight"));

        // Overflow badge expansion
        assert!(bundle.inline_js.contains("initOverflowBadges"));
        assert!(bundle.inline_js.contains("tool-overflow"));

        // Outside click to close
        assert!(bundle.inline_js.contains("initOutsideClick"));
        assert!(bundle.inline_js.contains("hideAll"));
    }

    #[test]
    fn test_tool_popovers_reinit_after_decryption() {
        let opts = ExportOptions {
            encrypt: true,
            show_tool_calls: true,
            ..Default::default()
        };
        let bundle = generate_scripts(&opts);

        // After decryption, both ToolCalls and ToolPopovers should be reinitialized
        assert!(bundle.inline_js.contains("ToolCalls.init()"));
        assert!(bundle.inline_js.contains("ToolPopovers.init()"));
    }
}
