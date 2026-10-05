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
const MAX_WASM32_ALLOCATION_SIZE = 0xffffffff;
// Bound row hydration independently of callers. In SQLite a negative LIMIT
// means unlimited, so invalid input must never reach a prepared statement.
const MAX_QUERY_PAGE_SIZE = 1000;

function validatePagination(limit, offset) {
  if (!Number.isSafeInteger(limit) || limit < 0 || limit > MAX_QUERY_PAGE_SIZE) {
    throw new RangeError(`Query limit must be an integer between 0 and ${MAX_QUERY_PAGE_SIZE}`);
  }
  if (!Number.isSafeInteger(offset) || offset < 0) {
    throw new RangeError("Query offset must be a non-negative safe integer");
  }
}

function checkedDatabaseAllocationSize(byteLength) {
  if (!Number.isSafeInteger(byteLength) || byteLength <= 0) {
    throw new TypeError("Database payload must have a positive safe-integer byte length");
  }
  if (byteLength > MAX_BROWSER_DATABASE_SIZE) {
    throw new RangeError(
      `Database payload exceeds the ${MAX_BROWSER_DATABASE_SIZE}-byte browser limit`,
    );
  }

  const allocationSize = byteLength + SQLITE_DESERIALIZE_PADDING;
  if (!Number.isSafeInteger(allocationSize) || allocationSize > MAX_WASM32_ALLOCATION_SIZE) {
    throw new RangeError("Database payload plus SQLite safety padding exceeds wasm32 limits");
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
    throw new TypeError("Database payload must be a Uint8Array");
  }

  let pending = null;
  try {
    checkedDatabaseAllocationSize(dbBytes.byteLength);
    if (isInitialized) {
      console.warn("[DB] Already initialized");
      return;
    }
    if (initializationPromise) {
      throw new Error("Database initialization is already in progress");
    }

    console.log("[DB] Initializing sqlite-wasm...");
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
  const sqliteApi = sqlite3 || (await loadSqliteWasm());
  if (generation !== lifecycleGeneration) {
    throw new Error("Database initialization was cancelled");
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
    wasmHeap.fill(0, wasmOffset + dbBytes.byteLength, wasmOffset + allocationSize);
    const flags = SQLITE_DESERIALIZE_FREEONCLOSE | SQLITE_DESERIALIZE_READONLY;
    const resultCode = sqliteApi.capi.sqlite3_deserialize(
      candidateDb.pointer,
      "main",
      wasmPtr,
      dbBytes.byteLength,
      allocationSize,
      flags,
    );
    // Once the C call returns, FREEONCLOSE makes SQLite responsible for
    // this allocation on both success and error. Do not double-free it.
    sqliteOwnsBytes = true;
    candidateDb.checkRc(resultCode);

    if (generation !== lifecycleGeneration) {
      throw new Error("Database initialization was cancelled");
    }

    db = candidateDb;
    candidateDb = null;
    isInitialized = true;
    console.log("[DB] Loaded read-only database into memory");
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
        console.warn("[DB] Failed to close rejected database handle:", closeError);
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
    const moduleUrl = new URL("./vendor/sqlite3.mjs", import.meta.url);
    const module = await import(moduleUrl.href);
    if (typeof module.default !== "function") {
      throw new Error("sqlite-wasm module has no default initializer");
    }
    return await module.default({
      locateFile: (filename) => new URL(filename, moduleUrl).href,
    });
  } catch (error) {
    console.error("[DB] Failed to load sqlite-wasm:", error);
    throw new Error("SQLite runtime is unavailable or invalid.");
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
    throw new Error("Database not initialized");
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
    throw new Error("Database not initialized");
  }
  void sql;
  void params;
  throw new Error("Archive database is read-only");
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
    const rows = queryAll("SELECT key, value FROM export_meta");
    return Object.fromEntries(rows.map((r) => [r.key, r.value]));
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
    conversations: queryValue("SELECT COUNT(*) FROM conversations") || 0,
    messages: queryValue("SELECT COUNT(*) FROM messages") || 0,
    agents: queryAll("SELECT DISTINCT agent FROM conversations").map((r) => r.agent),
    workspaces: queryAll(
      "SELECT DISTINCT workspace FROM conversations WHERE workspace IS NOT NULL",
    ).map((r) => r.workspace),
  };
}

