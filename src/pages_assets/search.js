/**
 * cass Archive Search UI Component
 *
 * Provides search interface with query input, filters, and result rendering.
 * Uses FTS5 for full-text search with intelligent query routing.
 */

import {
    searchConversations,
    detectSearchMode,
    getStatistics,
    getRecentConversations,
    getConversationsByAgent,
    getConversationsByTimeRange,
} from './database.js';
import { VirtualList } from './virtual-list.js';

// Search configuration
const SEARCH_CONFIG = {
    DEBOUNCE_MS: 300,
    PAGE_SIZE: 50,
    SNIPPET_LENGTH: 64,
    MAX_RESULTS: 1000,
    // Virtual list configuration
    RESULT_CARD_HEIGHT: 88, // Fixed height per result card
    VIRTUAL_LIST_OVERSCAN: 5, // Extra items to render above/below viewport
    VIRTUAL_LIST_THRESHOLD: 20, // Use virtual list above this count
};

// Module state
let currentQuery = '';
let currentFilters = {
    agent: null,
    since: null,
    until: null,
};
let currentSearchMode = 'auto'; // 'auto', 'prose', or 'code'
let currentResults = [];
let currentPage = 0;
let searchTimeout = null;
let onResultSelect = null;
let virtualList = null; // Virtual list instance for large result sets

// DOM element references
let elements = {
    container: null,
    searchInput: null,
    searchModeToggle: null,
    searchModeIndicator: null,
    agentFilter: null,
    timeFilter: null,
    resultsContainer: null,
    resultsList: null,
    loadingIndicator: null,
    resultCount: null,
    noResults: null,
};

/**
 * Initialize the search UI
 * @param {HTMLElement} container - Container element
 * @param {Function} onSelect - Callback when result is selected
 */
export function initSearch(container, onSelect) {
    elements.container = container;
    onResultSelect = onSelect;

    // Render search UI
    renderSearchUI();

    // Cache element references
    cacheElements();

    // Set up event listeners
    setupEventListeners();

    // Load initial data (recent conversations)
    loadRecentConversations();

    // Populate filter options
    populateFilters();
}

/**
 * Render the search UI structure
 */
function renderSearchUI() {
    elements.container.innerHTML = `
        <div class="search-container">
            <div class="search-box">
                <input
                    type="search"
                    id="search-input"
                    class="search-input"
                    placeholder="Search conversations..."
                    autocomplete="off"
                >
                <button type="button" id="search-btn" class="btn btn-primary search-btn">
                    Search
                </button>
            </div>

            <div class="search-filters">
                <div class="filter-group search-mode-group">
                    <label>Mode</label>
                    <div id="search-mode-toggle" class="search-mode-toggle">
                        <button type="button" class="search-mode-btn active" data-mode="auto" title="Auto-detect based on query">Auto</button>
                        <button type="button" class="search-mode-btn" data-mode="prose" title="Natural language search with stemming">Prose</button>
                        <button type="button" class="search-mode-btn" data-mode="code" title="Code search for identifiers and paths">Code</button>
                    </div>
                </div>

                <div class="filter-group">
                    <label for="agent-filter">Agent</label>
                    <select id="agent-filter" class="filter-select">
                        <option value="">All agents</option>
                    </select>
                </div>

                <div class="filter-group">
                    <label for="time-filter">Time</label>
                    <select id="time-filter" class="filter-select">
                        <option value="">All time</option>
                        <option value="today">Today</option>
                        <option value="week">Past week</option>
                        <option value="month">Past month</option>
                        <option value="year">Past year</option>
                    </select>
                </div>
            </div>

            <div class="search-results">
                <div class="search-results-header">
                    <div id="result-count" class="result-count"></div>
                    <div id="search-mode-indicator" class="search-mode-indicator hidden"></div>
                </div>
                <div id="loading-indicator" class="loading-indicator hidden">
                    <div class="spinner-small"></div>
                    <span>Searching...</span>
                </div>
                <div id="no-results" class="no-results hidden">
                    <span class="no-results-icon">🔍</span>
                    <p>No results found</p>
                    <p class="no-results-hint">Try different keywords or adjust filters</p>
                </div>
                <div id="results-list" class="results-list"></div>
            </div>
        </div>
    `;
}

/**
 * Cache DOM element references
 */
