/**
 * cass Archive Viewer - Main Application Module
 *
 * Ties together search, conversation viewer, and database modules.
 * Manages application state and view transitions with hash-based routing.
 *
 * Routes:
 *   #/                      -> home / search
 *   #/search?q=auth+bug     -> search query
 *   #/c/12345               -> conversation 12345
 *   #/c/12345/m/67          -> message 67 in conversation 12345
 *   #/settings              -> settings panel
 *   #/stats                 -> analytics dashboard
 */

import { isDatabaseReady, getStatistics, closeDatabase } from './database.js';
import { initSearch, clearSearch, getSearchState, setSearchRoute } from './search.js';
import {
    initConversationViewer,
    loadConversation,
    clearViewer,
    cleanupConversationViewer,
    getCurrentConversation,
} from './conversation.js';
import { createRouter, getRouter, parseSearchParams, buildConversationPath, buildSearchPath } from './router.js';
import { getConversationLink, copyConversationLink, isWebShareAvailable, shareConversation } from './share.js';
import { initStats, renderStatsDashboard, clearStatsCache } from './stats.js';
import { initStorage, StorageKeys } from './storage.js';
import { initSettings, render as renderSettings, cleanupSettings } from './settings.js';

// Application state
const state = {
    view: 'search', // 'search' | 'conversation' | 'settings' | 'stats' | 'not-found'
    conversationId: null,
    messageId: null,
    searchQuery: '',
    searchFilters: {
        agent: null,
        since: null,
        until: null,
        timePreset: null,
    },
    initialized: false,
};

// Router instance
let router = null;
let storageReady = null;
let settingsReady = false;
let waitingForDatabaseReady = false;
let viewerLifecycleEpoch = 0;

// DOM element references
let elements = {
    appContent: null,
    searchView: null,
    conversationView: null,
    settingsView: null,
    statsView: null,
    notFoundView: null,
    statsDisplay: null,
    navBar: null,
};

/**
 * Initialize the viewer application
 */
export function init() {
    console.log('[Viewer] Initializing...');

    // Get the app content container
    elements.appContent = document.getElementById('app-content');

    if (!elements.appContent) {
        console.error('[Viewer] App content container not found');
        return;
    }

    if (state.initialized) {
        if (!isDatabaseReady()) {
            console.log('[Viewer] Waiting for database re-open...');
            ensureDatabaseReadyListener();
            return;
        }

        refreshAfterDatabaseReady();
        return;
    }

    if (!isDatabaseReady()) {
        console.log('[Viewer] Waiting for database...');
        ensureDatabaseReadyListener();
        return;
    }

    initializeViews();
}

function ensureDatabaseReadyListener() {
    if (waitingForDatabaseReady) {
        return;
    }

    waitingForDatabaseReady = true;
    window.addEventListener('cass:db-ready', handleDatabaseReady);
}

/**
 * Handle database ready event
 */
function handleDatabaseReady(event) {
    console.log('[Viewer] Database ready:', event.detail);
    window.removeEventListener('cass:db-ready', handleDatabaseReady);
    waitingForDatabaseReady = false;

    if (state.initialized) {
        refreshAfterDatabaseReady();
        return;
    }

    initializeViews();
}

/**
 * Initialize views after database is ready
 */
function initializeViews() {
    const lifecycleEpoch = ++viewerLifecycleEpoch;

    // Clear loading state
    elements.appContent.innerHTML = '';

    // Create view containers
    createViewContainers();

    // Expose notifications to settings module
    window.showNotification = showNotification;

    // Apply stored theme early
    applyStoredTheme();

    // Initialize storage and settings
    storageReady = initStorage().then(() => ({ ok: true })).catch((error) => {
        console.warn('[Viewer] Storage init failed:', error);
        return { ok: false, error };
    });
    storageReady.then((result) => {
        void initializeSettingsAfterStorageReady(result, lifecycleEpoch);
    });

    // Initialize search view
    initSearch(elements.searchView, handleResultSelect);

    // Initialize conversation viewer
    initConversationViewer(elements.conversationView, handleBackToSearch);

    // Initialize stats module
    initStats(elements.statsView);

    // Create router with navigation handler
    router = createRouter({
        onNavigate: handleRouteChange,
    });

    window.addEventListener('cass:lock', handleGlobalLock);

    // Mark as initialized
    state.initialized = true;

    console.log('[Viewer] Initialized with hash-based routing');
}

