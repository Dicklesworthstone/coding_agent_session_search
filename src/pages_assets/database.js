/**
 * cass Archive Database Module
 *
 * sqlite-wasm integration for browser-based database queries.
 *
 * The live database is a read-only in-memory deserialization. The official
 * sqlite-wasm OPFS VFS requires a dedicated worker because its synchronous VFS
 * bridge uses Atomics.wait(), while this module intentionally exposes
 * synchronous query helpers on the main thread. Until a bounded, export-bound
 * OPFS reuse path exists, decrypted database bytes remain memory-only.
 */

// Module state
let sqlite3 = null;
let db = null;
let isInitialized = false;
let initializationPromise = null;
let lifecycleGeneration = 0;

// sqlite3.h's stable sqlite3_deserialize() flags. The official JS wrapper
// exposes sqlite3_deserialize() but does not export these preprocessor macros.
const SQLITE_DESERIALIZE_FREEONCLOSE = 0x01;
const SQLITE_DESERIALIZE_READONLY = 0x04;
// sqlite3_deserialize() may read a small distance beyond N while validating a
// malformed image. SQLite's API contract recommends at least 20 spare bytes.
const SQLITE_DESERIALIZE_PADDING = 20;
// The viewer necessarily holds a JavaScript copy and a SQLite WASM copy. Keep
// this guard inside the database module so every caller, encrypted or not, is
// bounded even if config validation or a fetch-path check is bypassed.
const MAX_BROWSER_DATABASE_SIZE = 512 * 1024 * 1024;
const MAX_WASM32_ALLOCATION_SIZE = 0xFFFFFFFF;

function checkedDatabaseAllocationSize(byteLength) {
    if (!Number.isSafeInteger(byteLength) || byteLength <= 0) {
        throw new TypeError('Database payload must have a positive safe-integer byte length');
    }
    if (byteLength > MAX_BROWSER_DATABASE_SIZE) {
        throw new RangeError(
            `Database payload exceeds the ${MAX_BROWSER_DATABASE_SIZE}-byte browser limit`
        );
    }

    const allocationSize = byteLength + SQLITE_DESERIALIZE_PADDING;
    if (
        !Number.isSafeInteger(allocationSize)
        || allocationSize > MAX_WASM32_ALLOCATION_SIZE
    ) {
        throw new RangeError('Database payload plus SQLite safety padding exceeds wasm32 limits');
    }
    return allocationSize;
}

/**
 * Initialize sqlite-wasm with decrypted database bytes
 * Ownership of a valid Uint8Array transfers to this function. It is zeroized
 * on every return path and must not be read or modified by the caller again.
 * @param {Uint8Array} dbBytes - Owned decrypted database bytes
 * @returns {Promise<void>}
 */
export async function initDatabase(dbBytes) {
    if (!(dbBytes instanceof Uint8Array)) {
        throw new TypeError('Database payload must be a Uint8Array');
    }

    let pending = null;
    try {
        checkedDatabaseAllocationSize(dbBytes.byteLength);
        if (isInitialized) {
            console.warn('[DB] Already initialized');
            return;
        }
        if (initializationPromise) {
            throw new Error('Database initialization is already in progress');
        }

        console.log('[DB] Initializing sqlite-wasm...');
        const generation = ++lifecycleGeneration;
        pending = initializeDatabase(dbBytes, generation);
        initializationPromise = pending;
        await pending;
    } finally {
        try {
            dbBytes.fill(0);
        } catch {
            // A caller which violated the ownership contract may have detached
            // the view; never let cleanup obscure the initialization result.
        }
        if (pending && initializationPromise === pending) {
            initializationPromise = null;
        }
    }
}

