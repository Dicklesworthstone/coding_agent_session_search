/**
 * cass Archive Service Worker
 *
 * Provides COOP/COEP headers for SharedArrayBuffer support,
 * offline caching, and proper resource management.
 */

const CACHE_VERSION = 'v3';
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

function hashScopeId(input) {
    let hash = 0x811c9dc5;
    for (let i = 0; i < input.length; i++) {
        hash ^= input.charCodeAt(i);
        hash = Math.imul(hash, 0x01000193) >>> 0;
    }
    return hash.toString(16).padStart(8, '0');
}

function getCacheScopeUrl() {
    try {
        return self.registration?.scope || self.location.href;
    } catch (error) {
        return self.location.href;
    }
}

function getCacheName() {
    return `cass-archive-${hashScopeId(getCacheScopeUrl())}-${CACHE_VERSION}`;
}

function getCachePrefix() {
    return `cass-archive-${hashScopeId(getCacheScopeUrl())}-`;
}

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
    const cacheName = getCacheName();

    event.waitUntil(
        caches.open(cacheName)
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
    const cacheName = getCacheName();
    const cachePrefix = getCachePrefix();

    event.waitUntil(
        caches.keys()
            .then((keys) => {
                return Promise.all(
                    keys
                        .filter((key) => key.startsWith(cachePrefix) && key !== cacheName)
                        .map(key => {
                            log(LOG.INFO, 'Deleting old cache:', key);
                            return caches.delete(key);
                        })
                );
            })
            .then((results) => {
                if (!results.every(Boolean)) {
                    log(LOG.WARN, 'Some old caches could not be deleted during activation');
                }
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
 * Fetch event: Handle requests with COOP/COEP headers and caching.
 * Use network-first so archive updates do not get pinned behind stale cache entries.
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
 * Handle fetch request with network-first caching and security headers.
 * This preserves offline support without letting old config/payload/viewer files
 * silently override newer archive content.
 */
async function handleFetch(request) {
    const url = new URL(request.url);
    const cacheName = getCacheName();

    // Network first so updated archive contents win when online.
    try {
        const response = await fetch(request);

        // Only cache successful responses
        if (response.ok) {
            try {
                const cache = await caches.open(cacheName);
                // Clone response for caching
                cache.put(request, response.clone()).catch(e => {
                    log(LOG.WARN, 'Cache put error:', e);
                });
            } catch (cacheError) {
                log(LOG.WARN, 'Cache open error:', cacheError);
            }
        }

        return addSecurityHeaders(response);
    } catch (error) {
        log(LOG.ERROR, 'Fetch failed:', url.pathname, error.message);

        // Offline/cache fallback
        try {
            const cached = await caches.match(request);
            if (cached) {
                log(LOG.INFO, 'Serving cached response after network failure:', url.pathname);
                return addSecurityHeaders(cached.clone());
            }
        } catch (cacheError) {
            log(LOG.WARN, 'Cache fallback error:', cacheError);
        }

        // Try cache as fallback for navigation requests
        if (request.mode === 'navigate') {
            try {
                const cachedIndex = await caches.match('./index.html');
                if (cachedIndex) {
                    log(LOG.INFO, 'Serving cached index.html for offline navigation');
                    return addSecurityHeaders(cachedIndex.clone());
                }
            } catch (cacheError) {
                log(LOG.WARN, 'Navigation cache fallback error:', cacheError);
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
    const respond = (message) => {
        if (event.ports && event.ports[0]) {
            event.ports[0].postMessage(message);
        } else if (event.source) {
            event.source.postMessage(message);
        }
    };

    const rejectRequest = (error) => {
        respond({
            type: 'REQUEST_INVALID',
            error,
        });
    };

    const payload = event.data && typeof event.data === 'object' ? event.data : null;
    if (!payload) {
        log(LOG.WARN, 'Ignoring malformed message payload');
        rejectRequest('Malformed message payload');
        return;
    }

    const { type, ...data } = payload;
    if (typeof type !== 'string' || type.length === 0) {
        log(LOG.WARN, 'Ignoring message without a valid type');
        rejectRequest('Message type must be a non-empty string');
        return;
    }

    switch (type) {
        case 'SKIP_WAITING':
            self.skipWaiting();
            break;

        case 'GET_VERSION':
            respond({
                type: 'VERSION',
                version: getCacheName(),
            });
            break;

        case 'CLEAR_CACHE':
            caches.keys()
                .then((keys) => {
                    const cachePrefix = getCachePrefix();
                    const targets = keys.filter((key) => key.startsWith(cachePrefix));
                    return Promise.all(targets.map((key) => caches.delete(key))).then((results) => ({
                        targets,
                        cleared: results.every(Boolean),
                    }));
                })
                .then(({ targets, cleared }) => {
                    if (!cleared) {
                        throw new Error('Some cache entries could not be deleted');
                    }
                    respond({
                        type: 'CACHE_CLEARED',
                        cleared: targets,
                    });
                })
                .catch((error) => {
                    log(LOG.WARN, 'Failed to clear cache:', error);
                    respond({
                        type: 'CACHE_CLEAR_FAILED',
                        error: error?.message || String(error),
                    });
                });
            break;

        case 'SET_LOG_LEVEL':
            logLevel = data.level;
            log(LOG.INFO, 'Log level set to:', Object.keys(LOG).find(k => LOG[k] === logLevel));
            break;

        default:
            log(LOG.WARN, 'Unknown message type:', type);
            rejectRequest(`Unknown message type: ${type}`);
    }
});

// Log startup
log(LOG.INFO, 'Service worker script loaded');