function cacheElements() {
    elements.searchInput = document.getElementById('search-input');
    elements.agentFilter = document.getElementById('agent-filter');
    elements.timeFilter = document.getElementById('time-filter');
    elements.resultsContainer = elements.container.querySelector('.search-results');
    elements.resultsList = document.getElementById('results-list');
    elements.loadingIndicator = document.getElementById('loading-indicator');
    elements.resultCount = document.getElementById('result-count');
    elements.noResults = document.getElementById('no-results');
}

/**
 * Set up event listeners
 */
function setupEventListeners() {
    // Search input with debounce
    elements.searchInput.addEventListener('input', (e) => {
        clearTimeout(searchTimeout);
        searchTimeout = setTimeout(() => {
            handleSearch(e.target.value);
        }, SEARCH_CONFIG.DEBOUNCE_MS);
    });

    // Enter key in search
    elements.searchInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') {
            clearTimeout(searchTimeout);
            handleSearch(e.target.value);
        }
    });

    // Search button
    const searchBtn = document.getElementById('search-btn');
    searchBtn?.addEventListener('click', () => {
        handleSearch(elements.searchInput.value);
    });

    // Agent filter
    elements.agentFilter.addEventListener('change', (e) => {
        currentFilters.agent = e.target.value || null;
        handleSearch(currentQuery);
    });

    // Time filter
    elements.timeFilter.addEventListener('change', (e) => {
        updateTimeFilter(e.target.value);
        handleSearch(currentQuery);
    });

    // Result click delegation
    elements.resultsList.addEventListener('click', (e) => {
        const resultCard = e.target.closest('.result-card');
        if (resultCard) {
            const convId = parseInt(resultCard.dataset.conversationId, 10);
            const msgId = parseInt(resultCard.dataset.messageId, 10) || null;
            if (onResultSelect) {
                onResultSelect(convId, msgId);
            }
        }
    });

    // Keyboard navigation
    elements.resultsList.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
            const focused = document.activeElement;
            if (focused?.classList.contains('result-card')) {
                focused.click();
            }
        }
    });
}

/**
 * Populate filter dropdowns from database
 */
async function populateFilters() {
    try {
        const stats = getStatistics();

        // Populate agent filter
        if (stats.agents && stats.agents.length > 0) {
            stats.agents.forEach(agent => {
                const option = document.createElement('option');
                option.value = agent;
                option.textContent = formatAgentName(agent);
                elements.agentFilter.appendChild(option);
            });
        }
    } catch (error) {
        console.error('[Search] Failed to populate filters:', error);
    }
}

/**
 * Update time filter values
 */
function updateTimeFilter(value) {
    const now = Date.now();
    const day = 24 * 60 * 60 * 1000;

    switch (value) {
        case 'today':
            currentFilters.since = now - day;
            currentFilters.until = now;
            break;
        case 'week':
            currentFilters.since = now - (7 * day);
            currentFilters.until = now;
            break;
        case 'month':
            currentFilters.since = now - (30 * day);
            currentFilters.until = now;
            break;
        case 'year':
            currentFilters.since = now - (365 * day);
            currentFilters.until = now;
            break;
        default:
            currentFilters.since = null;
            currentFilters.until = null;
    }
}

/**
 * Handle search query
 */
async function handleSearch(query) {
    currentQuery = query.trim();
    currentPage = 0;

    showLoading();

    try {
        if (!currentQuery) {
            // Empty query - show recent conversations
            await loadRecentConversations();
        } else {
            // FTS5 search
            await performSearch();
        }
    } catch (error) {
        console.error('[Search] Search error:', error);
        showError('Search failed. Please try again.');
    }

    hideLoading();
}

/**
 * Perform FTS5 search
 */
async function performSearch() {
    const options = {
        limit: SEARCH_CONFIG.PAGE_SIZE,
        offset: currentPage * SEARCH_CONFIG.PAGE_SIZE,
        agent: currentFilters.agent,
    };

    // Pass raw query - searchConversations handles escaping and FTS table routing
    currentResults = searchConversations(currentQuery, options);

    // Apply time filter post-query if needed
    if (currentFilters.since || currentFilters.until) {
        currentResults = currentResults.filter(r => {
            const ts = r.started_at;
            if (currentFilters.since && ts < currentFilters.since) return false;
            if (currentFilters.until && ts > currentFilters.until) return false;
            return true;
        });
    }

    renderResults();
}

/**
 * Load recent conversations (no search query)
 */
