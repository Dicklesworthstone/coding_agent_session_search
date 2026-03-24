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
import { parseRouteIdSegment } from './router.js';
import { VirtualList } from './virtual-list.js';

// Search configuration
const SEARCH_CONFIG = {
    DEBOUNCE_MS: 300,
    PAGE_SIZE: 50,
    SNIPPET_LENGTH: 64,
    MAX_RESULTS: 1000,
    TIME_FILTER_CUSTOM_VALUE: 'custom',
    // Virtual list configuration
    RESULT_CARD_HEIGHT: 88, // Fixed height per result card
    VIRTUAL_LIST_OVERSCAN: 5, // Extra items to render above/below viewport
    VIRTUAL_LIST_THRESHOLD: 20, // Use virtual list above this count
};

function createEmptySearchFilters() {
    return {
        agent: null,
        since: null,
        until: null,
        timePreset: null,
    };
}

// Module state
let currentQuery = '';
let currentFilters = createEmptySearchFilters();
let currentSearchMode = 'auto'; // 'auto', 'prose', or 'code'
let currentResults = [];
let currentPage = 0;
let searchTimeout = null;
let onResultSelect = null;
let virtualList = null; // Virtual list instance for large result sets
let searchEpoch = 0;

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

function parseResultSelection(card) {
    const conversationId = parseRouteIdSegment(card?.dataset?.conversationId || '');
    if (conversationId === null) {
        return null;
    }

    const rawMessageId = card?.dataset?.messageId || '';
    if (!rawMessageId) {
        return { conversationId, messageId: null };
    }

    const messageId = parseRouteIdSegment(rawMessageId);
    if (messageId === null) {
        return null;
    }

    return { conversationId, messageId };
}

function parseResultIndex(card) {
    const rawIndex = card?.dataset?.resultIndex ?? '';
    if (!/^\d+$/.test(rawIndex)) {
        return null;
    }

    const index = Number.parseInt(rawIndex, 10);
    if (!Number.isSafeInteger(index) || index < 0 || index >= currentResults.length) {
        return null;
    }

    return index;
}

function findRenderedResultCard(index) {
    if (!elements.resultsList || !Number.isSafeInteger(index) || index < 0) {
        return null;
    }

    return elements.resultsList.querySelector(`.result-card[data-result-index="${index}"]`);
}

function focusResultCardAtIndex(index, align = 'start') {
    if (!Number.isSafeInteger(index) || index < 0 || index >= currentResults.length) {
        return false;
    }

    let card = findRenderedResultCard(index);
    if (!card && virtualList) {
        virtualList.scrollToIndex(index, align);
        card = findRenderedResultCard(index);
    }

    if (!card) {
        return false;
    }

    card.focus();
    return true;
}

function parseTimestampFilterValue(value) {
    if (value === undefined || value === null || value === '') {
        return null;
    }

    const numeric = Number(value);
    if (!Number.isFinite(numeric) || numeric < 0 || !Number.isSafeInteger(Math.trunc(numeric))) {
        return null;
    }

    return Math.trunc(numeric);
}

function calculateTimeFilterRange(value) {
    const now = Date.now();
    const day = 24 * 60 * 60 * 1000;

    switch (value) {
        case 'today':
            return { since: now - day, until: now, timePreset: value };
        case 'week':
            return { since: now - (7 * day), until: now, timePreset: value };
        case 'month':
            return { since: now - (30 * day), until: now, timePreset: value };
        case 'year':
            return { since: now - (365 * day), until: now, timePreset: value };
        default:
            return createEmptySearchFilters();
    }
}

function normalizeRouteFilters(routeSearch = {}) {
    const agent = routeSearch.agent === undefined || routeSearch.agent === null || routeSearch.agent === ''
        ? null
        : String(routeSearch.agent);
    const timePreset = typeof routeSearch.timePreset === 'string' && routeSearch.timePreset !== ''
        ? routeSearch.timePreset
        : typeof routeSearch.time === 'string' && routeSearch.time !== ''
            ? routeSearch.time
            : null;

    if (timePreset === 'today' || timePreset === 'week' || timePreset === 'month' || timePreset === 'year') {
        return {
            agent,
            ...calculateTimeFilterRange(timePreset),
        };
    }

    const since = parseTimestampFilterValue(routeSearch.since);
    const until = parseTimestampFilterValue(routeSearch.until);
    if (since !== null && until !== null && since > until) {
        return {
            ...createEmptySearchFilters(),
            agent,
        };
    }

    return {
        agent,
        since,
        until,
        timePreset: since !== null || until !== null ? SEARCH_CONFIG.TIME_FILTER_CUSTOM_VALUE : null,
    };
}

