/**
 * cass Archive Database Module
 *
 * sqlite-wasm integration for browser-based database queries.
 * Uses OPFS for persistence when available, falls back to in-memory.
 */

// Module state
let sqlite3 = null;
let db = null;
let isInitialized = false;

/**
 * Initialize sqlite-wasm with decrypted database bytes
 * @param {Uint8Array} dbBytes - Decrypted database bytes
 * @returns {Promise<void>}
 */
export async function initDatabase(dbBytes) {
    if (isInitialized) {
        console.warn('[DB] Already initialized');
        return;
    }

    console.log('[DB] Initializing sqlite-wasm...');

    // Load sqlite-wasm module
    sqlite3 = await loadSqliteWasm();

    // Try OPFS first (better performance, persists in cache)
    if (sqlite3.oo1.OpfsDb && navigator.storage?.getDirectory) {
        try {
            await writeBytesToOPFS(dbBytes);
            db = new sqlite3.oo1.OpfsDb('/cass-archive.sqlite3');
            console.log('[DB] Loaded from OPFS');
            isInitialized = true;
            return;
        } catch (error) {
            console.warn('[DB] OPFS unavailable, using in-memory:', error.message);
        }
    }

    // Fallback: in-memory database
    db = new sqlite3.oo1.DB();

    // Deserialize database bytes
    const ptr = sqlite3.wasm.allocFromTypedArray(dbBytes);
    try {
        db.deserialize(ptr, dbBytes.length);
        console.log('[DB] Loaded into memory');
    } finally {
        sqlite3.wasm.dealloc(ptr);
    }

    isInitialized = true;
}

/**
 * Load sqlite-wasm module
 */
async function loadSqliteWasm() {
    try {
        // Dynamic import from vendor folder
        const module = await import('./vendor/sqlite3.js');
        return await module.default();
    } catch (error) {
        console.error('[DB] Failed to load sqlite-wasm:', error);
        throw new Error('SQLite library not available. Ensure sqlite3.js is in the vendor folder.');
    }
}

/**
 * Write database bytes to OPFS
 */
async function writeBytesToOPFS(bytes) {
    const root = await navigator.storage.getDirectory();
    const handle = await root.getFileHandle('cass-archive.sqlite3', { create: true });
    const writable = await handle.createWritable();
    await writable.write(bytes);
    await writable.close();
}

/**
 * Execute query with automatic resource cleanup
 * Prevents memory leaks by ensuring statements are freed.
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
        stmt.free(); // Critical: free WASM memory
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
            results.push(stmt.getAsObject());
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
        return stmt.step() ? stmt.getAsObject() : null;
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
        return stmt.step() ? stmt.get()[0] : null;
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

    db.exec(sql, { bind: params });
    return db.changes();
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
 * @returns {Array<Object>} Search results
 */
export function searchConversations(query, options = {}) {
    const { limit = 50, offset = 0, agent = null, searchMode = 'auto' } = options;

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
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByAgent(agent, limit = 50) {
    return queryAll(`
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE agent = ?
        ORDER BY started_at DESC
        LIMIT ?
    `, [agent, limit]);
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
    if (!sqlite3?.wasm?.HEAPU8) {
        return null;
    }

    const heap = sqlite3.wasm.HEAPU8;
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
    if (db) {
        db.close();
        db = null;
        isInitialized = false;
        console.log('[DB] Closed');
    }
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
