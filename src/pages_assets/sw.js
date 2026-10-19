/**
 * cass Archive Service Worker
 *
 * Provides COOP/COEP headers for SharedArrayBuffer support,
 * offline caching, and proper resource management.
 */

// Bump whenever any shipped archive asset or cache policy changes. A new name
// keeps install from mutating the active generation entry-by-entry.
const CACHE_VERSION = "v10";
const STATIC_ASSETS = [
  "./",
  "./index.html",
  "./styles.css",
  "./auth.js",
  "./password-strength.js",
  "./session.js",
  "./storage.js",
  "./coi-detector.js",
  "./crypto_worker.js",
  "./viewer.js",
  "./router.js",
  "./share.js",
  "./stats.js",
  "./search.js",
  "./database.js",
  "./conversation.js",
  "./virtual-list.js",
  "./attachments.js",
  "./settings.js",
  "./sw-register.js",
  "./vendor/sqlite3.mjs",
  "./vendor/sqlite3.wasm",
  "./vendor/sqlite3-opfs-async-proxy.js",
  "./vendor/argon2.js",
  "./vendor/argon2-wasm.js",
  "./vendor/argon2.wasm",
  "./vendor/fflate.js",
  "./vendor/html5-qrcode.min.js",
  "./vendor/manifest.json",
  "./vendor/LICENSE-sqlite-wasm.txt",
  "./vendor/LICENSE-argon2-browser.txt",
  "./vendor/LICENSE-fflate.txt",
  "./vendor/LICENSE-html5-qrcode.txt",
];

// Log levels
const LOG = {
  ERROR: 0,
  WARN: 1,
  INFO: 2,
  DEBUG: 3,
};

let logLevel = LOG.INFO;

// A clear is a boundary for this worker's runtime cache reads AND writes.
// Fetches started before/during it may still return their network response,
// but cannot restore cleared data or fall back to a pre-clear cache handle.
// New requests after cleanup may cache again; this is not an offline opt-out
// or a purge of the browser's separate HTTP cache.
let cacheEpoch = {};
let pendingCacheClears = 0;
let cacheClearTail = Promise.resolve();
const cacheMutations = new Set();
const cacheResources = new Map();
let cacheStorageUnsafe = false;

function cacheRequestIsCurrent(epoch) {
  return epoch !== null && epoch === cacheEpoch && pendingCacheClears === 0 && !cacheStorageUnsafe;
}

function acquireCacheResource(url) {
  // Both shipped entry URLs can satisfy offline navigation. They share a
  // revocation identity even though CacheStorage retains separate URL keys.
  if (url === new URL("./", getCacheScopeUrl()).href) {
    url = new URL("./index.html", getCacheScopeUrl()).href;
  }
  let resource = cacheResources.get(url);
  if (!resource) {
    resource = { url, users: 0, version: {}, blocked: false, mutations: new Set() };
    cacheResources.set(url, resource);
  }
  resource.users++;
  return resource;
}

function pruneCacheResource(resource) {
  // Keep ordering state only for live requests/mutations, not every URL ever
  // fetched. Failed eviction disables fallback globally until a successful
  // explicit clear, so it needs no unbounded in-memory denylist.
  if (resource.users === 0 && resource.mutations.size === 0) {
    cacheResources.delete(resource.url);
  }
}

function trackResourceMutation(resource, operation) {
  const task = trackCacheMutation(operation);
  resource.mutations.add(task);
  const release = () => {
    resource.mutations.delete(task);
    pruneCacheResource(resource);
  };
  void task.then(release, release);
  return task;
}

function revokeCachedResource(resource, epoch) {
  // A newer private/deleted response supersedes every earlier request for
  // this URL, including writes already consuming a body and pending matches.
  resource.version = {};
  resource.blocked = true;
  const earlierMutations = [...resource.mutations];
  return trackResourceMutation(resource, async () => {
    await Promise.allSettled(earlierMutations);
    if (!cacheRequestIsCurrent(epoch)) return;
    const cacheName = getCacheName();
    if (!await caches.has(cacheName) || !cacheRequestIsCurrent(epoch)) return;
    const cache = await caches.open(cacheName);
    if (!cacheRequestIsCurrent(epoch)) return;
    // Remove all Vary representations of the URL, not just this request's
    // header variant. Never reinterpret a server refusal as an offline miss.
    const entryUrl = new URL("./index.html", getCacheScopeUrl()).href;
    const aliases = resource.url === entryUrl
      ? [entryUrl, new URL("./", getCacheScopeUrl()).href] : [resource.url];
    // The root copy must not bypass a withdrawn index.html (or vice versa).
    // Settle every delete before completing this mutation, including failure,
    // so a concurrent clear still waits for all outstanding Cache API work.
    const results = await Promise.allSettled(aliases.map(url =>
      cache.delete(url, { ignoreVary: true }),
    ));
    const failed = results.find(result => result.status === "rejected");
    if (failed) throw failed.reason;
  }).catch((error) => {
    // Never serve a known-revoked copy after storage failed to remove it.
    // Network responses remain available. This guard lasts for this worker;
    // it is not a durable erasure receipt when the Cache API is failing.
    cacheStorageUnsafe = true;
    log(LOG.WARN, "Cache eviction failed; offline fallback disabled until cache clear:", error);
  }).finally(() => {
    resource.blocked = false;
  });
}