async function initializeDatabase(dbBytes, generation) {
    const sqliteApi = sqlite3 || await loadSqliteWasm();
    if (generation !== lifecycleGeneration) {
        throw new Error('Database initialization was cancelled');
    }
    sqlite3 = sqliteApi;

    let candidateDb = null;
    let wasmPtr = 0;
    let sqliteOwnsBytes = false;
    try {
        candidateDb = new sqliteApi.oo1.DB();
        const allocationSize = checkedDatabaseAllocationSize(dbBytes.byteLength);
        wasmPtr = sqliteApi.wasm.alloc(allocationSize);
        const wasmOffset = Number(wasmPtr);
        const wasmHeap = sqliteApi.wasm.heap8u();
        wasmHeap.set(dbBytes, wasmOffset);
        wasmHeap.fill(
            0,
            wasmOffset + dbBytes.byteLength,
            wasmOffset + allocationSize
        );
        const flags = SQLITE_DESERIALIZE_FREEONCLOSE | SQLITE_DESERIALIZE_READONLY;
        const resultCode = sqliteApi.capi.sqlite3_deserialize(
            candidateDb.pointer,
            'main',
            wasmPtr,
            dbBytes.byteLength,
            allocationSize,
            flags
        );
        // Once the C call returns, FREEONCLOSE makes SQLite responsible for
        // this allocation on both success and error. Do not double-free it.
        sqliteOwnsBytes = true;
        candidateDb.checkRc(resultCode);

        if (generation !== lifecycleGeneration) {
            throw new Error('Database initialization was cancelled');
        }

        db = candidateDb;
        candidateDb = null;
        isInitialized = true;
        console.log('[DB] Loaded read-only database into memory');
    } catch (error) {
        // A JS wrapper failure before sqlite3_deserialize() reaches C leaves
        // ownership with us. A returned C result with FREEONCLOSE does not.
        if (wasmPtr && !sqliteOwnsBytes) {
            sqliteApi.wasm.dealloc(wasmPtr);
        }
        if (candidateDb) {
            try {
                candidateDb.close();
            } catch (closeError) {
                console.warn('[DB] Failed to close rejected database handle:', closeError);
            }
        }
        throw error;
    }

}

/**
 * Load sqlite-wasm module
 */
async function loadSqliteWasm() {
    try {
        const moduleUrl = new URL('./vendor/sqlite3.mjs', import.meta.url);
        const module = await import(moduleUrl.href);
        if (typeof module.default !== 'function') {
            throw new Error('sqlite-wasm module has no default initializer');
        }
        return await module.default({
            locateFile: (filename) => new URL(filename, moduleUrl).href,
        });
    } catch (error) {
        console.error('[DB] Failed to load sqlite-wasm:', error);
        throw new Error('SQLite runtime is unavailable or invalid.');
    }
}

/**
 * Execute query with automatic resource cleanup
 * Prevents memory leaks by ensuring statements are finalized.
 *
 * @param {string} sql - SQL query
 * @param {Array} params - Query parameters
 * @param {Function} callback - Callback to process statement
 * @returns {*} Result from callback
 */
export function withQuery(sql, params = [], callback) {
    if (!db) {
        throw new Error('Database not initialized');
    }

    const stmt = db.prepare(sql);
    try {
        if (params.length > 0) {
            stmt.bind(params);
        }
        return callback(stmt);
    } finally {
        stmt.finalize();
    }
}

/**
 * Execute query and return all results as objects
 * @param {string} sql - SQL query
 * @param {Array} params - Query parameters
 * @returns {Array<Object>} Array of row objects
 */
export function queryAll(sql, params = []) {
    return withQuery(sql, params, (stmt) => {
        const results = [];
        while (stmt.step()) {
            results.push(stmt.get({}));
        }
        return results;
    });
}

/**
 * Execute query and return first row as object
 * @param {string} sql - SQL query
 * @param {Array} params - Query parameters
 * @returns {Object|null} Row object or null
 */
export function queryOne(sql, params = []) {
    return withQuery(sql, params, (stmt) => {
        return stmt.step() ? stmt.get({}) : null;
    });
}

/**
 * Execute query and return single scalar value
 * @param {string} sql - SQL query
 * @param {Array} params - Query parameters
 * @returns {*} Scalar value or null
 */
export function queryValue(sql, params = []) {
    return withQuery(sql, params, (stmt) => {
        return stmt.step() ? stmt.get(0) : null;
    });
}