function refreshAfterDatabaseReady() {
    if (!state.initialized) {
        initializeViews();
        return;
    }

    switch (state.view) {
        case 'conversation':
            if (state.conversationId) {
                handleConversationRoute(state.conversationId, state.messageId);
                return;
            }
            break;
        case 'settings':
            handleSettingsRoute();
            return;
        case 'stats':
            handleStatsRoute();
            return;
        case 'not-found':
            handleNotFoundRoute(window.location.hash || '/');
            return;
        default:
            break;
    }

    handleSearchRoute({
        query: {
            q: state.searchQuery,
            agent: state.searchFilters.agent,
            since: state.searchFilters.since,
            until: state.searchFilters.until,
            time: state.searchFilters.timePreset && state.searchFilters.timePreset !== 'custom'
                ? state.searchFilters.timePreset
                : null,
        },
    });
}

/**
 * Create view containers
 */
function createViewContainers() {
    elements.appContent.innerHTML = `
        <nav id="nav-bar" class="nav-bar">
            <div class="nav-brand">
                <a href="#/" class="nav-logo">cass Archive</a>
            </div>
            <div class="nav-links">
                <a href="#/" class="nav-link" data-view="search">Search</a>
                <a href="#/stats" class="nav-link" data-view="stats">Stats</a>
                <a href="#/settings" class="nav-link" data-view="settings">Settings</a>
            </div>
        </nav>
        <div id="stats-display" class="stats-display"></div>
        <div id="search-view" class="view-container"></div>
        <div id="conversation-view" class="view-container hidden"></div>
        <div id="settings-view" class="view-container hidden"></div>
        <div id="stats-view" class="view-container hidden"></div>
        <div id="not-found-view" class="view-container hidden"></div>
    `;

    elements.navBar = document.getElementById('nav-bar');
    elements.searchView = document.getElementById('search-view');
    elements.conversationView = document.getElementById('conversation-view');
    elements.settingsView = document.getElementById('settings-view');
    elements.statsView = document.getElementById('stats-view');
    elements.notFoundView = document.getElementById('not-found-view');
    elements.statsDisplay = document.getElementById('stats-display');

    // Set up nav link highlighting
    setupNavLinks();
}

/**
 * Set up navigation link click handling
 */
function setupNavLinks() {
    const navLinks = elements.navBar.querySelectorAll('.nav-link');
    navLinks.forEach(link => {
        link.addEventListener('click', (e) => {
            // Update active state (router handles actual navigation)
            updateActiveNavLink(link.dataset.view);
        });
    });
}

/**
 * Update active navigation link
 */
function updateActiveNavLink(activeView) {
    const navLinks = elements.navBar.querySelectorAll('.nav-link');
    navLinks.forEach(link => {
        if (link.dataset.view === activeView) {
            link.classList.add('active');
        } else {
            link.classList.remove('active');
        }
    });
}

/**
 * Handle route changes from the router
 */
function handleRouteChange(route) {
    console.debug('[Viewer] Route change:', route);

    const { view, params, query } = route;
    const leavingConversation = state.view === 'conversation' && view !== 'conversation';
    const leavingSearch = state.view === 'search' && view !== 'search';
    const leavingStats = state.view === 'stats' && view !== 'stats';

    if (leavingConversation) {
        clearViewer();
    }
    if (leavingSearch) {
        clearSearch({ reloadRecent: false });
    }
    if (leavingStats) {
        clearStatsCache();
    }

    switch (view) {
        case 'search':
            handleSearchRoute(route);
            break;

        case 'conversation':
            handleConversationRoute(params.conversationId, params.messageId);
            break;

        case 'settings':
            handleSettingsRoute();
            break;

        case 'stats':
            handleStatsRoute();
            break;

        case 'not-found':
        default:
            handleNotFoundRoute(params.path || route.raw);
            break;
    }
}

/**
 * Handle search route
 */