async function loadRecentConversations() {
    try {
        let results;

        if (currentFilters.agent) {
            results = getConversationsByAgent(currentFilters.agent, SEARCH_CONFIG.PAGE_SIZE);
        } else if (currentFilters.since || currentFilters.until) {
            const since = currentFilters.since || 0;
            const until = currentFilters.until || Date.now();
            results = getConversationsByTimeRange(since, until, SEARCH_CONFIG.PAGE_SIZE);
        } else {
            results = getRecentConversations(SEARCH_CONFIG.PAGE_SIZE);
        }

        // Transform to match search result format
        currentResults = results.map(conv => ({
            conversation_id: conv.id,
            message_id: null,
            agent: conv.agent,
            workspace: conv.workspace,
            title: conv.title || 'Untitled conversation',
            started_at: conv.started_at,
            snippet: null,
            rank: 0,
        }));

        renderResults();
    } catch (error) {
        console.error('[Search] Failed to load recent:', error);
        showError('Failed to load conversations');
    }
}

// Note: FTS5 query formatting and escaping is now handled in database.js
// searchConversations() automatically routes to messages_fts (natural language)
// or messages_code_fts (code identifiers) based on query content

/**
 * Render search results
 * Uses virtual scrolling for large result sets (> VIRTUAL_LIST_THRESHOLD)
 */
function renderResults() {
    if (currentResults.length === 0) {
        showNoResults();
        destroyVirtualList();
        return;
    }

    hideNoResults();
    updateResultCount();

    // Use virtual scrolling for large result sets
    if (currentResults.length > SEARCH_CONFIG.VIRTUAL_LIST_THRESHOLD) {
        renderVirtualResults();
    } else {
        renderDirectResults();
    }
}

/**
 * Render results using virtual scrolling
 * @private
 */
function renderVirtualResults() {
    // Destroy previous virtual list if exists
    destroyVirtualList();

    // Clear container and set up for virtual scrolling
    elements.resultsList.innerHTML = '';
    elements.resultsList.style.height = '100%';
    elements.resultsList.style.minHeight = '400px';
    elements.resultsList.style.maxHeight = 'calc(100vh - 300px)';

    // Create virtual list
    virtualList = new VirtualList({
        container: elements.resultsList,
        itemHeight: SEARCH_CONFIG.RESULT_CARD_HEIGHT,
        totalCount: currentResults.length,
        renderItem: (index) => createResultCard(currentResults[index], index),
        overscan: SEARCH_CONFIG.VIRTUAL_LIST_OVERSCAN,
    });

    console.debug(`[Search] Using virtual scrolling for ${currentResults.length} results`);
}

/**
 * Render results directly (for small result sets)
 * @private
 */
function renderDirectResults() {
    destroyVirtualList();

    // Reset container styling
    elements.resultsList.style.height = '';
    elements.resultsList.style.minHeight = '';
    elements.resultsList.style.maxHeight = '';

    const html = currentResults.map((result, index) => createResultCardHtml(result)).join('');
    elements.resultsList.innerHTML = html;
}

/**
 * Create a result card element (for virtual list)
 * @private
 */
function createResultCard(result, index) {
    const article = document.createElement('article');
    article.className = 'result-card';
    article.dataset.conversationId = result.conversation_id;
    article.dataset.messageId = result.message_id || '';
    article.tabIndex = 0;
    article.setAttribute('role', 'button');
    article.setAttribute('aria-label', `Open conversation: ${result.title || 'Untitled'}`);

    article.innerHTML = `
        <div class="result-header">
            <span class="result-title">${escapeHtml(result.title || 'Untitled conversation')}</span>
            <span class="result-agent">${escapeHtml(formatAgentName(result.agent))}</span>
        </div>
        ${result.snippet ? `
            <div class="result-snippet">${result.snippet}</div>
        ` : ''}
        <div class="result-meta">
            ${result.workspace ? `<span class="result-workspace">${escapeHtml(formatWorkspace(result.workspace))}</span>` : ''}
            <span class="result-time">${formatTime(result.started_at)}</span>
        </div>
    `;

    // Add click handler for virtual list items
    article.addEventListener('click', () => {
        const convId = parseInt(article.dataset.conversationId, 10);
        const msgId = parseInt(article.dataset.messageId, 10) || null;
        if (onResultSelect) {
            onResultSelect(convId, msgId);
        }
    });

    return article;
}

/**
 * Create result card HTML string (for direct rendering)
 * @private
 */