function syncAgentFilterControl() {
    if (!elements.agentFilter) {
        return;
    }

    const agent = currentFilters.agent;
    if (!agent) {
        elements.agentFilter.value = '';
        return;
    }

    const optionExists = Array.from(elements.agentFilter.options).some((option) => option.value === agent);
    if (!optionExists) {
        const option = document.createElement('option');
        option.value = agent;
        option.textContent = formatAgentName(agent);
        elements.agentFilter.appendChild(option);
    }

    elements.agentFilter.value = agent;
}

function syncTimeFilterControl() {
    if (!elements.timeFilter) {
        return;
    }

    const customValue = SEARCH_CONFIG.TIME_FILTER_CUSTOM_VALUE;
    let customOption = Array.from(elements.timeFilter.options).find((option) => option.value === customValue);

    if (currentFilters.timePreset === customValue) {
        if (!customOption) {
            customOption = document.createElement('option');
            customOption.value = customValue;
            customOption.textContent = 'Custom range';
            elements.timeFilter.appendChild(customOption);
        }
        elements.timeFilter.value = customValue;
        return;
    }

    if (customOption) {
        customOption.remove();
    }

    elements.timeFilter.value = currentFilters.timePreset || '';
}

function syncFilterControls() {
    syncAgentFilterControl();
    syncTimeFilterControl();
}

export function buildResultCardId(result, index = 0) {
    const conversationId = String(result?.conversation_id ?? 'unknown');
    const messageId = result?.message_id;
    if (messageId !== undefined && messageId !== null && messageId !== '') {
        return `result-${conversationId}-m-${messageId}`;
    }

    return `result-${conversationId}-r-${index}`;
}