/**
 * Execute a statement (INSERT, UPDATE, DELETE)
 * @param {string} sql - SQL statement
 * @param {Array} params - Statement parameters
 * @returns {number} Number of affected rows
 */
export function execute(sql, params = []) {
    if (!db) {
        throw new Error('Database not initialized');
    }
    void sql;
    void params;
    throw new Error('Archive database is read-only');
}

// ============================================
// Pre-built Queries
// ============================================

/**
 * Get export metadata
 * @returns {Object} Metadata key-value pairs
 */
export function getExportMeta() {
    try {
        const rows = queryAll('SELECT key, value FROM export_meta');
        return Object.fromEntries(rows.map(r => [r.key, r.value]));
    } catch {
        return {};
    }
}

/**
 * Get archive statistics
 * @returns {Object} Statistics object
 */
export function getStatistics() {
    return {
        conversations: queryValue('SELECT COUNT(*) FROM conversations') || 0,
        messages: queryValue('SELECT COUNT(*) FROM messages') || 0,
        agents: queryAll('SELECT DISTINCT agent FROM conversations').map(r => r.agent),
        workspaces: queryAll('SELECT DISTINCT workspace FROM conversations WHERE workspace IS NOT NULL').map(r => r.workspace),
    };
}

/**
 * Get recent conversations
 * @param {number} limit - Maximum number of conversations
 * @returns {Array<Object>} Conversation objects
 */
export function getRecentConversations(limit = 50) {
    return queryAll(`
        SELECT id, agent, workspace, title, source_path, started_at, ended_at, message_count
        FROM conversations
        ORDER BY started_at DESC
        LIMIT ?
    `, [limit]);
}

/**
 * Get conversation by ID
 * @param {number} convId - Conversation ID
 * @returns {Object|null} Conversation object
 */
export function getConversation(convId) {
    return queryOne(`
        SELECT id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json
        FROM conversations
        WHERE id = ?
    `, [convId]);
}

/**
 * Get messages for a conversation
 * @param {number} convId - Conversation ID
 * @returns {Array<Object>} Message objects
 */
export function getConversationMessages(convId) {
    return queryAll(`
        SELECT id, idx, role, content, created_at, updated_at, model
        FROM messages
        WHERE conversation_id = ?
        ORDER BY idx ASC
    `, [convId]);
}

/**
 * Search mode for FTS5 query routing
 * @typedef {'auto' | 'prose' | 'code'} SearchMode
 */

/**
 * Detect if query looks like code (for FTS table routing)
 *
 * Checks for code patterns:
 * - Underscores (snake_case)
 * - Dots (file extensions, method calls)
 * - Path separators (/ or \)
 * - Namespaces (::)
 * - Special chars (#, @, $, %)
 * - camelCase (lowercase followed by uppercase)
 * - kebab-case (letter-hyphen-letter)
 *
 * Also checks for prose indicators to reduce false positives:
 * - Question words (how, what, why, when, where)
 * - Common articles (the, is, are, was, were)
 * - Multiple words (>3 space-separated words)
 *
 * @param {string} query - Search query
 * @returns {boolean} True if query contains code patterns
 */
function isCodeQuery(query) {
    // Check for code-like characters
    const hasCodeChars =
        query.includes('_') ||
        query.includes('.') ||
        query.includes('/') ||
        query.includes('\\') ||
        query.includes('::') ||
        query.includes('#') ||
        query.includes('@') ||
        query.includes('$') ||
        query.includes('%');

    // Check for camelCase (lowercase followed by uppercase)
    const hasCamelCase = /[a-z][A-Z]/.test(query);

    // Check for kebab-case (letter-hyphen-letter)
    const hasKebabCase = /[a-zA-Z]-[a-zA-Z]/.test(query);

    const isCode = hasCodeChars || hasCamelCase || hasKebabCase;

    // Check for prose indicators
    const words = query.trim().split(/\s+/);
    const wordCount = words.length;
    const lower = query.toLowerCase();

    const hasProseIndicators =
        wordCount > 3 ||
        lower.startsWith('how ') ||
        lower.startsWith('what ') ||
        lower.startsWith('why ') ||
        lower.startsWith('when ') ||
        lower.startsWith('where ') ||
        lower.includes(' the ') ||
        lower.includes(' is ') ||
        lower.includes(' are ') ||
        lower.includes(' was ') ||
        lower.includes(' were ');

    // Code patterns win unless prose indicators are strong
    if (isCode && !hasProseIndicators) {
        return true;
    }
    if (hasProseIndicators && !isCode) {
        return false;
    }
    if (isCode) {
        // Both indicators present - code chars are more specific
        return true;
    }
    return false;
}

