/**
 * cass Archive Conversation Viewer
 *
 * Displays conversation messages with markdown rendering and syntax highlighting.
 * CSP-safe: No inline styles or eval-based rendering.
 * Uses virtual scrolling for long conversations with 50+ messages.
 */

import { getConversation, getConversationMessages, checkMemoryPressure, getMemoryUsage } from './database.js';
import {
    createAttachmentElement,
    getMessageAttachments,
    initAttachments,
    reset as resetAttachments,
} from './attachments.js';
import { VariableHeightVirtualList } from './virtual-list.js';

// Virtual scrolling configuration
const VIRTUAL_CONFIG = {
    MESSAGE_THRESHOLD: 50, // Use virtual scrolling above this message count
    ESTIMATED_MESSAGE_HEIGHT: 150, // Estimated average message height
    OVERSCAN: 3, // Extra items to render above/below viewport
};

// Memory management configuration
const MEMORY_CONFIG = {
    MAX_LOADED_CONVERSATIONS: 5, // Maximum conversations to keep in memory
    MEMORY_CHECK_INTERVAL: 30000, // Check memory every 30 seconds
    MEMORY_WARNING_THRESHOLD: 80, // Warn at 80% memory usage
};

// LRU cache for loaded conversations
const loadedConversations = new Map();
let memoryCheckIntervalId = null;

// DOMPurify configuration for XSS prevention
const SANITIZE_CONFIG = {
    ALLOWED_TAGS: [
        'p', 'br', 'strong', 'em', 'b', 'i', 'code', 'pre', 'ul', 'ol', 'li',
        'a', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'blockquote', 'mark', 'span',
        'table', 'thead', 'tbody', 'tr', 'th', 'td', 'hr', 'del', 'sup', 'sub',
    ],
    ALLOWED_ATTR: ['href', 'title', 'class', 'data-language', 'id', 'name'],
    ALLOW_DATA_ATTR: false,
    FORBID_TAGS: ['script', 'style', 'iframe', 'object', 'embed', 'form', 'input'],
    FORBID_ATTR: ['onerror', 'onclick', 'onload', 'onmouseover', 'style'],
};

// Module state
let currentConversation = null;
let currentMessages = [];
let onBack = null;
let messageVirtualList = null; // Virtual list for long conversations
let attachmentState = createAttachmentState();
let activeConversationLoadId = 0;

// DOM element references
let elements = {
    container: null,
    header: null,
    messagesList: null,
};

/**
 * Initialize the conversation viewer
 * @param {HTMLElement} container - Container element
 * @param {Function} backCallback - Callback when back button is clicked
 */
export function initConversationViewer(container, backCallback) {
    elements.container = container;
    onBack = backCallback;
    window.addEventListener('cass:lock', handleArchiveLock);
}

/**
 * Load and display a conversation with LRU caching
 * @param {number} conversationId - Conversation ID
 * @param {number|null} highlightMessageId - Message ID to highlight/scroll to
 */
export async function loadConversation(conversationId, highlightMessageId = null) {
    const loadId = ++activeConversationLoadId;
    let conversation;
    let messages;

    // Check if conversation is already in cache
    if (loadedConversations.has(conversationId)) {
        const cached = loadedConversations.get(conversationId);
        // Move to end of map (most recently used)
        loadedConversations.delete(conversationId);
        loadedConversations.set(conversationId, cached);
        conversation = cached.conversation;
        messages = cached.messages;
        console.debug(`[Conversation] Using cached conversation ${conversationId}`);
    } else {
        // Unload oldest conversation if at limit
        if (loadedConversations.size >= MEMORY_CONFIG.MAX_LOADED_CONVERSATIONS) {
            unloadOldestConversation();
        }

        // Load conversation metadata
        conversation = getConversation(conversationId);

        if (!conversation) {
            if (loadId === activeConversationLoadId) {
                showError('Conversation not found');
            }
            return;
        }

        // Load messages
        messages = getConversationMessages(conversationId);

        // Cache the loaded data
        loadedConversations.set(conversationId, {
            conversation,
            messages,
            loadedAt: Date.now(),
        });
        console.debug(`[Conversation] Loaded and cached conversation ${conversationId} (cache size: ${loadedConversations.size})`);
    }

    // Check memory pressure
    if (checkMemoryPressure()) {
        showMemoryWarning();
    }

    await ensureAttachmentsReady();

    if (loadId !== activeConversationLoadId) {
        return;
    }

    currentConversation = conversation;
    currentMessages = messages;

    // Render the view
    render(conversation, messages, highlightMessageId);
}