function trackCacheMutation(operation) {
  // Register before invoking any asynchronous Cache API work, including open.
  // Do not serialize body writes: queuing response clones behind a large
  // download would retain unread stream data for every subsequent request.
  const task = Promise.resolve().then(operation);
  cacheMutations.add(task);
  const release = () => cacheMutations.delete(task);
  void task.then(release, release);
  return task;
}

function discardCacheResponse(response) {
  // A clone's tee cancellation may wait for the network consumer. Do not
  // await it or make clearing depend on the client draining that response.
  void response.body?.cancel().catch(() => {});
}

function clearArchiveCaches() {
  // Invalidate synchronously, before keys()/open()/put() can resume. Requests
  // admitted DURING a clear carry no epoch and stay ineligible after it ends.
  cacheEpoch = {};
  pendingCacheClears++;
  const task = cacheClearTail.then(async () => {
    // Every previously admitted mutation settles before names are deleted.
    // No new mutation can enter while pendingCacheClears is nonzero. A slow
    // Cache API means no success acknowledgement, not a premature receipt.
    await Promise.allSettled([...cacheMutations]);
    const cachePrefix = getCachePrefix();
    const targets = (await caches.keys()).filter((key) => key.startsWith(cachePrefix));
    await Promise.allSettled(targets.map((key) => caches.delete(key)));
    const remaining = (await caches.keys()).filter((key) => key.startsWith(cachePrefix));
    if (remaining.length > 0) {
      throw new Error(`Archive caches still present after cleanup: ${remaining.join(", ")}`);
    }
    cacheStorageUnsafe = false;
    return targets;
  }).finally(() => {
    pendingCacheClears--;
  });
  // Multiple tabs can clear concurrently. A failed deletion must not poison
  // the next attempt, nor reopen admission while a later clear is queued.
  cacheClearTail = task.catch(() => {});
  return task;
}

function hashScopeId(input) {
  let hash = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, "0");
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
  return (
    isWithinArchiveScope(url) &&
    url.search === "" &&
    !request.headers.has("authorization") &&
    !request.headers.has("range") &&
    request.cache !== "no-store"
  );
}

function responseForbidsCaching(response) {
  const cacheDirectives = (response.headers.get("cache-control") || "")
    .split(",")
    .map((directive) => directive.trim().toLowerCase());
  return cacheDirectives.some((directive) => {
    const directiveName = directive.split("=", 1)[0].trim();
    return (
      directiveName === "no-store" || directiveName === "no-cache" || directiveName === "private"
    );
  });
}

function responseAllowsCaching(response) {
  return response.status === 200 && !responseForbidsCaching(response);
}

function responseRevokesCachedCopy(response) {
  return responseForbidsCaching(response) || [401, 403, 404, 410].includes(response.status);
}

function log(level, ...args) {
  if (level <= logLevel) {
    const prefix = ["[SW]", new Date().toISOString()];
    const levelName = Object.keys(LOG).find((k) => LOG[k] === level);
    console.log(...prefix, `[${levelName}]`, ...args);
  }
}

/**
 * Install event: Cache static assets
 */
self.addEventListener("install", (event) => {
  log(LOG.INFO, "Installing service worker...");
  const cacheName = getCacheName();
  const epoch = cacheEpoch;

  event.waitUntil(
    trackCacheMutation(async () => {
      if (!cacheRequestIsCurrent(epoch)) throw new Error("Archive installation invalidated");
      const cache = await caches.open(cacheName);
      log(LOG.INFO, "Caching static assets");
      // Cache.add ignores Cache-Control. Apply the same explicit storage
      // policy as runtime reads, and fail installation if offline essentials
      // cannot be retained. Settle every started put before completing the
      // mutation so a clear cannot race a sibling asset after one fails.
      const results = await Promise.allSettled(STATIC_ASSETS.map(async (asset) => {
        if (!cacheRequestIsCurrent(epoch)) throw new Error("Archive installation invalidated");
        const url = new URL(asset, getCacheScopeUrl()).href;
        const response = await fetch(url);
        if (!cacheRequestIsCurrent(epoch) || !responseAllowsCaching(response)) {
          discardCacheResponse(response);
          throw new Error(`Archive offline asset is unavailable or not cacheable: ${asset}`);
        }
        await cache.put(url, response);
      }));
      const failed = results.find((result) => result.status === "rejected");
      if (failed) throw failed.reason;
    })
      .then(() => {
        log(LOG.INFO, "Service worker installed");
      })
      .catch((error) => {
        log(LOG.ERROR, "Installation failed:", error);
        throw error;
      }),
  );
});