function isCurrentSearchEpoch(epoch) {
    return epoch === searchEpoch;
}

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

            <div class="search-results" role="region" aria-label="Search results">
                <div class="search-results-header">
                    <div id="result-count" class="result-count" aria-live="polite" aria-atomic="true"></div>
                    <div id="search-mode-indicator" class="search-mode-indicator hidden" aria-live="polite"></div>
                </div>
                <div id="loading-indicator" class="loading-indicator hidden" role="status" aria-live="polite">
                    <div class="spinner-small" aria-hidden="true"></div>
                    <span>Searching...</span>
                </div>
                <div id="no-results" class="no-results hidden" role="status" aria-live="polite">
                    <span class="no-results-icon" aria-hidden="true">🔍</span>
                    <p>No results found</p>
                    <p class="no-results-hint">Try different keywords or adjust filters</p>
                </div>
                <!-- Screen reader announcer for search results -->
                <div id="search-announcer" class="visually-hidden" aria-live="assertive" aria-atomic="true"></div>
                <div id="results-list" class="results-list" role="listbox" aria-label="Search results list"></div>
            </div>
        </div>
    `;
}

/**
 * Cache DOM element references
 */
function cacheElements() {
    elements.searchInput = document.getElementById('search-input');
    elements.searchModeToggle = document.getElementById('search-mode-toggle');
    elements.searchModeIndicator = document.getElementById('search-mode-indicator');
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

    // Search mode toggle
    if (elements.searchModeToggle) {
        elements.searchModeToggle.addEventListener('click', (e) => {
            const btn = e.target.closest('.search-mode-btn');
            if (btn) {
                const mode = btn.dataset.mode;
                setSearchMode(mode);
                // Re-run search with new mode if there's a query
                if (currentQuery) {
                    handleSearch(currentQuery);
                }
            }
        });
    }

    // Result click delegation
    elements.resultsList.addEventListener('click', (e) => {
        const resultCard = e.target.closest('.result-card');
        if (resultCard) {
            const selection = parseResultSelection(resultCard);
            if (!selection) {
                console.warn('[Search] Ignoring result with invalid conversation/message id');
                return;
            }
            if (onResultSelect) {
                onResultSelect(selection.conversationId, selection.messageId);
            }
        }
    });

    // Keyboard navigation for results list
    elements.resultsList.addEventListener('keydown', (e) => {
        const focused = document.activeElement;
        const isResultCard = focused?.classList.contains('result-card');

        switch (e.key) {
            case 'Enter':
            case ' ':
                if (isResultCard) {
                    e.preventDefault();
                    focused.click();
                }
                break;

            case 'ArrowDown':
                e.preventDefault();
                if (isResultCard) {
                    const currentIndex = parseResultIndex(focused);
                    if (currentIndex !== null) {
                        focusResultCardAtIndex(currentIndex + 1, 'end');
                    }
                } else {
                    focusResultCardAtIndex(0, 'start');
                }
                break;

            case 'ArrowUp':
                e.preventDefault();
                if (isResultCard) {
                    const currentIndex = parseResultIndex(focused);
                    if (currentIndex === null) {
                        break;
                    }

                    if (currentIndex === 0) {
                        // Move focus back to search input
                        elements.searchInput?.focus();
                    } else {
                        focusResultCardAtIndex(currentIndex - 1, 'start');
                    }
                }
                break;

            case 'Home':
                if (isResultCard) {
                    e.preventDefault();
                    focusResultCardAtIndex(0, 'start');
                }
                break;

            case 'End':
                if (isResultCard) {
                    e.preventDefault();
                    focusResultCardAtIndex(currentResults.length - 1, 'end');
                }
                break;
        }
    });

    // Allow arrow down from search input to results
    elements.searchInput?.addEventListener('keydown', (e) => {
        if (e.key === 'ArrowDown') {
            e.preventDefault();
            focusResultCardAtIndex(0, 'start');
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
                if (Array.from(elements.agentFilter.options).some((option) => option.value === agent)) {
                    return;
                }
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
 * Set search mode and update UI
 * @param {'auto' | 'prose' | 'code'} mode - Search mode
 */
function setSearchMode(mode) {
    currentSearchMode = mode;

    // Update button states
    if (elements.searchModeToggle) {
        const buttons = elements.searchModeToggle.querySelectorAll('.search-mode-btn');
        buttons.forEach(btn => {
            btn.classList.toggle('active', btn.dataset.mode === mode);
        });
    }
}

/**
 * Update search mode indicator (shows which FTS table is being used)
 * @param {string} query - Current search query
 */
function updateSearchModeIndicator(query) {
    if (!elements.searchModeIndicator || !query) {
        if (elements.searchModeIndicator) {
            elements.searchModeIndicator.classList.add('hidden');
        }
        return;
    }

    let activeMode;
    let modeLabel;

    if (currentSearchMode === 'auto') {
        activeMode = detectSearchMode(query);
        modeLabel = activeMode === 'code'
            ? '🔍 Code search (detected)'
            : '🔍 Prose search (detected)';
    } else {
        activeMode = currentSearchMode;
        modeLabel = activeMode === 'code'
            ? '🔍 Code search'
            : '🔍 Prose search';
    }

    elements.searchModeIndicator.textContent = modeLabel;
    elements.searchModeIndicator.classList.remove('hidden');
    elements.searchModeIndicator.dataset.mode = activeMode;
}

/**
 * Update time filter values
 */
function updateTimeFilter(value) {
    const nextFilters = calculateTimeFilterRange(value);
    currentFilters.since = nextFilters.since;
    currentFilters.until = nextFilters.until;
    currentFilters.timePreset = nextFilters.timePreset;
    syncTimeFilterControl();
}

/**
 * Handle search query
 */
async function handleSearch(query) {
    const epoch = ++searchEpoch;
    currentQuery = query.trim();
    currentPage = 0;

    showLoading();

    try {
        if (!currentQuery) {
            // Empty query - show recent conversations
            await loadRecentConversations(epoch);
        } else {
            // FTS5 search
            await performSearch(epoch);
        }
    } catch (error) {
        if (!isCurrentSearchEpoch(epoch)) {
            return;
        }
        console.error('[Search] Search error:', error);
        showError('Search failed. Please try again.');
    }

    if (!isCurrentSearchEpoch(epoch)) {
        return;
    }
    hideLoading();
}

/**
 * Perform FTS5 search
 */
async function performSearch(epoch) {
    const options = {
        limit: SEARCH_CONFIG.PAGE_SIZE,
        offset: currentPage * SEARCH_CONFIG.PAGE_SIZE,
        agent: currentFilters.agent,
        searchMode: currentSearchMode,
    };

    // Pass raw query - searchConversations handles escaping and FTS table routing
    currentResults = searchConversations(currentQuery, options);

    // Update search mode indicator
    updateSearchModeIndicator(currentQuery);

    // Apply time filter post-query if needed
    if (currentFilters.since || currentFilters.until) {
        currentResults = currentResults.filter(r => {
            const ts = r.started_at;
            if (currentFilters.since && ts < currentFilters.since) return false;
            if (currentFilters.until && ts > currentFilters.until) return false;
            return true;
        });
    }

    if (!isCurrentSearchEpoch(epoch)) {
        return;
    }

    renderResults();
}

/**
 * Load recent conversations (no search query)
 */
async function loadRecentConversations(epoch = searchEpoch) {
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

        if (!isCurrentSearchEpoch(epoch)) {
            return;
        }

        renderResults();
    } catch (error) {
        if (!isCurrentSearchEpoch(epoch)) {
            return;
        }
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
        destroyVirtualResultsView();
        showNoResults();
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
    destroyVirtualResultsView();

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
    destroyVirtualResultsView();

    const html = currentResults.map((result, index) => createResultCardHtml(result, index)).join('');
    elements.resultsList.innerHTML = html;
}

/**
 * Sanitize FTS5 snippet HTML
 * Escapes all content but preserves <mark> tags
 */
function sanitizeSnippet(html) {
    if (!html) return '';

    return html
        .split(/(<\/?mark>)/g)
        .map((segment) => (segment === '<mark>' || segment === '</mark>')
            ? segment
            : escapeHtml(segment))
        .join('');
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
    article.dataset.resultIndex = String(index);
    article.tabIndex = 0;
    article.setAttribute('role', 'option');
    article.setAttribute('aria-selected', 'false');
    article.id = buildResultCardId(result, index);
    article.setAttribute('aria-label', getResultAriaLabel(result));

    article.innerHTML = `
        <div class="result-header">
            <span class="result-title">${escapeHtml(result.title || 'Untitled conversation')}</span>
            <span class="result-agent">${escapeHtml(formatAgentName(result.agent))}</span>
        </div>
        ${result.snippet ? `
            <div class="result-snippet">${sanitizeSnippet(result.snippet)}</div>
        ` : ''}
        <div class="result-meta">
            ${result.workspace ? `<span class="result-workspace">${escapeHtml(formatWorkspace(result.workspace))}</span>` : ''}
            <span class="result-time">${formatTime(result.started_at)}</span>
        </div>
    `;

    // Add click handler for virtual list items
    article.addEventListener('click', () => {
        const selection = parseResultSelection(article);
        if (!selection) {
            console.warn('[Search] Ignoring result with invalid conversation/message id');
            return;
        }
        if (onResultSelect) {
            onResultSelect(selection.conversationId, selection.messageId);
        }
    });

    return article;
}

/**
 * Create result card HTML string (for direct rendering)
 * @private
 */
function createResultCardHtml(result, index) {
    const ariaLabel = escapeHtml(getResultAriaLabel(result));
    return `
        <article
            class="result-card"
            id="${buildResultCardId(result, index)}"
            data-conversation-id="${result.conversation_id}"
            data-message-id="${result.message_id || ''}"
            data-result-index="${index}"
            tabindex="0"
            role="option"
            aria-selected="false"
            aria-label="${ariaLabel}"
        >
            <div class="result-header">
                <span class="result-title">${escapeHtml(result.title || 'Untitled conversation')}</span>
                <span class="result-agent">${escapeHtml(formatAgentName(result.agent))}</span>
            </div>
            ${result.snippet ? `
                <div class="result-snippet">${sanitizeSnippet(result.snippet)}</div>
            ` : ''}
            <div class="result-meta">
                ${result.workspace ? `<span class="result-workspace">${escapeHtml(formatWorkspace(result.workspace))}</span>` : ''}
                <span class="result-time">${formatTime(result.started_at)}</span>
            </div>
        </article>
    `;
}

function getResultAriaLabel(result) {
    const title = result.title || 'Untitled conversation';
    const workspaceLabel = result.workspace ? `, ${formatWorkspace(result.workspace)}` : '';
    return `${title}, ${formatAgentName(result.agent)}${workspaceLabel}, ${formatTime(result.started_at)}`;
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

function resetResultsListLayout() {
    if (!elements.resultsList) {
        return;
    }

    elements.resultsList.style.height = '';
    elements.resultsList.style.minHeight = '';
    elements.resultsList.style.maxHeight = '';
}

function destroyVirtualResultsView() {
    destroyVirtualList();
    resetResultsListLayout();
}

/**
 * Update result count display and announce to screen readers
 */
function updateResultCount() {
    const count = currentResults.length;
    const hasMore = count >= SEARCH_CONFIG.PAGE_SIZE;

    let message;
    if (currentQuery) {
        message = hasMore
            ? `${count}+ results for "${currentQuery}"`
            : `${count} result${count !== 1 ? 's' : ''} for "${currentQuery}"`;
    } else {
        message = `${count} recent conversation${count !== 1 ? 's' : ''}`;
    }

    elements.resultCount.textContent = message;

    // Announce to screen readers
    announceToScreenReader(message, searchEpoch);
}

/**
 * Announce message to screen readers via the live region
 * @param {string} message - Message to announce
 */
function announceToScreenReader(message, epoch = searchEpoch) {
    const announcer = document.getElementById('search-announcer');
    if (announcer) {
        // Clear and set to trigger announcement
        announcer.textContent = '';
        // Use setTimeout to ensure the clear is processed first
        setTimeout(() => {
            if (!isCurrentSearchEpoch(epoch)) {
                return;
            }
            announcer.textContent = message;
        }, 50);
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
    destroyVirtualResultsView();
    hideNoResults();
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
    if (agent === undefined || agent === null || agent === '') return 'Unknown';
    const value = String(agent);

    // Capitalize first letter
    return value.charAt(0).toUpperCase() + value.slice(1);
}

/**
 * Format workspace path for display
 */
function formatWorkspace(workspace) {
    if (workspace === undefined || workspace === null || workspace === '') return '';
    const value = String(workspace);

    // Show last 2 path components
    const parts = value.split('/').filter(Boolean);
    if (parts.length <= 2) return value;

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
 * Set the search query programmatically.
 * Optionally triggers a search (default true).
 */
export async function setSearchQuery(query, options = {}) {
    const { runSearch = true, filters } = options;
    if (filters !== undefined) {
        await setSearchRoute({
            query,
            ...filters,
        }, { runSearch });
        return;
    }

    if (!elements.searchInput) {
        return;
    }

    const normalized = (query ?? '').toString();
    elements.searchInput.value = normalized;
    clearTimeout(searchTimeout);

    if (runSearch) {
        await handleSearch(normalized);
    } else {
        currentQuery = normalized.trim();
        updateSearchModeIndicator(currentQuery);
    }
}

export async function setSearchRoute(routeSearch = {}, options = {}) {
    const { runSearch = true } = options;
    if (!elements.searchInput) {
        return;
    }

    clearTimeout(searchTimeout);
    currentFilters = normalizeRouteFilters(routeSearch);
    syncFilterControls();

    const normalizedQuery = (routeSearch.query ?? routeSearch.q ?? '').toString();
    elements.searchInput.value = normalizedQuery;

    if (runSearch) {
        await handleSearch(normalizedQuery);
    } else {
        currentQuery = normalizedQuery.trim();
        updateSearchModeIndicator(currentQuery);
    }
}

/**
 * Clear search and reset to initial state
 */
export function clearSearch(options = {}) {
    const { reloadRecent = true } = options;

    clearTimeout(searchTimeout);
    searchEpoch += 1;
    currentQuery = '';
    currentFilters = createEmptySearchFilters();
    currentSearchMode = 'auto';
    currentResults = [];
    currentPage = 0;

    // Clean up virtual list if it exists
    destroyVirtualResultsView();
    hideLoading();

    if (elements.searchInput) {
        elements.searchInput.value = '';
    }
    syncFilterControls();
    if (elements.searchModeIndicator) {
        elements.searchModeIndicator.classList.add('hidden');
    }
    const announcer = document.getElementById('search-announcer');
    if (announcer) {
        announcer.textContent = '';
    }

    // Reset search mode toggle
    setSearchMode('auto');

    if (reloadRecent) {
        loadRecentConversations(searchEpoch);
    } else {
        hideNoResults();
        if (elements.resultsList) {
            elements.resultsList.innerHTML = '';
        }
        if (elements.resultCount) {
            elements.resultCount.textContent = '';
        }
    }
}

/**
 * Get current search state
 */
export function getSearchState() {
    return {
        query: currentQuery,
        filters: { ...currentFilters },
        searchMode: currentSearchMode,
        resultCount: currentResults.length,
    };
}

// Export default
export default {
    initSearch,
    clearSearch,
    getSearchState,
    setSearchQuery,
    setSearchRoute,
};