function createAttachmentState() {
    return {
        ready: false,
        available: false,
        dek: null,
        exportId: null,
    };
}

async function ensureAttachmentsReady() {
    if (attachmentState.ready) {
        return attachmentState.available;
    }

    const session = window.cassSession;
    const dekBase64 = session?.dek;
    const exportIdBase64 = session?.config?.export_id;

    if (!dekBase64 || !exportIdBase64) {
        attachmentState.ready = true;
        attachmentState.available = false;
        return false;
    }

    try {
        attachmentState.dek = base64ToBytes(dekBase64);
        attachmentState.exportId = base64ToBytes(exportIdBase64);
        const manifest = await initAttachments(attachmentState.dek, attachmentState.exportId);
        attachmentState.available = Boolean(manifest?.entries?.length);
    } catch (error) {
        console.warn('[Conversation] Attachment manifest unavailable:', error);
        attachmentState.available = false;
    }

    attachmentState.ready = true;
    return attachmentState.available;
}

/**
 * Render the conversation view
 * Uses virtual scrolling for long conversations (> MESSAGE_THRESHOLD)
 */
function render(conv, messages, highlightId) {
    // Clean up previous virtual list
    destroyVirtualList();

    const formattedDate = formatDate(conv.started_at);
    const duration = conv.ended_at ? formatDuration(conv.ended_at - conv.started_at) : null;
    const useVirtualScrolling = messages.length > VIRTUAL_CONFIG.MESSAGE_THRESHOLD;

    elements.container.innerHTML = `
        <div class="conversation-container">
            <header class="conversation-header">
                <button id="back-btn" type="button" class="back-btn" aria-label="Back to search">
                    ←
                </button>
                <div class="conversation-title">
                    <h2>${escapeHtml(conv.title || 'Untitled conversation')}</h2>
                    <div class="meta">
                        <span class="conv-agent">${escapeHtml(formatAgentName(conv.agent))}</span>
                        <span class="conv-date">${escapeHtml(formattedDate)}</span>
                        ${duration ? `<span class="conv-duration">${escapeHtml(duration)}</span>` : ''}
                        <span class="conv-count">${conv.message_count} message${conv.message_count !== 1 ? 's' : ''}</span>
                        ${useVirtualScrolling ? '<span class="virtual-indicator" title="Virtual scrolling enabled for performance">⚡</span>' : ''}
                    </div>
                </div>
                <div class="conversation-actions">
                    <button id="copy-btn" type="button" class="btn btn-small" aria-label="Copy conversation">
                        📋 Copy
                    </button>
                </div>
            </header>

            ${conv.workspace ? `
                <div class="conversation-workspace">
                    <span class="workspace-label">Workspace:</span>
                    <code>${escapeHtml(conv.workspace)}</code>
                </div>
            ` : ''}

            <div class="messages-list ${useVirtualScrolling ? 'virtual-messages' : ''}" id="messages-list">
            </div>
        </div>
    `;

    // Cache element references
    elements.header = elements.container.querySelector('.conversation-header');
    elements.messagesList = document.getElementById('messages-list');

    // Render messages (virtual or direct)
    if (useVirtualScrolling) {
        renderVirtualMessages(messages, highlightId);
    } else {
        renderDirectMessages(messages, highlightId);
    }

    // Set up event listeners
    setupEventListeners();

    // Scroll to highlighted message (for direct rendering)
    if (highlightId && !useVirtualScrolling) {
        scrollToMessage(highlightId);
    }
}

/**
 * Render messages using virtual scrolling
 * @private
 */
function renderVirtualMessages(messages, highlightId) {
    // Set up container for virtual scrolling
    elements.messagesList.style.height = 'calc(100vh - 200px)';
    elements.messagesList.style.minHeight = '400px';
    elements.messagesList.style.overflow = 'auto';

    // Create virtual list
    messageVirtualList = new VariableHeightVirtualList({
        container: elements.messagesList,
        totalCount: messages.length,
        estimatedItemHeight: VIRTUAL_CONFIG.ESTIMATED_MESSAGE_HEIGHT,
        renderItem: (index) => createMessageElement(messages[index], index, messages[index].id === highlightId),
        overscan: VIRTUAL_CONFIG.OVERSCAN,
    });

    console.debug(`[Conversation] Using virtual scrolling for ${messages.length} messages`);

    // Scroll to highlighted message if specified
    if (highlightId) {
        const highlightIndex = messages.findIndex(m => m.id === highlightId);
        if (highlightIndex >= 0) {
            setTimeout(() => {
                messageVirtualList.scrollToIndex(highlightIndex, 'center');
            }, 100);
        }
    }
}

