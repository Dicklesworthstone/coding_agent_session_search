/**
 * cass Archive Service Worker
 *
 * Provides COOP/COEP headers for SharedArrayBuffer support,
 * offline caching, and proper resource management.
 */

const CACHE_NAME = 'cass-archive-v1';
const STATIC_ASSETS = [
    './',
    './index.html',
    './auth.js',
    './session.js',
    './crypto_worker.js',
    './styles.css',
    './viewer.js',
    './search.js',
    './database.js',
    './vendor/sqlite3.js',
    './vendor/sqlite3.wasm',
    './vendor/argon2-wasm.js',
    './vendor/fflate.min.js',
];

// Log levels
const LOG = {
    ERROR: 0,
    WARN: 1,
    INFO: 2,
    DEBUG: 3,
};

let logLevel = LOG.INFO;

function log(level, ...args) {
    if (level <= logLevel) {
        const prefix = ['[SW]', new Date().toISOString()];
        const levelName = Object.keys(LOG).find(k => LOG[k] === level);
        console.log(...prefix, `[${levelName}]`, ...args);
    }
}

/**
 * Install event: Cache static assets
 */
self.addEventListener('install', (event) => {
    log(LOG.INFO, 'Installing service worker...');

    event.waitUntil(
        caches.open(CACHE_NAME)
            .then((cache) => {
                log(LOG.INFO, 'Caching static assets');
                // Cache each asset individually to handle missing files gracefully
                return Promise.allSettled(
                    STATIC_ASSETS.map(asset =>
                        cache.add(asset).catch(e => {
                            log(LOG.WARN, `Failed to cache ${asset}:`, e.message);
                        })
                    )
                );
            })
            .then(() => {
                log(LOG.INFO, 'Service worker installed');
                // Skip waiting to activate immediately
                return self.skipWaiting();
            })
            .catch((error) => {
                log(LOG.ERROR, 'Installation failed:', error);
            })
    );
});

/**
 * Activate event: Clean up old caches
 */
self.addEventListener('activate', (event) => {
    log(LOG.INFO, 'Activating service worker...');

    event.waitUntil(
        caches.keys()
            .then((keys) => {
                return Promise.all(
                    keys
                        .filter(key => key !== CACHE_NAME)
                        .map(key => {
                            log(LOG.INFO, 'Deleting old cache:', key);
                            return caches.delete(key);
                        })
                );
            })
            .then(() => {
                log(LOG.INFO, 'Service worker activated');
                // Take control of all clients immediately
                return self.clients.claim();
            })
            .catch((error) => {
                log(LOG.ERROR, 'Activation failed:', error);
            })
    );
});

/**
 * Fetch event: Handle requests with COOP/COEP headers and caching
 */
self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);

    // Only handle same-origin requests
    if (url.origin !== self.location.origin) {
        return;
    }

    // Skip non-GET requests
    if (event.request.method !== 'GET') {
        return;
    }

    event.respondWith(handleFetch(event.request));
});

/**
 * Handle fetch request with caching and security headers
 */
async function handleFetch(request) {
    const url = new URL(request.url);

    // Try cache first for static assets
    try {
        const cached = await caches.match(request);
        if (cached) {
            log(LOG.DEBUG, 'Cache hit:', url.pathname);
            return addSecurityHeaders(cached.clone());
        }
    } catch (error) {
        log(LOG.WARN, 'Cache match error:', error);
    }

    // Network fetch
    try {
        const response = await fetch(request);

        // Only cache successful responses
        if (response.ok) {
            const cache = await caches.open(CACHE_NAME);
            // Clone response for caching
            cache.put(request, response.clone()).catch(e => {
                log(LOG.WARN, 'Cache put error:', e);
            });
        }

        return addSecurityHeaders(response);
    } catch (error) {
        log(LOG.ERROR, 'Fetch failed:', url.pathname, error.message);

        // Try cache as fallback for navigation requests
        if (request.mode === 'navigate') {
            const cachedIndex = await caches.match('./index.html');
            if (cachedIndex) {
                log(LOG.INFO, 'Serving cached index.html for offline navigation');
                return addSecurityHeaders(cachedIndex.clone());
            }
        }

        // Return offline error response
        return new Response('Offline - Resource not cached', {
            status: 503,
            statusText: 'Service Unavailable',
            headers: {
                'Content-Type': 'text/plain',
            },
        });
    }
}

/**
 * Add security headers for COOP/COEP and CSP
 *
 * These headers enable SharedArrayBuffer support required for
 * optimal sqlite-wasm performance.
 */
function addSecurityHeaders(response) {
    // Clone headers
    const headers = new Headers(response.headers);

    // COOP/COEP for SharedArrayBuffer support
    headers.set('Cross-Origin-Opener-Policy', 'same-origin');
    headers.set('Cross-Origin-Embedder-Policy', 'require-corp');

    // Content Security Policy
    headers.set('Content-Security-Policy', [
        "default-src 'self'",
        "script-src 'self' 'wasm-unsafe-eval'",
        "style-src 'self'",
        "img-src 'self' data: blob:",
        "connect-src 'self'",
        "worker-src 'self' blob:",
        "object-src 'none'",
        "frame-ancestors 'none'",
        "form-action 'none'",
        "base-uri 'none'",
    ].join('; '));

    // Additional security headers
    headers.set('X-Content-Type-Options', 'nosniff');
    headers.set('X-Frame-Options', 'DENY');
    headers.set('Referrer-Policy', 'no-referrer');
    headers.set('X-Robots-Tag', 'noindex, nofollow');

    return new Response(response.body, {
        status: response.status,
        statusText: response.statusText,
        headers,
    });
}

/**
 * Message event: Handle messages from clients
 */
self.addEventListener('message', (event) => {
    const { type, ...data } = event.data;

    // Helper to respond - use MessageChannel port if available, otherwise event.source
    const respond = (message) => {
        if (event.ports && event.ports[0]) {
            event.ports[0].postMessage(message);
        } else if (event.source) {
            event.source.postMessage(message);
        }
    };

    switch (type) {
        case 'SKIP_WAITING':
            self.skipWaiting();
            break;

        case 'GET_VERSION':
            respond({
                type: 'VERSION',
                version: CACHE_NAME,
            });
            break;

        case 'CLEAR_CACHE':
            caches.delete(CACHE_NAME).then(() => {
                respond({ type: 'CACHE_CLEARED' });
            });
            break;

        case 'SET_LOG_LEVEL':
            logLevel = data.level;
            log(LOG.INFO, 'Log level set to:', Object.keys(LOG).find(k => LOG[k] === logLevel));
            break;

        default:
            log(LOG.WARN, 'Unknown message type:', type);
    }
});

// Log startup
log(LOG.INFO, 'Service worker script loaded');