/**
 * Activate event: Clean up old caches
 */
self.addEventListener("activate", (event) => {
  log(LOG.INFO, "Activating service worker...");
  const cacheName = getCacheName();
  const cachePrefix = getCachePrefix();

  event.waitUntil(
    caches
      .keys()
      .then((keys) => {
        return Promise.all(
          keys
            .filter((key) => key.startsWith(cachePrefix) && key !== cacheName)
            .map((key) => {
              log(LOG.INFO, "Deleting old cache:", key);
              return caches.delete(key);
            }),
        );
      })
      .then((results) => {
        if (!results.every(Boolean)) {
          log(LOG.WARN, "Some old caches could not be deleted during activation");
        }
        log(LOG.INFO, "Service worker activated");
        // Take control of all clients immediately
        return self.clients.claim();
      })
      .catch((error) => {
        log(LOG.ERROR, "Activation failed:", error);
      }),
  );
});

/**
 * Fetch event: Handle requests with COOP/COEP headers and caching.
 * Use network-first so archive updates do not get pinned behind stale cache entries.
 */
self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  // A controlled archive page can fetch same-origin URLs outside the
  // registration scope. Leave those unrelated requests entirely alone.
  if (!isWithinArchiveScope(url)) {
    return;
  }

  // Skip non-GET requests
  if (event.request.method !== "GET") {
    return;
  }

  const backgroundTasks = [];
  const responsePromise = handleFetch(event.request, (task) => backgroundTasks.push(task));
  event.respondWith(responsePromise);
  // Keep the worker alive until any cache write scheduled by handleFetch()
  // settles, without delaying the network response on large payload chunks.
  event.waitUntil(responsePromise.then(() => Promise.all(backgroundTasks)).then(() => undefined));
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
  const epoch = pendingCacheClears === 0 && !cacheStorageUnsafe ? cacheEpoch : null;
  const resource = cacheEligible && epoch !== null ? acquireCacheResource(url.href) : null;
  const version = resource && !resource.blocked ? resource.version : null;
  const canUseCache = () => resource !== null && version !== null && !resource.blocked
    && version === resource.version && cacheRequestIsCurrent(epoch);

  // Network first so updated archive contents win when online.
  try {
    const response = await fetch(request);

    if (canUseCache() && responseRevokesCachedCopy(response)) {
      trackBackgroundTask(revokeCachedResource(resource, epoch));
    } else if (canUseCache() && responseAllowsCaching(response)) {
      // Clone before returning the original response. Once the client
      // starts consuming its body, a later clone would throw.
      const responseForCache = response.clone();
      const cacheWrite = trackResourceMutation(resource, async () => {
        if (!canUseCache()) {
          discardCacheResponse(responseForCache);
          return;
        }
        const cache = await caches.open(cacheName);
        if (!canUseCache()) {
          discardCacheResponse(responseForCache);
          return;
        }
        await cache.put(request, responseForCache);
      }).catch((error) => {
        discardCacheResponse(responseForCache);
        log(LOG.WARN, "Cache put error:", error);
      });
      trackBackgroundTask(cacheWrite);
    }

    return addSecurityHeaders(response);
  } catch (error) {
    log(LOG.ERROR, "Fetch failed:", url.pathname, error.message);

    // Offline/cache fallback
    try {
      if (canUseCache()) {
        // CacheStorage.match does not create an empty cache after a purge.
        const cached = await caches.match(request, { cacheName });
        if (cached && canUseCache() && responseAllowsCaching(cached)) {
          log(LOG.INFO, "Serving cached response after network failure:", url.pathname);
          return addSecurityHeaders(cached);
        }
        if (cached) discardCacheResponse(cached);
      }
    } catch (cacheError) {
      log(LOG.WARN, "Cache fallback error:", cacheError);
    }

    // Try cache as fallback for navigation requests
    if (canUseCache() && request.mode === "navigate") {
      const indexUrl = new URL("./index.html", getCacheScopeUrl()).href;
      const entryResource = acquireCacheResource(indexUrl);
      const entryVersion = entryResource.blocked ? null : entryResource.version;
      const canUseEntry = () => canUseCache() && entryVersion !== null
        && !entryResource.blocked && entryVersion === entryResource.version;
      try {
        // The original navigation URL is not the resource we read here.
        // Fence the actual entry document before AND after the async match.
        if (canUseEntry()) {
          const cachedIndex = await caches.match(indexUrl, { cacheName });
          if (cachedIndex && canUseEntry() && responseAllowsCaching(cachedIndex)) {
            log(LOG.INFO, "Serving cached index.html for offline navigation");
            return addSecurityHeaders(cachedIndex);
          }
          if (cachedIndex) discardCacheResponse(cachedIndex);
        }
      } catch (cacheError) {
        log(LOG.WARN, "Navigation cache fallback error:", cacheError);
      } finally {
        entryResource.users--;
        pruneCacheResource(entryResource);
      }
    }

    // Return offline error response
    return new Response("Offline - Resource not cached", {
      status: 503,
      statusText: "Service Unavailable",
      headers: {
        "Content-Type": "text/plain",
      },
    });
  } finally {
    if (resource) {
      resource.users--;
      pruneCacheResource(resource);
    }
  }
}