/**
 * Render messages directly (for short conversations)
 * @private
 */
function renderDirectMessages(messages, highlightId) {
    const html = messages.map((msg, idx) => renderMessage(msg, idx, msg.id === highlightId)).join('');
    elements.messagesList.innerHTML = html;
    hydrateDirectMessageAttachments(messages);

    // Apply syntax highlighting
    applySyntaxHighlighting();
}

/**
 * Create a message element (for virtual list)
 * @private
 */
function createMessageElement(message, index, isHighlighted = false) {
    const roleClass = message.role === 'user' ? 'user' : 'assistant';
    const highlightClass = isHighlighted ? 'highlighted' : '';
    const time = message.created_at ? formatTime(message.created_at) : '';

    // Render markdown content
    const renderedContent = renderMarkdown(message.content);

    const article = document.createElement('article');
    article.className = `message ${roleClass} ${highlightClass}`;
    article.id = `message-${message.id}`;
    article.dataset.messageId = message.id;

    article.innerHTML = `
        <header class="message-header">
            <span class="message-role ${roleClass}">
                ${roleClass === 'user' ? '👤 User' : '🤖 Assistant'}
            </span>
            ${message.model ? `<span class="message-model">${escapeHtml(message.model)}</span>` : ''}
            <span class="message-time">${escapeHtml(time)}</span>
        </header>
        <div class="message-content">
            ${renderedContent}
        </div>
    `;

    appendAttachmentsToMessage(article, message);

    // Apply syntax highlighting after element is created
    requestAnimationFrame(() => {
        highlightCodeInElement(article);
    });

    return article;
}

/**
 * Apply syntax highlighting to code blocks in a specific element
 * @private
 */
function highlightCodeInElement(element) {
    if (typeof window.Prism !== 'undefined') {
        const codeBlocks = element.querySelectorAll('pre code[data-language]');
        codeBlocks.forEach(block => {
            const lang = block.dataset.language;
            if (window.Prism.languages[lang]) {
                block.innerHTML = window.Prism.highlight(
                    block.textContent,
                    window.Prism.languages[lang],
                    lang
                );
                block.parentElement.classList.add(`language-${lang}`);
            }
        });
    }
}

/**
 * Destroy virtual list if it exists
 * @private
 */
function destroyVirtualList() {
    if (messageVirtualList) {
        messageVirtualList.destroy();
        messageVirtualList = null;
    }
}

/**
 * Render a single message
 */
function renderMessage(message, index, isHighlighted = false) {
    const roleClass = message.role === 'user' ? 'user' : 'assistant';
    const highlightClass = isHighlighted ? 'highlighted' : '';
    const time = message.created_at ? formatTime(message.created_at) : '';

    // Render markdown content
    const renderedContent = renderMarkdown(message.content);

    return `
        <article
            class="message ${roleClass} ${highlightClass}"
            id="message-${message.id}"
            data-message-id="${message.id}"
        >
            <header class="message-header">
                <span class="message-role ${roleClass}">
                    ${roleClass === 'user' ? '👤 User' : '🤖 Assistant'}
                </span>
                ${message.model ? `<span class="message-model">${escapeHtml(message.model)}</span>` : ''}
                <span class="message-time">${escapeHtml(time)}</span>
            </header>
            <div class="message-content">
                ${renderedContent}
            </div>
        </article>
    `;
}

function hydrateDirectMessageAttachments(messages) {
    if (!attachmentState.available) {
        return;
    }

    const byId = new Map(messages.map(message => [String(message.id), message]));
    const renderedMessages = elements.messagesList.querySelectorAll('.message[data-message-id]');

    renderedMessages.forEach(messageElement => {
        const message = byId.get(messageElement.dataset.messageId);
        if (message) {
            appendAttachmentsToMessage(messageElement, message);
        }
    });
}