function createResultCardHtml(result) {
    return `
        <article
            class="result-card"
            data-conversation-id="${result.conversation_id}"
            data-message-id="${result.message_id || ''}"
            tabindex="0"
            role="button"
            aria-label="Open conversation: ${escapeHtml(result.title || 'Untitled')}"
        >
            <div class="result-header">
                <span class="result-title">${escapeHtml(result.title || 'Untitled conversation')}</span>
                <span class="result-agent">${escapeHtml(formatAgentName(result.agent))}</span>
            </div>
            ${result.snippet ? `
                <div class="result-snippet">${result.snippet}</div>
            ` : ''}
            <div class="result-meta">
                ${result.workspace ? `<span class="result-workspace">${escapeHtml(formatWorkspace(result.workspace))}</span>` : ''}
                <span class="result-time">${formatTime(result.started_at)}</span>
            </div>
        </article>
    `;
}

/**
 * Destroy virtual list if it exists
 * @private
 */
function destroyVirtualList() {
    if (virtualList) {
        virtualList.destroy();
        virtualList = null;
    }
}

/**
 * Update result count display
 */
function updateResultCount() {
    const count = currentResults.length;
    const hasMore = count >= SEARCH_CONFIG.PAGE_SIZE;

    if (currentQuery) {
        elements.resultCount.textContent = hasMore
            ? `${count}+ results for "${currentQuery}"`
            : `${count} result${count !== 1 ? 's' : ''} for "${currentQuery}"`;
    } else {
        elements.resultCount.textContent = `${count} recent conversation${count !== 1 ? 's' : ''}`;
    }
}

/**
 * Show loading indicator
 */
function showLoading() {
    elements.loadingIndicator.classList.remove('hidden');
    elements.resultsList.classList.add('loading');
}

/**
 * Hide loading indicator
 */
function hideLoading() {
    elements.loadingIndicator.classList.add('hidden');
    elements.resultsList.classList.remove('loading');
}

/**
 * Show no results message
 */
function showNoResults() {
    elements.noResults.classList.remove('hidden');
    elements.resultsList.innerHTML = '';
    elements.resultCount.textContent = '';
}

/**
 * Hide no results message
 */
function hideNoResults() {
    elements.noResults.classList.add('hidden');
}

/**
 * Show error message
 */
function showError(message) {
    elements.resultsList.innerHTML = `
        <div class="search-error">
            <span class="error-icon">⚠️</span>
            <p>${escapeHtml(message)}</p>
        </div>
    `;
    elements.resultCount.textContent = '';
}

/**
 * Format agent name for display
 */
function formatAgentName(agent) {
    if (!agent) return 'Unknown';

    // Capitalize first letter
    return agent.charAt(0).toUpperCase() + agent.slice(1);
}

/**
 * Format workspace path for display
 */
function formatWorkspace(workspace) {
    if (!workspace) return '';

    // Show last 2 path components
    const parts = workspace.split('/').filter(Boolean);
    if (parts.length <= 2) return workspace;

    return '.../' + parts.slice(-2).join('/');
}

/**
 * Format timestamp for display
 */
function formatTime(timestamp) {
    if (!timestamp) return '';

    const date = new Date(timestamp);
    const now = new Date();
    const diff = now - date;

    const minute = 60 * 1000;
    const hour = 60 * minute;
    const day = 24 * hour;
    const week = 7 * day;

    if (diff < hour) {
        const mins = Math.floor(diff / minute);
        return mins <= 1 ? 'Just now' : `${mins}m ago`;
    }
    if (diff < day) {
        const hours = Math.floor(diff / hour);
        return `${hours}h ago`;
    }
    if (diff < week) {
        const days = Math.floor(diff / day);
        return days === 1 ? 'Yesterday' : `${days}d ago`;
    }

    // Format as date
    return date.toLocaleDateString(undefined, {
        month: 'short',
        day: 'numeric',
        year: date.getFullYear() !== now.getFullYear() ? 'numeric' : undefined,
    });
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
 * Clear search and reset to initial state
 */
export function clearSearch() {
    currentQuery = '';
    currentFilters = { agent: null, since: null, until: null };
    currentResults = [];
    currentPage = 0;

    // Clean up virtual list if it exists
    destroyVirtualList();

    if (elements.searchInput) {
        elements.searchInput.value = '';
    }
    if (elements.agentFilter) {
        elements.agentFilter.value = '';
    }
    if (elements.timeFilter) {
        elements.timeFilter.value = '';
    }

    loadRecentConversations();
}

/**
 * Get current search state
 */
export function getSearchState() {
    return {
        query: currentQuery,
        filters: { ...currentFilters },
        resultCount: currentResults.length,
    };
}

// Export default
export default {
    initSearch,
    clearSearch,
    getSearchState,
};
