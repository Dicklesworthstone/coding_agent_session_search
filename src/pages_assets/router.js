/**
 * cass Archive Hash-based Router
 *
 * Provides hash-based routing for static hosting compatibility.
 * Supports deep linking to conversations, messages, search queries, and panels.
 *
 * URL Schema:
 *   #/                      -> home / search
 *   #/search?q=auth+bug     -> search query
 *   #/c/12345               -> conversation 12345
 *   #/c/12345/m/67          -> message 67 in conversation 12345
 *   #/settings              -> settings panel
 *   #/stats                 -> analytics dashboard
 */

// Route definitions
const ROUTES = {
    HOME: '/',
    SEARCH: '/search',
    CONVERSATION: '/c/:id',
    CONVERSATION_MESSAGE: '/c/:id/m/:msgId',
    SETTINGS: '/settings',
    STATS: '/stats',
};

// Route handlers registry
const routeHandlers = new Map();

// Current route state
let currentRoute = {
    path: '/',
    params: {},
    query: {},
    raw: '',
};

// Router instance
let routerInstance = null;

export function parseRouteIdSegment(segment) {
    if (typeof segment !== 'string' || !/^[1-9]\d*$/.test(segment)) {
        return null;
    }

    const value = Number.parseInt(segment, 10);
    return Number.isSafeInteger(value) ? value : null;
}

export function parseConversationRouteParts(parts) {
    if (!Array.isArray(parts) || parts[0] !== 'c') {
        return null;
    }

    if (parts.length !== 2 && !(parts.length === 4 && parts[2] === 'm' && parts[3])) {
        return null;
    }

    const conversationId = parseRouteIdSegment(parts[1]);
    if (conversationId === null) {
        return null;
    }

    const messageId = parts.length === 4 ? parseRouteIdSegment(parts[3]) : null;
    if (parts.length === 4 && messageId === null) {
        return null;
    }

    return {
        conversationId,
        messageId,
    };
}

/**
 * Hash-based Router class
 */
class Router {
    /**
     * Create a router instance
     * @param {Object} options - Router options
     * @param {Function} options.onNavigate - Callback when route changes
     * @param {boolean} options.autoInit - Auto-initialize on creation (default: true)
     */
    constructor(options = {}) {
        this.onNavigate = options.onNavigate || (() => {});
        this.autoInit = options.autoInit !== false;
        this._boundHashHandler = this._handleHashChange.bind(this);

        if (this.autoInit) {
            this.init();
        }
    }

    /**
     * Initialize the router
     */
    init() {
        // Listen for hash changes
        window.addEventListener('hashchange', this._boundHashHandler);

        // Handle initial route
        this._handleHashChange();

        console.debug('[Router] Initialized');
    }

    /**
     * Clean up router
     */
    destroy() {
        window.removeEventListener('hashchange', this._boundHashHandler);
        console.debug('[Router] Destroyed');
    }

    /**
     * Navigate to a path
     * @param {string} path - Path to navigate to
     * @param {Object} options - Navigation options
     * @param {boolean} options.replace - Replace current history entry
     */
    navigate(path, options = {}) {
        const normalizedPath = path.startsWith('/') ? path : `/${path}`;
        const newHash = `#${normalizedPath}`;

        if (options.replace) {
            window.location.replace(newHash);
        } else {
            window.location.hash = normalizedPath;
        }
    }

    /**
     * Navigate to home/search
     * @param {string} query - Optional search query
     */
    goHome(query = null) {
        if (query) {
            this.navigate(`/search?q=${encodeURIComponent(query)}`);
        } else {
            this.navigate('/');
        }
    }

    /**
     * Navigate to a conversation
     * @param {number|string} conversationId - Conversation ID
     * @param {number|string|null} messageId - Optional message ID to scroll to
     */
    goToConversation(conversationId, messageId = null) {
        if (messageId) {
            this.navigate(`/c/${conversationId}/m/${messageId}`);
        } else {
            this.navigate(`/c/${conversationId}`);
        }
    }

    /**
     * Navigate to settings
     */
    goToSettings() {
        this.navigate('/settings');
    }

    /**
     * Navigate to stats
     */
    goToStats() {
        this.navigate('/stats');
    }

    /**
     * Go back in history
     */
    back() {
        window.history.back();
    }

    /**
     * Get current route state
     * @returns {Object} Current route
     */
    getCurrentRoute() {
        return { ...currentRoute };
    }

    /**
     * Handle hash change events
     * @private
     */
    _handleHashChange() {
        const hash = (window.location.hash || '').slice(1) || '/';
        const parsed = this._parseHash(hash);

        currentRoute = parsed;

        // Call navigation handler
        this.onNavigate(parsed);

        // Dispatch custom event for other listeners
        window.dispatchEvent(new CustomEvent('cass:route-change', {
            detail: parsed,
        }));
    }