function appendAttachmentsToMessage(messageElement, message) {
    if (!attachmentState.available) {
        return;
    }

    const attachments = getMessageAttachments(message.id);
    if (!attachments.length) {
        return;
    }

    const contentElement = messageElement.querySelector('.message-content');
    if (!contentElement || contentElement.querySelector('.message-attachments')) {
        return;
    }

    const attachmentsContainer = document.createElement('div');
    attachmentsContainer.className = 'message-attachments';

    const label = document.createElement('div');
    label.className = 'message-attachments-label';
    label.textContent = attachments.length === 1 ? 'Attachment' : 'Attachments';
    attachmentsContainer.appendChild(label);

    attachments.forEach(entry => {
        attachmentsContainer.appendChild(
            createAttachmentElement(entry, attachmentState.dek, attachmentState.exportId)
        );
    });

    contentElement.appendChild(attachmentsContainer);
}

function handleArchiveLock() {
    activeConversationLoadId += 1;
    currentConversation = null;
    currentMessages = [];
    destroyVirtualList();
    clearAllCache();
    attachmentState = createAttachmentState();
    resetAttachments();
}

/**
 * Set up event listeners
 */
function setupEventListeners() {
    // Back button
    const backBtn = document.getElementById('back-btn');
    backBtn?.addEventListener('click', () => {
        if (onBack) {
            onBack();
        }
    });

    // Copy button
    const copyBtn = document.getElementById('copy-btn');
    copyBtn?.addEventListener('click', () => {
        copyConversation();
    });

    // Escape key to go back
    const handleKeydown = (e) => {
        if (e.key === 'Escape' && onBack) {
            onBack();
            document.removeEventListener('keydown', handleKeydown);
        }
    };
    document.addEventListener('keydown', handleKeydown);
}

/**
 * Render markdown content (simple implementation)
 * Falls back to plain text if marked.js is not available
 */
function renderMarkdown(content) {
    if (!content) return '';

    // Check if marked is available
    if (typeof window.marked !== 'undefined') {
        try {
            const html = window.marked.parse(content);
            return sanitizeHtml(html);
        } catch (error) {
            console.warn('[Conversation] Markdown rendering failed:', error);
        }
    }

    // Fallback: simple markdown-like rendering
    return sanitizeHtml(simpleMarkdown(content));
}

function base64ToBytes(base64) {
    const binary = atob(base64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
}

/**
 * Simple markdown-like rendering (fallback)
 */
function simpleMarkdown(text) {
    // Escape HTML first
    let html = escapeHtml(text);

    // Code blocks
    html = html.replace(/```(\w*)\n?([\s\S]*?)```/g, (_, lang, code) => {
        const langClass = lang ? ` data-language="${lang}"` : '';
        return `<pre><code${langClass}>${code.trim()}</code></pre>`;
    });

    // Inline code
    html = html.replace(/`([^`]+)`/g, '<code>$1</code>');

    // Bold
    html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
    html = html.replace(/__([^_]+)__/g, '<strong>$1</strong>');

    // Italic
    html = html.replace(/\*([^*]+)\*/g, '<em>$1</em>');
    html = html.replace(/_([^_]+)_/g, '<em>$1</em>');

    // Headers
    html = html.replace(/^### (.+)$/gm, '<h3>$1</h3>');
    html = html.replace(/^## (.+)$/gm, '<h2>$1</h2>');
    html = html.replace(/^# (.+)$/gm, '<h1>$1</h1>');

    // Links
    html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g,
        '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');

    // Line breaks
    html = html.replace(/\n\n/g, '</p><p>');
    html = `<p>${html}</p>`;

    // Clean up empty paragraphs
    html = html.replace(/<p>\s*<\/p>/g, '');
    html = html.replace(/<p>(<h[1-6]>)/g, '$1');
    html = html.replace(/(<\/h[1-6]>)<\/p>/g, '$1');
    html = html.replace(/<p>(<pre>)/g, '$1');
    html = html.replace(/(<\/pre>)<\/p>/g, '$1');

    return html;
}

/**
 * Sanitize HTML to prevent XSS
 */
function sanitizeHtml(html) {
    // Check if DOMPurify is available
    if (typeof window.DOMPurify !== 'undefined') {
        return window.DOMPurify.sanitize(html, SANITIZE_CONFIG);
    }

    // Fallback: create a document fragment and extract text/safe elements
    const template = document.createElement('template');
    template.innerHTML = html;

    // Remove script tags and event handlers
    const scripts = template.content.querySelectorAll('script, style, iframe, object, embed');
    scripts.forEach(el => el.remove());

    // Remove event handlers
    const allElements = template.content.querySelectorAll('*');
    allElements.forEach(el => {
        Array.from(el.attributes).forEach(attr => {
            if (attr.name.startsWith('on') || attr.name === 'style') {
                el.removeAttribute(attr.name);
            }
        });
    });

    return template.innerHTML;
}

/**
 * Apply syntax highlighting to code blocks
 */
function applySyntaxHighlighting() {
    // Check if Prism is available
    if (typeof window.Prism !== 'undefined') {
        const codeBlocks = elements.container.querySelectorAll('pre code[data-language]');
        codeBlocks.forEach(block => {
            const lang = block.dataset.language;
            if (window.Prism.languages[lang]) {
                block.innerHTML = window.Prism.highlight(
                    block.textContent,
                    window.Prism.languages[lang],
                    lang
                );
                block.parentElement.classList.add(`language-${lang}`);
            }
        });
    }
}

/**
 * Scroll to a specific message
 */
function scrollToMessage(messageId) {
    setTimeout(() => {
        const messageEl = document.getElementById(`message-${messageId}`);
        if (messageEl) {
            messageEl.scrollIntoView({ behavior: 'smooth', block: 'center' });
            messageEl.classList.add('highlight-flash');
            setTimeout(() => {
                messageEl.classList.remove('highlight-flash');
            }, 2000);
        }
    }, 100);
}

/**
 * Copy conversation to clipboard
 */
async function copyConversation() {
    if (!currentConversation || !currentMessages.length) return;

    const text = formatConversationAsText(currentConversation, currentMessages);

    try {
        await navigator.clipboard.writeText(text);
        showCopyFeedback('Copied!');
    } catch (error) {
        console.error('[Conversation] Copy failed:', error);
        showCopyFeedback('Copy failed');
    }
}

/**
 * Format conversation as plain text
 */
function formatConversationAsText(conv, messages) {
    const lines = [
        `# ${conv.title || 'Untitled conversation'}`,
        `Agent: ${conv.agent}`,
        `Date: ${formatDate(conv.started_at)}`,
        conv.workspace ? `Workspace: ${conv.workspace}` : '',
        '',
        '---',
        '',
    ];

    messages.forEach(msg => {
        const role = msg.role === 'user' ? 'User' : 'Assistant';
        lines.push(`## ${role}:`);
        lines.push('');
        lines.push(msg.content);
        lines.push('');
        lines.push('---');
        lines.push('');
    });

    return lines.filter(line => line !== null).join('\n');
}