/**
 * Escape query for FTS5 MATCH
 * Wraps each term in double-quotes and escapes internal quotes
 * @param {string} query - Search query
 * @returns {string} Escaped query safe for FTS5
 */
function escapeFts5Query(query) {
    return query
        .split(/\s+/)
        .filter(t => t.length > 0)
        .map(t => `"${t.replace(/"/g, '""')}"`)
        .join(' ');
}

function normalizeTimestampFilterValue(value) {
    if (value === undefined || value === null || value === '') {
        return null;
    }

    const numeric = Number(value);
    if (!Number.isFinite(numeric) || numeric < 0 || !Number.isSafeInteger(numeric)) {
        return null;
    }

    return numeric;
}

/**
 * Search conversations using FTS5
 * Automatically routes to the appropriate FTS table:
 * - messages_fts (porter stemmer) for natural language
 * - messages_code_fts (unicode61) for code identifiers/paths
 *
 * @param {string} query - Search query
 * @param {Object} options - Search options
 * @param {number} [options.limit=50] - Maximum results
 * @param {number} [options.offset=0] - Result offset for pagination
 * @param {string|null} [options.agent=null] - Filter by agent name
 * @param {SearchMode} [options.searchMode='auto'] - Search mode: 'auto', 'prose', or 'code'
 * @param {number|string|null} [options.since=null] - Earliest conversation start timestamp (ms)
 * @param {number|string|null} [options.until=null] - Latest conversation start timestamp (ms)
 * @returns {Array<Object>} Search results
 */
export function searchConversations(query, options = {}) {
    const { limit = 50, offset = 0, agent = null, searchMode = 'auto', since = null, until = null } = options;

    // Escape query for FTS5
    const escapedQuery = escapeFts5Query(query);
    if (!escapedQuery) {
        return [];
    }

    // Route to appropriate FTS table based on search mode
    let ftsTable;
    if (searchMode === 'code') {
        ftsTable = 'messages_code_fts';
    } else if (searchMode === 'prose') {
        ftsTable = 'messages_fts';
    } else {
        // Auto mode - detect based on query content
        ftsTable = isCodeQuery(query) ? 'messages_code_fts' : 'messages_fts';
    }

    let sql = `
        SELECT
            m.conversation_id,
            m.id as message_id,
            m.role,
            snippet(${ftsTable}, 0, '<mark>', '</mark>', '...', 32) as snippet,
            c.agent,
            c.workspace,
            c.title,
            c.started_at,
            bm25(${ftsTable}) as score
        FROM ${ftsTable}
        JOIN messages m ON ${ftsTable}.rowid = m.id
        JOIN conversations c ON m.conversation_id = c.id
        WHERE ${ftsTable} MATCH ?
    `;

    const params = [escapedQuery];

    if (agent) {
        sql += ' AND c.agent = ?';
        params.push(agent);
    }

    const sinceTimestamp = normalizeTimestampFilterValue(since);
    if (sinceTimestamp !== null) {
        sql += ' AND c.started_at >= ?';
        params.push(sinceTimestamp);
    }

    const untilTimestamp = normalizeTimestampFilterValue(until);
    if (untilTimestamp !== null) {
        sql += ' AND c.started_at <= ?';
        params.push(untilTimestamp);
    }

    sql += `
        ORDER BY score
        LIMIT ? OFFSET ?
    `;
    params.push(limit, offset);

    try {
        return queryAll(sql, params);
    } catch (error) {
        console.error('[DB] Search error:', error);
        return [];
    }
}