/**
 * Get recent conversations
 * @param {number} limit - Maximum number of conversations (0-1000)
 * @param {number} offset - Number of conversations to skip
 * @returns {Array<Object>} Conversation objects
 */
export function getRecentConversations(limit = 50, offset = 0) {
  validatePagination(limit, offset);
  return queryAll(
    `
        SELECT id, agent, workspace, title, source_path, started_at, ended_at, message_count
        FROM conversations
        ORDER BY started_at DESC, id DESC
        LIMIT ? OFFSET ?
    `,
    [limit, offset],
  );
}

/**
 * Get conversation by ID
 * @param {number} convId - Conversation ID
 * @returns {Object|null} Conversation object
 */
export function getConversation(convId) {
  return queryOne(
    `
        SELECT id, agent, workspace, title, source_path, started_at, ended_at, message_count, metadata_json
        FROM conversations
        WHERE id = ?
    `,
    [convId],
  );
}

/**
 * Get messages for a conversation
 * @param {number} convId - Conversation ID
 * @returns {Array<Object>} Message objects
 */
export function getConversationMessages(convId) {
  return queryAll(
    `
        SELECT id, idx, role, content, created_at, updated_at, model
        FROM messages
        WHERE conversation_id = ?
        ORDER BY idx ASC
    `,
    [convId],
  );
}

/**
 * Read-through transcript source for the virtual viewer.
 *
 * Construction reads only a count. Scrolling loads small ID windows and fetches
 * message bodies by primary key, rather than hydrating the whole conversation.
 * Each source retains at most four 50-ID windows and 64 bodies, subject to a
 * 2M UTF-16-code-unit text budget. Oversized individual messages are returned
 * but not cached. This bounds retained text, not SQLite/WASM, DOM or clipboard
 * memory: displaying/copying one huge message can still allocate its full text.
 */
export class ConversationMessageSource {
  #conversationId;
  #generation;
  #length;
  #idPages = new Map();
  #messages = new Map();
  #textUnits = 0;
  #disposed = false;

  constructor(conversationId) {
    if (!Number.isSafeInteger(conversationId) || conversationId <= 0) {
      throw new TypeError("Conversation ID must be a positive safe integer");
    }
    this.#conversationId = conversationId;
    this.#generation = lifecycleGeneration;
    this.#length = Number(
      queryValue("SELECT COUNT(*) FROM messages WHERE conversation_id = ?", [conversationId]),
    );
    if (!Number.isSafeInteger(this.#length) || this.#length < 0) {
      throw new Error("Invalid conversation message count");
    }
  }

  get length() {
    this.#ensureActive();
    return this.#length;
  }