/**
 * Show copy feedback
 */
function showCopyFeedback(message) {
    const copyBtn = document.getElementById('copy-btn');
    if (copyBtn) {
        const originalText = copyBtn.innerHTML;
        copyBtn.innerHTML = message;
        setTimeout(() => {
            copyBtn.innerHTML = originalText;
        }, 2000);
    }
}

/**
 * Show error message
 */
function showError(message) {
    elements.container.innerHTML = `
        <div class="conversation-container">
            <div class="conversation-error">
                <span class="error-icon">⚠️</span>
                <p>${escapeHtml(message)}</p>
                <button type="button" class="btn" id="error-back-btn">Go back</button>
            </div>
        </div>
    `;

    // Add CSP-safe event listener (no inline onclick)
    const backBtn = document.getElementById('error-back-btn');
    backBtn?.addEventListener('click', () => {
        if (onBack) {
            onBack();
        } else {
            history.back();
        }
    });
}

/**
 * Format agent name for display
 */
function formatAgentName(agent) {
    if (!agent) return 'Unknown';
    return agent.charAt(0).toUpperCase() + agent.slice(1);
}

/**
 * Format timestamp as date string
 */
function formatDate(timestamp) {
    if (!timestamp) return '';

    const date = new Date(timestamp);
    return date.toLocaleDateString(undefined, {
        weekday: 'short',
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
    });
}

/**
 * Format timestamp as time string
 */
function formatTime(timestamp) {
    if (!timestamp) return '';

    const date = new Date(timestamp);
    return date.toLocaleTimeString(undefined, {
        hour: '2-digit',
        minute: '2-digit',
    });
}

/**
 * Format duration in human-readable format
 */
function formatDuration(ms) {
    if (!ms || ms < 0) return '';

    const seconds = Math.floor(ms / 1000);
    const minutes = Math.floor(seconds / 60);
    const hours = Math.floor(minutes / 60);

    if (hours > 0) {
        return `${hours}h ${minutes % 60}m`;
    }
    if (minutes > 0) {
        return `${minutes}m`;
    }
    return `${seconds}s`;
}

/**
 * Escape HTML special characters
 */