function handleSearchRoute(route = { query: {} }) {
    const searchParams = parseSearchParams(route);

    state.view = 'search';
    state.conversationId = null;
    state.messageId = null;
    state.searchQuery = searchParams.query;
    state.searchFilters = {
        agent: searchParams.agent,
        since: searchParams.since,
        until: searchParams.until,
        timePreset: searchParams.timePreset,
    };

    // Show search view
    showViewContainer('search');

    // Display stats header
    displayStats();

    // Update nav
    updateActiveNavLink('search');

    if (state.searchQuery || state.searchFilters.agent || state.searchFilters.since || state.searchFilters.until || state.searchFilters.timePreset) {
        console.debug('[Viewer] Search route from URL:', searchParams);
        setSearchRoute(searchParams).catch((error) => {
            console.warn('[Viewer] Failed to run search route from URL:', error);
        });
        return;
    }

    clearSearch({ reloadRecent: true });
}

/**
 * Handle conversation route
 */
function handleConversationRoute(conversationId, messageId = null) {
    if (!conversationId) {
        handleNotFoundRoute('/c/');
        return;
    }

    state.view = 'conversation';
    state.conversationId = conversationId;
    state.messageId = messageId;

    // Show conversation view
    showViewContainer('conversation');

    // Load conversation
    loadConversation(conversationId, messageId);

    // Hide stats header
    if (elements.statsDisplay) {
        elements.statsDisplay.classList.add('hidden');
    }

    // Update nav (no specific nav for conversation)
    updateActiveNavLink(null);
}

/**
 * Handle settings route
 */
function handleSettingsRoute() {
    state.view = 'settings';
    state.conversationId = null;
    state.messageId = null;

    // Show settings view
    showViewContainer('settings');

    // Render settings panel
    renderSettingsPanel();

    // Hide stats header
    if (elements.statsDisplay) {
        elements.statsDisplay.classList.add('hidden');
    }

    // Update nav
    updateActiveNavLink('settings');
}

/**
 * Handle stats route
 */
function handleStatsRoute() {
    state.view = 'stats';
    state.conversationId = null;
    state.messageId = null;

    // Show stats view
    showViewContainer('stats');

    // Render stats panel
    renderStatsPanel();

    // Hide stats header
    if (elements.statsDisplay) {
        elements.statsDisplay.classList.add('hidden');
    }

    // Update nav
    updateActiveNavLink('stats');
}

/**
 * Handle not-found route
 */
function handleNotFoundRoute(path) {
    state.view = 'not-found';

    // Show not found view
    showViewContainer('not-found');

    // Render 404 content
    renderNotFoundPanel(path);

    // Hide stats header
    if (elements.statsDisplay) {
        elements.statsDisplay.classList.add('hidden');
    }

    // Update nav
    updateActiveNavLink(null);
}

/**
 * Show a specific view container
 */
function showViewContainer(viewName) {
    // Hide all views
    elements.searchView.classList.add('hidden');
    elements.conversationView.classList.add('hidden');
    elements.settingsView.classList.add('hidden');
    elements.statsView.classList.add('hidden');
    elements.notFoundView.classList.add('hidden');

    // Show requested view
    switch (viewName) {
        case 'search':
            elements.searchView.classList.remove('hidden');
            elements.statsDisplay.classList.remove('hidden');
            break;
        case 'conversation':
            elements.conversationView.classList.remove('hidden');
            break;
        case 'settings':
            elements.settingsView.classList.remove('hidden');
            break;
        case 'stats':
            elements.statsView.classList.remove('hidden');
            break;
        case 'not-found':
            elements.notFoundView.classList.remove('hidden');
            break;
    }
}

/**
 * Display archive statistics (header bar)
 */
function displayStats() {
    try {
        const stats = getStatistics();

        elements.statsDisplay.innerHTML = `
            <div class="stats-container">
                <div class="stat-item">
                    <span class="stat-value">${stats.conversations}</span>
                    <span class="stat-label">Conversations</span>
                </div>
                <div class="stat-item">
                    <span class="stat-value">${stats.messages}</span>
                    <span class="stat-label">Messages</span>
                </div>
                <div class="stat-item">
                    <span class="stat-value">${stats.agents.length}</span>
                    <span class="stat-label">Agents</span>
                </div>
            </div>
        `;
        elements.statsDisplay.classList.remove('hidden');
    } catch (error) {
        console.error('[Viewer] Failed to display stats:', error);
        elements.statsDisplay.innerHTML = '';
    }
}