  #ensureActive() {
    if (this.#disposed || this.#generation !== lifecycleGeneration || !db) {
      this.dispose();
      throw new Error("Conversation message source is no longer active");
    }
  }

  get(index) {
    this.#ensureActive();
    if (!Number.isSafeInteger(index) || index < 0 || index >= this.#length) {
      return undefined;
    }
    const page = Math.floor(index / 50);
    let ids = this.#idPages.get(page);
    if (ids) {
      this.#idPages.delete(page);
    } else {
      ids = queryAll(
        `
                SELECT id FROM messages WHERE conversation_id = ?
                ORDER BY idx ASC, id ASC LIMIT ? OFFSET ?
            `,
        [this.#conversationId, 50, page * 50],
      ).map((row) => row.id);
      if (ids.some((id) => !Number.isSafeInteger(id) || id <= 0)) {
        throw new Error("Invalid message ID in archive");
      }
    }
    this.#idPages.set(page, ids);
    if (this.#idPages.size > 4) {
      this.#idPages.delete(this.#idPages.keys().next().value);
    }

    const id = ids[index % 50];
    if (id === undefined) {
      throw new Error("Conversation changed while reading messages");
    }
    const cached = this.#messages.get(id);
    if (cached) {
      this.#messages.delete(id);
      this.#messages.set(id, cached);
      return cached.row;
    }

    const row = queryOne(
      `
            SELECT id, idx, role, content, created_at, updated_at, model
            FROM messages WHERE conversation_id = ? AND id = ?
        `,
      [this.#conversationId, id],
    );
    if (!row) {
      throw new Error("Message is missing from the archive");
    }
    Object.freeze(row);
    const units = Object.values(row).reduce(
      (sum, value) => sum + (typeof value === "string" ? value.length : 0),
      0,
    );
    const budget = 2 * 1024 * 1024;
    if (units <= budget) {
      while (this.#messages.size >= 64 || this.#textUnits + units > budget) {
        const oldest = this.#messages.keys().next().value;
        this.#textUnits -= this.#messages.get(oldest).units;
        this.#messages.delete(oldest);
      }
      this.#messages.set(id, { row, units });
      this.#textUnits += units;
    }
    return row;
  }

  /** Locate a deep-linked message without reading any message bodies. */
  indexOfId(messageId) {
    this.#ensureActive();
    if (!Number.isSafeInteger(messageId) || messageId <= 0) {
      return -1;
    }
    const target = queryOne("SELECT idx FROM messages WHERE conversation_id = ? AND id = ?", [
      this.#conversationId,
      messageId,
    ]);
    if (!target) {
      return -1;
    }
    // Match SQLite's NULL-first ascending order and break idx ties by ID.
    const condition =
      target.idx === null
        ? "idx IS NULL AND id < ?"
        : "(idx IS NULL OR idx < ? OR (idx = ? AND id < ?))";
    const params =
      target.idx === null
        ? [this.#conversationId, messageId]
        : [this.#conversationId, target.idx, target.idx, messageId];
    return Number(
      queryValue(
        `SELECT COUNT(*) FROM messages WHERE conversation_id = ? AND (${condition})`,
        params,
      ),
    );
  }

  // Eager traversal is reserved for short direct-rendered conversations and
  // an explicit full-conversation Copy action, never initial virtual loading.
  forEach(callback) {
    this.#ensureActive();
    for (let index = 0; index < this.#length; index += 1) {
      callback(this.get(index), index, this);
    }
  }

  map(callback) {
    const result = [];
    this.forEach((message, index) => result.push(callback(message, index, this)));
    return result;
  }

  clearCache() {
    this.#idPages.clear();
    this.#messages.clear();
    this.#textUnits = 0;
  }

  dispose() {
    this.clearCache();
    this.#disposed = true;
    this.#length = 0;
  }

  getCacheStats() {
    return {
      idPages: this.#idPages.size,
      messages: this.#messages.size,
      textUnits: this.#textUnits,
    };
  }
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
    query.includes("_") ||
    query.includes(".") ||
    query.includes("/") ||
    query.includes("\\") ||
    query.includes("::") ||
    query.includes("#") ||
    query.includes("@") ||
    query.includes("$") ||
    query.includes("%");

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
    lower.startsWith("how ") ||
    lower.startsWith("what ") ||
    lower.startsWith("why ") ||
    lower.startsWith("when ") ||
    lower.startsWith("where ") ||
    lower.includes(" the ") ||
    lower.includes(" is ") ||
    lower.includes(" are ") ||
    lower.includes(" was ") ||
    lower.includes(" were ");

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
 * Compile the viewer's query language to a bound FTS5 MATCH expression.
 *
 * Bare terms are ANDed; quotes preserve a phrase and a trailing * requests a
 * token prefix. Uppercase AND/OR and binary NOT support grouping with
 * parentheses (NOT > AND > OR). `a AND NOT b` is also accepted. Standalone
 * negation has no positive FTS candidate set and is rejected, not dropped.
 * Quote operator words to search them literally; double a quote inside a
 * quoted phrase. Other punctuation, paths and column-looking text stay data,
 * never raw FTS syntax. Attached call parentheses such as foo() stay literal.
 *
 * The byte-independent input, token and nesting limits bound parser work on
 * pasted queries. Invalid syntax is surfaced through the UI's search error
 * path instead of silently broadening the search or pretending there are no
 * matches. No term is ever interpolated into SQL.
 * @param {string} query - Search query
 * @returns {string} Escaped, precedence-preserving expression safe for FTS5
 */
function escapeFts5Query(query) {
  if (typeof query !== "string") {
    throw new TypeError("Search query must be a string");
  }
  if (query.length > 4096) {
    throw new RangeError("Search query exceeds the 4096-character limit");
  }

  const tokens = [];
  const operators = new Set(["AND", "OR", "NOT"]);
  const whitespace = /\s/;
  let position = 0;
  while (position < query.length) {
    const character = query[position];
    if (whitespace.test(character)) {
      position += 1;
      continue;
    }
    if (tokens.length >= 128) {
      throw new RangeError("Search query exceeds the 128-token limit");
    }
    if (character === "(" || character === ")") {
      tokens.push({ kind: character });
      position += 1;
      continue;
    }

    let text = "";
    let quoted = false;
    let prefix = false;
    if (character === '"') {
      quoted = true;
      position += 1;
      let closed = false;
      while (position < query.length) {
        const next = query[position++];
        if (next !== '"') {
          text += next;
        } else if (query[position] === '"') {
          text += '"';
          position += 1;
        } else {
          closed = true;
          break;
        }
      }
      if (!closed) {
        throw new SyntaxError("Unterminated quoted phrase");
      }
      if (!text.trim()) {
        throw new SyntaxError("Quoted search phrases must not be empty");
      }
      if (query[position] === "*") {
        prefix = true;
        position += 1;
      }
      if (position < query.length && !whitespace.test(query[position]) && query[position] !== ")") {
        throw new SyntaxError("Separate a quoted phrase from the next term with a space");
      }
    } else {
      let attachedDepth = 0;
      while (position < query.length && !whitespace.test(query[position])) {
        const next = query[position];
        if (next === '"') {
          // Quotes inside a bare code token are literal, as they were before
          // phrase support; only a leading quote opens a phrase.
          text += next;
        } else if (next === "(") {
          if (operators.has(text)) {
            break;
          }
          attachedDepth += 1;
          text += next;
        } else if (next === ")") {
          if (attachedDepth === 0) {
            break;
          }
          attachedDepth -= 1;
          text += next;
        } else {
          text += next;
        }
        position += 1;
      }
      if (text.endsWith("*")) {
        prefix = true;
        text = text.slice(0, -1);
        if (!text || text.includes("*")) {
          throw new SyntaxError("A prefix search needs a term followed by a single *");
        }
      }
    }
    tokens.push(
      !quoted && !prefix && operators.has(text)
        ? { kind: text }
        : { kind: "term", value: `"${text.replace(/"/g, '""')}"${prefix ? "*" : ""}` },
    );
  }
  if (tokens.length === 0) {
    return "";
  }

  let cursor = 0;
  const peek = () => tokens[cursor]?.kind;
  // Render only necessary grouping. Wrapping each link in a long AND chain
  // would overflow FTS5's parser stack even within our token budget.
  const combine = (left, operator, right) => {
    const precedence = { OR: 1, AND: 2, NOT: 3 }[operator];
    const lhs = left.precedence < precedence ? `(${left.value})` : left.value;
    const rhs = right.precedence < precedence ||
      (operator === "NOT" && right.precedence === precedence)
      ? `(${right.value})` : right.value;
    return { value: `${lhs} ${operator} ${rhs}`, precedence };
  };

  function primary(depth) {
    const token = tokens[cursor++];
    if (token?.kind === "term") {
      return { value: token.value, precedence: 4 };
    }
    if (token?.kind === "(") {
      if (depth >= 16) {
        throw new RangeError("Search groups exceed the 16-level nesting limit");
      }
      const expression = disjunction(depth + 1);
      if (peek() !== ")") {
        throw new SyntaxError("Missing closing parenthesis in search query");
      }
      cursor += 1;
      return expression;
    }
    if (token?.kind === "NOT") {
      throw new SyntaxError("NOT needs a positive term on its left, such as error NOT timeout");
    }
    throw new SyntaxError("Expected a search term or quoted phrase");
  }

  function exclusion(depth) {
    let expression = primary(depth);
    while (peek() === "NOT" || (peek() === "AND" && tokens[cursor + 1]?.kind === "NOT")) {
      cursor += peek() === "AND" ? 2 : 1;
      expression = combine(expression, "NOT", primary(depth));
    }
    return expression;
  }

  function conjunction(depth) {
    let expression = exclusion(depth);
    while (peek() === "AND" || peek() === "term" || peek() === "(") {
      if (peek() === "AND") {
        cursor += 1;
      }
      expression = combine(expression, "AND", exclusion(depth));
    }
    return expression;
  }

  function disjunction(depth) {
    let expression = conjunction(depth);
    while (peek() === "OR") {
      cursor += 1;
      expression = combine(expression, "OR", conjunction(depth));
    }
    return expression;
  }

  const expression = disjunction(0);
  if (cursor !== tokens.length) {
    throw new SyntaxError("Unexpected closing parenthesis in search query");
  }
  return expression.value;
}

function normalizeTimestampFilterValue(value, bound) {
  if (value === undefined || value === null || value === "") {
    return null;
  }

  const invalid = () => new RangeError(
    `${bound} must be a non-negative integer timestamp in milliseconds or a valid YYYY-MM-DD date`,
  );
  let numeric;
  if (typeof value === "number") {
    numeric = value;
  } else if (typeof value === "string") {
    const text = value.trim();
    if (/^\d{4}-\d{2}-\d{2}$/.test(text)) {
      // Date.parse can normalize impossible dates (for example February 30).
      // Check the round trip, and use UTC so links do not change across zones.
      numeric = Date.parse(`${text}T00:00:00.000Z`);
      if (!Number.isFinite(numeric) || new Date(numeric).toISOString().slice(0, 10) !== text) {
        throw invalid();
      }
      if (bound === "until") {
        numeric += 24 * 60 * 60 * 1000 - 1;
      }
    } else if (/^\d+$/.test(text)) {
      numeric = Number(text);
    } else {
      throw invalid();
    }
  } else {
    // Number(true), Number([]), etc. are not timestamp validation.
    throw invalid();
  }
  if (!Number.isSafeInteger(numeric) || numeric < 0) {
    throw invalid();
  }
  return numeric;
}

/**
 * Validate optional, inclusive archive time bounds without silently dropping
 * a supplied filter. Numeric strings are decimal milliseconds. Date-only
 * strings mean the beginning of the UTC day for since, and its end for until.
 * Shared by direct database queries and browser search routes.
 * @returns {{since: number|null, until: number|null}}
 */
export function normalizeSearchTimeRange(since = null, until = null) {
  const start = normalizeTimestampFilterValue(since, "since");
  const end = normalizeTimestampFilterValue(until, "until");
  if (start !== null && end !== null && start > end) {
    throw new RangeError("since must not be later than until");
  }
  return { since: start, until: end };
}

/**
 * Search conversations using FTS5
 * Automatically routes to the appropriate FTS table:
 * - messages_fts (porter stemmer) for natural language
 * - messages_code_fts (unicode61) for code identifiers/paths
 *
 * @param {string} query - Search query
 * @param {Object} options - Search options
 * @param {number} [options.limit=50] - Maximum results (0-1000)
 * @param {number} [options.offset=0] - Result offset for pagination
 * @param {string|null} [options.agent=null] - Filter by agent name
 * @param {SearchMode} [options.searchMode='auto'] - Search mode: 'auto', 'prose', or 'code'
 * @param {number|string|null} [options.since=null] - Earliest conversation start timestamp (ms or YYYY-MM-DD)
 * @param {number|string|null} [options.until=null] - Latest conversation start timestamp (ms or YYYY-MM-DD)
 * @returns {Array<Object>} Search results
 */
export function searchConversations(query, options = {}) {
  const {
    limit = 50,
    offset = 0,
    agent = null,
    searchMode = "auto",
    since = null,
    until = null,
  } = options;
  validatePagination(limit, offset);
  const { since: sinceTimestamp, until: untilTimestamp } = normalizeSearchTimeRange(since, until);

  // Parse and escape the query before preparing any database statement.
  const escapedQuery = escapeFts5Query(query);
  if (!escapedQuery) {
    return [];
  }

  // Route to appropriate FTS table based on search mode
  let ftsTable;
  if (searchMode === "code") {
    ftsTable = "messages_code_fts";
  } else if (searchMode === "prose") {
    ftsTable = "messages_fts";
  } else {
    // Auto mode - detect based on query content
    ftsTable = isCodeQuery(query) ? "messages_code_fts" : "messages_fts";
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
    sql += " AND c.agent = ?";
    params.push(agent);
  }

  if (sinceTimestamp !== null) {
    sql += " AND c.started_at >= ?";
    params.push(sinceTimestamp);
  }

  if (untilTimestamp !== null) {
    sql += " AND c.started_at <= ?";
    params.push(untilTimestamp);
  }

  sql += `
        ORDER BY score, m.id ASC
        LIMIT ? OFFSET ?
    `;
  params.push(limit, offset);

  // Let the UI distinguish a failed query from a successful empty result.
  return queryAll(sql, params);
}

/**
 * Get conversations by agent
 * @param {string} agent - Agent name
 * @param {number} limit - Maximum results
 * @param {number|string|null} since - Earliest conversation start timestamp (ms or YYYY-MM-DD)
 * @param {number|string|null} until - Latest conversation start timestamp (ms or YYYY-MM-DD)
 * @param {number} offset - Number of matching conversations to skip
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByAgent(agent, limit = 50, since = null, until = null, offset = 0) {
  validatePagination(limit, offset);
  const { since: sinceTimestamp, until: untilTimestamp } = normalizeSearchTimeRange(since, until);
  let sql = `
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE agent = ?
    `;
  const params = [agent];

  if (sinceTimestamp !== null) {
    sql += " AND started_at >= ?";
    params.push(sinceTimestamp);
  }

  if (untilTimestamp !== null) {
    sql += " AND started_at <= ?";
    params.push(untilTimestamp);
  }

  sql += `
        ORDER BY started_at DESC, id DESC
        LIMIT ? OFFSET ?
    `;
  params.push(limit, offset);

  return queryAll(sql, params);
}

/**
 * Get conversations by workspace
 * @param {string} workspace - Workspace path
 * @param {number} limit - Maximum results (0-1000)
 * @param {number} offset - Number of matching conversations to skip
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByWorkspace(workspace, limit = 50, offset = 0) {
  validatePagination(limit, offset);
  return queryAll(
    `
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE workspace = ?
        ORDER BY started_at DESC, id DESC
        LIMIT ? OFFSET ?
    `,
    [workspace, limit, offset],
  );
}

/**
 * Get conversations by time range (either bound may be omitted)
 * @param {number|string|null} since - Start timestamp (ms or YYYY-MM-DD)
 * @param {number} limit - Maximum results (0-1000)
 * @param {number} offset - Number of matching conversations to skip
 * @returns {Array<Object>} Conversation objects
 */
export function getConversationsByTimeRange(since, until, limit = 50, offset = 0) {
  validatePagination(limit, offset);
  const { since: sinceTimestamp, until: untilTimestamp } = normalizeSearchTimeRange(since, until);
  let sql = `
        SELECT id, agent, workspace, title, source_path, started_at, message_count
        FROM conversations
        WHERE 1 = 1
    `;
  const params = [];
  if (sinceTimestamp !== null) {
    sql += " AND started_at >= ?";
    params.push(sinceTimestamp);
  }
  if (untilTimestamp !== null) {
    sql += " AND started_at <= ?";
    params.push(untilTimestamp);
  }
  sql += `
        ORDER BY started_at DESC, id DESC
        LIMIT ? OFFSET ?
    `;
  params.push(limit, offset);
  return queryAll(sql, params);
}

// ============================================
// Memory Management
// ============================================

/**
 * Get WASM memory usage
 * @returns {Object|null} Memory usage info
 */
export function getMemoryUsage() {
  if (typeof sqlite3?.wasm?.heap8u !== "function") {
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
      console.log("[DB] Closed");
    } catch (error) {
      console.warn("[DB] Close failed, resetting handle anyway:", error);
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
  return isCodeQuery(query) ? "code" : "prose";
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
  normalizeSearchTimeRange,
  detectSearchMode,
  getConversationsByAgent,
  getConversationsByWorkspace,
  getConversationsByTimeRange,
  getMemoryUsage,
  checkMemoryPressure,
  closeDatabase,
  isDatabaseReady,
};