function escapeHtml(text) {
    if (!text) return '';
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

/**
 * Get current conversation ID
 */
export function getCurrentConversationId() {
    return currentConversation?.id || null;
}

/**
 * Get current conversation data
 */
export function getCurrentConversation() {
    return currentConversation;
}

/**
 * Unload the oldest (least recently used) conversation from cache
 * @private
 */
function unloadOldestConversation() {
    const oldest = loadedConversations.keys().next().value;
    if (oldest !== undefined) {
        loadedConversations.delete(oldest);
        console.debug(`[Conversation] Unloaded oldest conversation ${oldest} (cache size: ${loadedConversations.size})`);
    }
}

/**
 * Clear old conversations from cache to free memory
 * @param {number} [keepCount=1] - Number of recent conversations to keep
 */
export function clearOldConversations(keepCount = 1) {
    const entries = Array.from(loadedConversations.entries());
    const toRemove = entries.length - keepCount;

    if (toRemove > 0) {
        // Remove oldest entries (first ones in the Map)
        for (let i = 0; i < toRemove; i++) {
            loadedConversations.delete(entries[i][0]);
        }
        console.debug(`[Conversation] Cleared ${toRemove} old conversations (cache size: ${loadedConversations.size})`);
    }
}

/**
 * Show memory warning banner
 */
function showMemoryWarning() {
    // Check if warning already exists
    if (document.getElementById('memory-warning')) return;

    const usage = getMemoryUsage();
    const percent = usage ? usage.percent.toFixed(1) : 'N/A';

    const banner = document.createElement('div');
    banner.id = 'memory-warning';
    banner.className = 'memory-warning-banner';
    banner.setAttribute('role', 'alert');
    banner.innerHTML = `
        <span class="memory-warning-icon" aria-hidden="true">&#x26A0;&#xFE0F;</span>
        <span class="memory-warning-text">Memory usage is high (${percent}%). Consider closing some conversations.</span>
        <button id="memory-clear-btn" type="button" class="btn btn-small memory-clear-btn">
            Clear Cache
        </button>
        <button class="memory-dismiss-btn" type="button" aria-label="Dismiss">&#x2715;</button>
    `;

    // Add to page
    document.body.prepend(banner);

    // Event listeners
    const clearBtn = document.getElementById('memory-clear-btn');
    clearBtn?.addEventListener('click', () => {
        clearOldConversations(1);
        hideMemoryWarning();
    });

    const dismissBtn = banner.querySelector('.memory-dismiss-btn');
    dismissBtn?.addEventListener('click', hideMemoryWarning);
}

/**
 * Hide memory warning banner
 */
function hideMemoryWarning() {
    const banner = document.getElementById('memory-warning');
    if (banner) {
        banner.remove();
    }
}

/**
 * Start periodic memory monitoring
 */
export function startMemoryMonitoring() {
    if (memoryCheckIntervalId) return; // Already running

    memoryCheckIntervalId = setInterval(() => {
        if (checkMemoryPressure()) {
            showMemoryWarning();
        }
    }, MEMORY_CONFIG.MEMORY_CHECK_INTERVAL);

    console.debug('[Conversation] Memory monitoring started');
}

/**
 * Stop periodic memory monitoring
 */
export function stopMemoryMonitoring() {
    if (memoryCheckIntervalId) {
        clearInterval(memoryCheckIntervalId);
        memoryCheckIntervalId = null;
        console.debug('[Conversation] Memory monitoring stopped');
    }
}

/**
 * Get conversation cache statistics
 * @returns {Object} Cache stats
 */
export function getCacheStats() {
    const memory = getMemoryUsage();
    return {
        cachedCount: loadedConversations.size,
        maxCached: MEMORY_CONFIG.MAX_LOADED_CONVERSATIONS,
        memoryUsed: memory?.used || 0,
        memoryLimit: memory?.limit || 0,
        memoryPercent: memory?.percent || 0,
    };
}

/**
 * Clear the viewer
 */
export function clearViewer() {
    activeConversationLoadId += 1;
    // Clean up virtual list
    destroyVirtualList();

    currentConversation = null;
    currentMessages = [];
    elements.container.innerHTML = '';
}

/**
 * Clear all cached conversations
 */
export function clearAllCache() {
    loadedConversations.clear();
    hideMemoryWarning();
    console.debug('[Conversation] All cached conversations cleared');
}

// Export default
export default {
    initConversationViewer,
    loadConversation,
    getCurrentConversationId,
    getCurrentConversation,
    clearViewer,
    clearAllCache,
    clearOldConversations,
    getCacheStats,
    startMemoryMonitoring,
    stopMemoryMonitoring,
};