/**
 * Render settings panel
 */
function renderSettingsPanel() {
    if (storageReady) {
        storageReady.then((result) => {
            void renderSettingsPanelAfterStorageReady(result);
        });
        return;
    }

    if (settingsReady) {
        void renderSettingsPanelNow();
    }
}

async function initializeSettingsAfterStorageReady(result, lifecycleEpoch) {
    if (lifecycleEpoch !== viewerLifecycleEpoch) {
        return;
    }

    if (!result?.ok) {
        settingsReady = false;
        return;
    }

    try {
        await initSettings(elements.settingsView, {
            onSessionReset: handleSessionReset,
        });
        if (lifecycleEpoch !== viewerLifecycleEpoch) {
            return;
        }
        settingsReady = true;
    } catch (error) {
        console.error('[Viewer] Failed to initialize settings:', error);
        settingsReady = false;
        if (state.initialized && state.view === 'settings') {
            renderSettingsErrorPanel('Settings could not be initialized for this archive.');
        }
    }
}

async function renderSettingsPanelAfterStorageReady(result) {
    if (!result?.ok) {
        if (state.initialized && state.view === 'settings') {
            renderSettingsErrorPanel('Settings are unavailable because browser storage failed to initialize.');
        }
        return;
    }

    if (!settingsReady || !state.initialized || state.view !== 'settings') {
        return;
    }

    await renderSettingsPanelNow();
}

async function renderSettingsPanelNow() {
    try {
        await renderSettings();
    } catch (error) {
        console.error('[Viewer] Failed to render settings panel:', error);
        renderSettingsErrorPanel('Settings could not be rendered for this archive.');
    }
}

/**
 * Apply theme
 */
function applyTheme(theme) {
    const root = document.documentElement;

    if (theme === 'auto') {
        root.removeAttribute('data-theme');
    } else {
        root.setAttribute('data-theme', theme);
    }
}

function applyStoredTheme() {
    try {
        const theme = localStorage.getItem(StorageKeys.THEME) || 'auto';
        applyTheme(theme);
    } catch (error) {
        // Ignore storage errors
    }
}

/**
 * Render stats panel (full analytics view)
 * Delegates to the stats module for precomputed analytics
 */
function renderStatsPanel() {
    // Use the stats module for rendering
    renderStatsDashboard();
}

/**
 * Render 404 not found panel
 */
function renderNotFoundPanel(path) {
    elements.notFoundView.innerHTML = `
        <div class="panel not-found-panel">
            <div class="not-found-content">
                <div class="not-found-icon">404</div>
                <h2>Page Not Found</h2>
                <p>The requested page <code>${escapeHtml(path || 'unknown')}</code> could not be found.</p>
                <a href="#/" class="btn btn-primary">Go to Search</a>
            </div>
        </div>
    `;
}

function renderSettingsErrorPanel(message) {
    if (!elements.settingsView) {
        return;
    }

    elements.settingsView.innerHTML = `
        <div class="panel settings-panel">
            <header class="panel-header">
                <h2>Settings</h2>
            </header>
            <div class="panel-content">
                <p>${escapeHtml(message)}</p>
            </div>
        </div>
    `;
}

/**
 * Handle search result selection
 */
function handleResultSelect(conversationId, messageId = null) {
    // Navigate using router
    if (router) {
        router.goToConversation(conversationId, messageId);
    }
}

/**
 * Handle back to search
 */
function handleBackToSearch() {
    clearViewer();

    // Navigate using router
    if (router) {
        const searchState = getSearchState();
        router.navigate(buildSearchPath(searchState.query, searchState.filters));
    }
}

function syncLockedViewerState() {
    state.view = 'search';
    state.conversationId = null;
    state.messageId = null;
    state.searchQuery = '';
    state.searchFilters = {
        agent: null,
        since: null,
        until: null,
        timePreset: null,
    };

    if (window?.location?.href) {
        const url = new URL(window.location.href);
        url.hash = '/';
        if (window.history?.replaceState) {
            window.history.replaceState(null, '', url.toString());
        } else {
            window.location.replace(url.toString());
        }
    }
}

