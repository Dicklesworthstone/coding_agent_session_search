/**
 * cass Archive Service Worker
 *
 * Provides COOP/COEP headers for SharedArrayBuffer support,
 * offline caching, and proper resource management.
 */

// Bump whenever any shipped archive asset or cache policy changes. A new name
// keeps install from mutating the active generation entry-by-entry.
const CACHE_VERSION = 'v6';
const STATIC_ASSETS = [
    './',
    './index.html',
    './styles.css',
    './auth.js',
    './password-strength.js',
    './session.js',
    './storage.js',
    './coi-detector.js',
    './crypto_worker.js',
    './viewer.js',
    './router.js',
    './share.js',
    './stats.js',
    './search.js',
    './database.js',
    './conversation.js',
    './virtual-list.js',
    './attachments.js',
    './settings.js',
    './sw-register.js',
    './vendor/sqlite3.mjs',
    './vendor/sqlite3.wasm',
    './vendor/sqlite3-opfs-async-proxy.js',
    './vendor/argon2.js',
    './vendor/argon2-wasm.js',
    './vendor/argon2.wasm',
    './vendor/fflate.js',
    './vendor/html5-qrcode.min.js',
    './vendor/manifest.json',
    './vendor/LICENSE-sqlite-wasm.txt',
    './vendor/LICENSE-argon2-browser.txt',
    './vendor/LICENSE-fflate.txt',
    './vendor/LICENSE-html5-qrcode.txt',
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

function isWithinArchiveScope(url) {
    const scopeUrl = new URL(getCacheScopeUrl());
    return url.origin === scopeUrl.origin && url.pathname.startsWith(scopeUrl.pathname);
}

function isCacheEligibleRequest(request, url) {
    return isWithinArchiveScope(url)
        && url.search === ''
        && !request.headers.has('authorization')
        && !request.headers.has('range')
        && request.cache !== 'no-store';
}

function responseAllowsCaching(response) {
    const cacheDirectives = (response.headers.get('cache-control') || '')
        .split(',')
        .map((directive) => directive.trim().toLowerCase());
    const forbidsStorage = cacheDirectives.some((directive) => {
        const directiveName = directive.split('=', 1)[0].trim();
        return directiveName === 'no-store'
            || directiveName === 'no-cache'
            || directiveName === 'private';
    });
    return response.status === 200 && !forbidsStorage;
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
                // Runtime dependencies are part of the bundle contract. A
                // missing asset must fail installation instead of producing a
                // service worker that only breaks once the archive is offline.
                return Promise.all(STATIC_ASSETS.map(asset => cache.add(asset)));
            })
            .then(() => {
                log(LOG.INFO, 'Service worker installed');
            })
            .catch((error) => {
                log(LOG.ERROR, 'Installation failed:', error);
                throw error;
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

    // A controlled archive page can fetch same-origin URLs outside the
    // registration scope. Leave those unrelated requests entirely alone.
    if (!isWithinArchiveScope(url)) {
        return;
    }

    // Skip non-GET requests
    if (event.request.method !== 'GET') {
        return;
    }

    const backgroundTasks = [];
    const responsePromise = handleFetch(
        event.request,
        (task) => backgroundTasks.push(task)
    );
    event.respondWith(responsePromise);
    // Keep the worker alive until any cache write scheduled by handleFetch()
    // settles, without delaying the network response on large payload chunks.
    event.waitUntil(
        responsePromise.then(() => Promise.all(backgroundTasks)).then(() => undefined)
    );
});

/**
 * Handle fetch request with network-first caching and security headers.
 * This preserves offline support without letting old config/payload/viewer files
 * silently override newer archive content.
 */
async function handleFetch(request, trackBackgroundTask = () => {}) {
    const url = new URL(request.url);
    const cacheName = getCacheName();
    const cacheEligible = isCacheEligibleRequest(request, url);
    let cachePromise = null;
    const getCurrentCache = () => {
        cachePromise ||= caches.open(cacheName);
        return cachePromise;
    };

    // Network first so updated archive contents win when online.
    try {
        const response = await fetch(request);

        // Only cache successful responses
        if (cacheEligible && responseAllowsCaching(response)) {
            // Clone before returning the original response. Once the client
            // starts consuming its body, a later clone would throw.
            const responseForCache = response.clone();
            const cacheWrite = getCurrentCache()
                .then((cache) => cache.put(request, responseForCache))
                .catch((error) => {
                    log(LOG.WARN, 'Cache put error:', error);
                });
            trackBackgroundTask(cacheWrite);
        }

        return addSecurityHeaders(response);
    } catch (error) {
        log(LOG.ERROR, 'Fetch failed:', url.pathname, error.message);

        // Offline/cache fallback
        try {
            if (cacheEligible) {
                const cache = await getCurrentCache();
                const cached = await cache.match(request);
                if (cached) {
                    log(LOG.INFO, 'Serving cached response after network failure:', url.pathname);
                    return addSecurityHeaders(cached.clone());
                }
            }
        } catch (cacheError) {
            log(LOG.WARN, 'Cache fallback error:', cacheError);
        }

        // Try cache as fallback for navigation requests
        if (cacheEligible && request.mode === 'navigate') {
            try {
                const cache = await getCurrentCache();
                const indexUrl = new URL('./index.html', getCacheScopeUrl()).href;
                const cachedIndex = await cache.match(indexUrl);
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
        case 'SKIP_WAITING': {
            const skipWaitingTask = self.skipWaiting().catch((error) => {
                log(LOG.WARN, 'Failed to activate waiting worker:', error);
                throw error;
            });
            event.waitUntil(skipWaitingTask);
            break;
        }

        case 'GET_VERSION':
            respond({
                type: 'VERSION',
                version: getCacheName(),
            });
            break;

        case 'CLEAR_CACHE': {
            const clearCacheTask = caches.keys()
                .then(async (keys) => {
                    const cachePrefix = getCachePrefix();
                    const targets = keys.filter((key) => key.startsWith(cachePrefix));
                    await Promise.allSettled(targets.map((key) => caches.delete(key)));
                    const remaining = (await caches.keys())
                        .filter((key) => key.startsWith(cachePrefix));
                    if (remaining.length > 0) {
                        throw new Error(
                            `Archive caches still present after cleanup: ${remaining.join(', ')}`
                        );
                    }
                    return targets;
                })
                .then((targets) => {
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
            event.waitUntil(clearCacheTask);
            break;
        }

        case 'SET_LOG_LEVEL':
            if (!Number.isInteger(data.level) || !Object.values(LOG).includes(data.level)) {
                rejectRequest('Invalid log level');
                break;
            }
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