/**
 * Get conversations by agent
 * @param {string} agent - Agent name
 * @param {number} limit - Maximum results
 * @param {number|string|null} since - Earliest conversation start timestamp (ms)
 * @param {number|string|null} until - Latest conversation start timestamp (ms)
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByAgent(agent, limit = 50, since = null, until = null) {
    let sql = `
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE agent = ?
    `;
    const params = [agent];

    const sinceTimestamp = normalizeTimestampFilterValue(since);
    if (sinceTimestamp !== null) {
        sql += ' AND started_at >= ?';
        params.push(sinceTimestamp);
    }

    const untilTimestamp = normalizeTimestampFilterValue(until);
    if (untilTimestamp !== null) {
        sql += ' AND started_at <= ?';
        params.push(untilTimestamp);
    }

    sql += `
        ORDER BY started_at DESC
        LIMIT ?
    `;
    params.push(limit);

    return queryAll(sql, params);
}

/**
 * Get conversations by workspace
 * @param {string} workspace - Workspace path
 * @param {number} limit - Maximum results
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByWorkspace(workspace, limit = 50) {
    return queryAll(`
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE workspace = ?
        ORDER BY started_at DESC
        LIMIT ?
    `, [workspace, limit]);
}

/**
 * Get conversations by time range
 * @param {number} since - Start timestamp (ms)
 * @param {number} until - End timestamp (ms)
 * @param {number} limit - Maximum results
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByTimeRange(since, until, limit = 50) {
    return queryAll(`
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE started_at >= ? AND started_at <= ?
        ORDER BY started_at DESC
        LIMIT ?
    `, [since, until, limit]);
}

// ============================================
// Memory Management
// ============================================

/**
 * Get WASM memory usage
 * @returns {Object|null} Memory usage info
 */
export function getMemoryUsage() {
    if (typeof sqlite3?.wasm?.heap8u !== 'function') {
        return null;
    }

    const heap = sqlite3.wasm.heap8u();
    const limit = 256 * 1024 * 1024; // 256MB typical WASM limit

    return {
        used: heap.length,
        limit: limit,
        percent: (heap.length / limit) * 100,
    };
}

/**
 * Check for memory pressure
 * @returns {boolean} True if memory usage is high
 */
export function checkMemoryPressure() {
    const usage = getMemoryUsage();
    if (usage && usage.percent > 80) {
        console.warn(`[DB] WASM memory at ${usage.percent.toFixed(1)}%`);
        return true;
    }
    return false;
}

/**
 * Close the database connection
 */
export function closeDatabase() {
    lifecycleGeneration += 1;
    if (db) {
        try {
            db.close();
            console.log('[DB] Closed');
        } catch (error) {
            console.warn('[DB] Close failed, resetting handle anyway:', error);
        } finally {
            db = null;
        }
    }
    isInitialized = false;
}

/**
 * Check if database is initialized
 * @returns {boolean}
 */
export function isDatabaseReady() {
    return isInitialized;
}

/**
 * Detect which search mode would be used for a query
 * Useful for showing the user which FTS table will be used
 *
 * @param {string} query - Search query
 * @returns {'prose' | 'code'} Detected search mode
 */
export function detectSearchMode(query) {
    return isCodeQuery(query) ? 'code' : 'prose';
}

// Export default instance
export default {
    initDatabase,
    queryAll,
    queryOne,
    queryValue,
    execute,
    withQuery,
    getExportMeta,
    getStatistics,
    getRecentConversations,
    getConversation,
    getConversationMessages,
    searchConversations,
    detectSearchMode,
    getConversationsByAgent,
    getConversationsByWorkspace,
    getConversationsByTimeRange,
    getMemoryUsage,
    checkMemoryPressure,
    closeDatabase,
    isDatabaseReady,
};