    /**
     * Parse a hash string into route components
     * @param {string} hash - Hash string (without #)
     * @returns {Object} Parsed route
     * @private
     */
    _parseHash(hash) {
        // Split path and query
        const [pathPart, queryPart] = hash.split('?');
        const path = pathPart || '/';

        // Parse query parameters
        const query = {};
        if (queryPart) {
            const searchParams = new URLSearchParams(queryPart);
            for (const [key, value] of searchParams) {
                query[key] = value;
            }
        }

        // Parse path and extract parameters
        const { view, params } = this._matchRoute(path);

        return {
            path,
            view,
            params,
            query,
            raw: hash,
        };
    }

    /**
     * Match path to route and extract parameters
     * @param {string} path - Path to match
     * @returns {Object} Matched view and params
     * @private
     */
    _matchRoute(path) {
        const parts = path.split('/').filter(Boolean);

        // Empty path or just '/' -> home/search
        if (parts.length === 0) {
            return { view: 'search', params: {} };
        }

        // /search -> search view
        if (parts[0] === 'search' && parts.length === 1) {
            return { view: 'search', params: {} };
        }

        // /c/:id -> conversation view
        if (parts[0] === 'c') {
            const conversationParams = parseConversationRouteParts(parts);
            if (conversationParams) {
                return {
                    view: 'conversation',
                    params: conversationParams,
                };
            }

            return { view: 'not-found', params: { path } };
        }

        // /settings -> settings panel
        if (parts[0] === 'settings' && parts.length === 1) {
            return { view: 'settings', params: {} };
        }

        // /stats -> stats panel
        if (parts[0] === 'stats' && parts.length === 1) {
            return { view: 'stats', params: {} };
        }

        // Unknown route -> 404 view
        return { view: 'not-found', params: { path } };
    }
}

/**
 * Create and initialize the global router
 * @param {Object} options - Router options
 * @returns {Router} Router instance
 */
export function createRouter(options = {}) {
    if (routerInstance) {
        console.warn('[Router] Router already exists, destroying old instance');
        routerInstance.destroy();
    }

    routerInstance = new Router(options);
    return routerInstance;
}

/**
 * Get the current router instance
 * @returns {Router|null} Router instance or null
 */
export function getRouter() {
    return routerInstance;
}

/**
 * Navigate to a path (convenience function)
 * @param {string} path - Path to navigate to
 * @param {Object} options - Navigation options
 */
export function navigate(path, options = {}) {
    if (!routerInstance) {
        console.error('[Router] Router not initialized');
        return;
    }
    routerInstance.navigate(path, options);
}

/**
 * Get current route (convenience function)
 * @returns {Object} Current route
 */
export function getCurrentRoute() {
    return { ...currentRoute };
}

/**
 * Build a path for a conversation
 * @param {number|string} conversationId - Conversation ID
 * @param {number|string|null} messageId - Optional message ID
 * @returns {string} Path string
 */
export function buildConversationPath(conversationId, messageId = null) {
    if (messageId) {
        return `/c/${conversationId}/m/${messageId}`;
    }
    return `/c/${conversationId}`;
}

/**
 * Build a path for search
 * @param {string} query - Search query
 * @param {Object} filters - Optional filters
 * @returns {string} Path string
 */
export function buildSearchPath(query = '', filters = {}) {
    const params = new URLSearchParams();

    if (query) {
        params.set('q', query);
    }

    if (filters.agent) {
        params.set('agent', filters.agent);
    }

    if (filters.timePreset && filters.timePreset !== 'custom') {
        params.set('time', filters.timePreset);
    } else {
        if (filters.since) {
            params.set('since', filters.since);
        }

        if (filters.until) {
            params.set('until', filters.until);
        }
    }

    const queryString = params.toString();
    return queryString ? `/search?${queryString}` : '/search';
}

/**
 * Parse search parameters from route
 * @param {Object} route - Route object
 * @returns {Object} Search parameters
 */
export function parseSearchParams(route) {
    return {
        query: route.query.q || '',
        agent: route.query.agent || null,
        timePreset: route.query.time || null,
        since: route.query.since || null,
        until: route.query.until || null,
    };
}

// Export route constants
export { ROUTES };

// Export Router class for advanced usage
export { Router };

// Export default
export default {
    createRouter,
    getRouter,
    navigate,
    getCurrentRoute,
    parseConversationRouteParts,
    parseRouteIdSegment,
    buildConversationPath,
    buildSearchPath,
    parseSearchParams,
    ROUTES,
    Router,
};