function handleSessionReset(action) {
    syncLockedViewerState();
    cleanup();
    window.dispatchEvent(new CustomEvent('cass:lock', {
        detail: { action, source: 'viewer' },
    }));
}

function handleGlobalLock(event) {
    if (event?.detail?.source === 'viewer') {
        return;
    }

    syncLockedViewerState();
    cleanup();
}

/**
 * Navigate to a conversation (public API)
 */
export function navigateToConversation(conversationId, messageId = null) {
    if (router) {
        router.goToConversation(conversationId, messageId);
    }
}

/**
 * Navigate to search (public API)
 */
export function navigateToSearch(query = null, filters = {}) {
    if (router) {
        router.navigate(buildSearchPath(query || '', filters));
    }
}

/**
 * Get share link for current conversation
 */
export function getCurrentShareLink() {
    if (state.view === 'conversation' && state.conversationId) {
        return getConversationLink(state.conversationId, state.messageId);
    }
    return null;
}

/**
 * Copy current conversation link to clipboard
 */
export async function copyCurrentLink() {
    if (state.view === 'conversation' && state.conversationId) {
        const result = await copyConversationLink(state.conversationId, state.messageId);
        if (result.success) {
            showNotification('Link copied to clipboard', 'success');
        } else {
            showNotification('Failed to copy link', 'error');
        }
        return result;
    }
    return { success: false, link: null };
}

/**
 * Share current conversation (using Web Share API)
 */
export async function shareCurrentConversation() {
    if (state.view === 'conversation' && state.conversationId) {
        const conv = getCurrentConversation();
        const title = conv?.title || 'Conversation';
        const success = await shareConversation(state.conversationId, title, state.messageId);
        return success;
    }
    return false;
}

/**
 * Show a notification toast
 */
function showNotification(message, type = 'info') {
    // Check if toast container exists
    let toastContainer = document.getElementById('toast-container');
    if (!toastContainer) {
        toastContainer = document.createElement('div');
        toastContainer.id = 'toast-container';
        toastContainer.className = 'toast-container';
        document.body.appendChild(toastContainer);
    }

    // Create toast
    const toast = document.createElement('div');
    toast.className = `toast toast-${type}`;
    toast.textContent = message;

    toastContainer.appendChild(toast);

    // Auto-remove after delay
    setTimeout(() => {
        toast.classList.add('toast-fade-out');
        setTimeout(() => {
            toast.remove();
        }, 300);
    }, 3000);
}

/**
 * Format agent name for display
 */
function formatAgentName(agent) {
    if (agent === undefined || agent === null || agent === '') return 'Unknown';
    const value = String(agent);
    // Capitalize first letter
    return value.charAt(0).toUpperCase() + value.slice(1).replace(/_/g, ' ');
}

/**
 * Format date for display
 */
function formatDate(timestamp) {
    if (!timestamp) return 'Unknown';

    const date = new Date(timestamp);
    return date.toLocaleDateString(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
        hour: '2-digit',
        minute: '2-digit',
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
 * Clean up resources
 */
export function cleanup() {
    viewerLifecycleEpoch += 1;

    // Destroy router
    if (router) {
        router.destroy();
        router = null;
    }

    window.removeEventListener('cass:db-ready', handleDatabaseReady);
    window.removeEventListener('cass:lock', handleGlobalLock);
    waitingForDatabaseReady = false;
    storageReady = null;
    settingsReady = false;
    state.initialized = false;

    cleanupSettings();
    closeDatabase();
    clearSearch({ reloadRecent: false });
    cleanupConversationViewer();
    clearStatsCache();
    console.log('[Viewer] Cleaned up');
}

/**
 * Get current application state
 */
export function getState() {
    return { ...state };
}

/**
 * Get router instance
 */
export function getViewerRouter() {
    return router;
}

// Export default
export default {
    init,
    cleanup,
    getState,
    getViewerRouter,
    navigateToConversation,
    navigateToSearch,
    getCurrentShareLink,
    copyCurrentLink,
    shareCurrentConversation,
};