/**
 * Add security headers for COOP/COEP and CSP
 *
 * These headers enable SharedArrayBuffer support required for
 * optimal sqlite-wasm performance.
 */
function addSecurityHeaders(response) {
  // Opaque (including manual redirect) responses cannot be reconstructed with
  // status 0. Preserve them rather than converting a network result into an
  // offline fallback. The browser enforces their request-mode restrictions.
  if (response.status === 0) return response;
  // Clone headers
  const headers = new Headers(response.headers);

  // COOP/COEP for SharedArrayBuffer support
  headers.set("Cross-Origin-Opener-Policy", "same-origin");
  headers.set("Cross-Origin-Embedder-Policy", "require-corp");

  // Content Security Policy
  headers.set(
    "Content-Security-Policy",
    [
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
    ].join("; "),
  );

  // Additional security headers
  headers.set("X-Content-Type-Options", "nosniff");
  headers.set("X-Frame-Options", "DENY");
  headers.set("Referrer-Policy", "no-referrer");
  headers.set("X-Robots-Tag", "noindex, nofollow");

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

/**
 * Message event: Handle messages from clients
 */
self.addEventListener("message", (event) => {
  const respond = (message) => {
    if (event.ports && event.ports[0]) {
      event.ports[0].postMessage(message);
    } else if (event.source) {
      event.source.postMessage(message);
    }
  };

  const rejectRequest = (error) => {
    respond({
      type: "REQUEST_INVALID",
      error,
    });
  };

  const payload = event.data && typeof event.data === "object" ? event.data : null;
  if (!payload) {
    log(LOG.WARN, "Ignoring malformed message payload");
    rejectRequest("Malformed message payload");
    return;
  }

  const { type, ...data } = payload;
  if (typeof type !== "string" || type.length === 0) {
    log(LOG.WARN, "Ignoring message without a valid type");
    rejectRequest("Message type must be a non-empty string");
    return;
  }

  switch (type) {
    case "SKIP_WAITING": {
      const skipWaitingTask = self.skipWaiting().catch((error) => {
        log(LOG.WARN, "Failed to activate waiting worker:", error);
        throw error;
      });
      event.waitUntil(skipWaitingTask);
      break;
    }

    case "GET_VERSION":
      respond({
        type: "VERSION",
        version: getCacheName(),
      });
      break;

    case "CLEAR_CACHE": {
      const clearCacheTask = clearArchiveCaches()
        .then((targets) => {
          respond({
            type: "CACHE_CLEARED",
            cleared: targets,
          });
        })
        .catch((error) => {
          log(LOG.WARN, "Failed to clear cache:", error);
          respond({
            type: "CACHE_CLEAR_FAILED",
            error: error?.message || String(error),
          });
        });
      event.waitUntil(clearCacheTask);
      break;
    }

    case "SET_LOG_LEVEL":
      if (!Number.isInteger(data.level) || !Object.values(LOG).includes(data.level)) {
        rejectRequest("Invalid log level");
        break;
      }
      logLevel = data.level;
      log(
        LOG.INFO,
        "Log level set to:",
        Object.keys(LOG).find((k) => LOG[k] === logLevel),
      );
      break;

    default:
      log(LOG.WARN, "Unknown message type:", type);
      rejectRequest(`Unknown message type: ${type}`);
  }
});

// Log startup
log(LOG.INFO, "Service worker script loaded");
