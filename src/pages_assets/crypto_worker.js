/**
 * cass Archive Crypto Worker
 *
 * Handles key derivation, DEK unwrapping, and chunk decryption in a Web Worker.
 * All expensive cryptographic operations run here to keep the main thread responsive.
 */

// State
let dek = null;
let config = null;

const MAX_ARCHIVE_CHUNK_SIZE = 32 * 1024 * 1024;
const MAX_ARCHIVE_CHUNKS = 0xFFFFFFFF;
const MIN_RECOVERY_SECRET_BYTES = 24;
const AES_GCM_TAG_SIZE = 16;
const MAX_DEFLATE_OVERHEAD_ALLOWANCE = 64 * 1024;
const SUPPORTED_ARGON2_PARAMS = Object.freeze({
    memory_kb: 65536,
    iterations: 3,
    parallelism: 4,
});

function hashScopeId(input) {
    let hash = 0x811c9dc5;
    for (let i = 0; i < input.length; i++) {
        hash ^= input.charCodeAt(i);
        hash = Math.imul(hash, 0x01000193) >>> 0;
    }
    return hash.toString(16).padStart(8, '0');
}

function getArchiveScopeId() {
    try {
        return hashScopeId(new URL('./', self.location.href).href);
    } catch (error) {
        const href = typeof self?.location?.href === 'string'
            ? self.location.href
            : 'unknown';
        return hashScopeId(href.split('#')[0].split('?')[0]);
    }
}

function getArchiveOpfsDbName() {
    return `cass-archive-${getArchiveScopeId()}.db`;
}

/**
 * Handle messages from main thread
 */
self.onmessage = async (event) => {
    const payload = event?.data && typeof event.data === 'object' ? event.data : null;
    const requestId = payload && 'requestId' in payload ? payload.requestId : null;
    if (!payload || typeof payload.type !== 'string' || payload.type.length === 0) {
        console.warn('Ignoring malformed worker request payload');
        if (requestId !== null && requestId !== undefined) {
            self.postMessage({
                type: 'WORKER_ERROR',
                error: 'Malformed worker request payload',
                requestId,
            });
        }
        return;
    }

    const { type, ...data } = payload;

    try {
        switch (type) {
            case 'UNLOCK_PASSWORD':
                await handleUnlockPassword(data.password, data.config, requestId);
                break;

            case 'UNLOCK_RECOVERY':
                await handleUnlockRecovery(data.recoverySecret, data.config, requestId);
                break;

            case 'DECRYPT_DATABASE':
                await handleDecryptDatabase(data.dek, data.config, data.opfsEnabled, requestId);
                break;

            case 'CLEAR_KEYS':
                clearKeys();
                break;

            default:
                throw new Error(`Unknown worker message type: ${type}`);
        }
    } catch (error) {
        console.error('Worker error:', error);
        self.postMessage({
            type: getWorkerFailureMessageType(type),
            error: error?.message || String(error),
            requestId,
        });
    }
};

function getWorkerFailureMessageType(type) {
    switch (type) {
        case 'UNLOCK_PASSWORD':
        case 'UNLOCK_RECOVERY':
            return 'UNLOCK_FAILED';
        case 'DECRYPT_DATABASE':
            return 'DECRYPT_FAILED';
        default:
            return 'WORKER_ERROR';
    }
}

/**
 * Handle password-based unlock
 */
async function handleUnlockPassword(password, cfg, requestId) {
    config = cfg;
    validateSupportedPayloadFormat(config);
    if (typeof password !== 'string' || password.trim().length === 0) {
        throw new Error('Please enter a password');
    }

    // Find password slot
    const passwordSlots = config.key_slots.filter(
        slot => slot.slot_type === 'password' && isValidKeySlotMetadata(slot)
    );
    if (passwordSlots.length === 0) {
        throw new Error('No password slot found in archive');
    }

    self.postMessage({ type: 'PROGRESS', phase: 'Deriving key...', percent: 10, requestId });

    // Try each password slot
    for (const slot of passwordSlots) {
        try {
            const kek = await deriveKekFromPassword(password, slot);
            self.postMessage({ type: 'PROGRESS', phase: 'Unwrapping key...', percent: 80, requestId });

            const unwrappedDek = await unwrapDek(kek, slot, config.export_id);
            dek = unwrappedDek;

            self.postMessage({
                type: 'UNLOCK_SUCCESS',
                dek: arrayToBase64(dek),
                requestId,
            });
            return;
        } catch (error) {
            // Try next slot
            console.debug('Slot unlock failed:', error);
        }
    }

    throw new Error('Incorrect password');
}

/**
 * Handle recovery secret-based unlock
 */
async function handleUnlockRecovery(recoverySecret, cfg, requestId) {
    config = cfg;
    validateSupportedPayloadFormat(config);

    // Find recovery slot
    const recoverySlots = config.key_slots.filter(
        slot => slot.slot_type === 'recovery' && isValidKeySlotMetadata(slot)
    );
    if (recoverySlots.length === 0) {
        throw new Error('No recovery slot found in archive');
    }

    self.postMessage({ type: 'PROGRESS', phase: 'Deriving key...', percent: 10, requestId });

    // Convert recovery secret to bytes
    let secretBytes;
    if (typeof recoverySecret === 'string') {
        // Try base64 first, then UTF-8
        try {
            secretBytes = base64ToArray(recoverySecret);
        } catch {
            secretBytes = new TextEncoder().encode(recoverySecret);
        }
    } else {
        secretBytes = recoverySecret;
    }
    if (!(secretBytes instanceof Uint8Array) || secretBytes.byteLength < MIN_RECOVERY_SECRET_BYTES) {
        throw new Error('Recovery secret must contain at least 192 bits');
    }

    // Try each recovery slot
    for (const slot of recoverySlots) {
        try {
            const kek = await deriveKekFromRecovery(secretBytes, slot);
            self.postMessage({ type: 'PROGRESS', phase: 'Unwrapping key...', percent: 80, requestId });

            const unwrappedDek = await unwrapDek(kek, slot, config.export_id);
            dek = unwrappedDek;

            self.postMessage({
                type: 'UNLOCK_SUCCESS',
                dek: arrayToBase64(dek),
                requestId,
            });
            return;
        } catch (error) {
            // Try next slot
            console.debug('Recovery slot unlock failed:', error);
        }
    }

    throw new Error('Invalid recovery code');
}

/**
 * Derive KEK from password using Argon2id
 */
async function deriveKekFromPassword(password, slot) {
    const params = slot.argon2_params || config.kdf_defaults;
    const salt = base64ToArray(slot.salt);

    // Load Argon2 if not loaded
    if (!self.argon2) {
        await loadArgon2();
    }

    const result = await self.argon2.hash({
        pass: password,
        salt: salt,
        time: params.iterations,
        mem: params.memory_kb,
        parallelism: params.parallelism,
        hashLen: 32,
        type: self.argon2.ArgonType.Argon2id,
    });

    return new Uint8Array(result.hash);
}

/**
 * Derive KEK from recovery secret using HKDF-SHA256
 */
async function deriveKekFromRecovery(secretBytes, slot) {
    const salt = base64ToArray(slot.salt);
    const info = new TextEncoder().encode('cass-pages-kek-v2');

    // Import secret as HKDF key
    const baseKey = await crypto.subtle.importKey(
        'raw',
        secretBytes,
        'HKDF',
        false,
        ['deriveBits']
    );

    // Derive KEK
    const kekBits = await crypto.subtle.deriveBits(
        {
            name: 'HKDF',
            hash: 'SHA-256',
            salt: salt,
            info: info,
        },
        baseKey,
        256
    );

    return new Uint8Array(kekBits);
}

/**
 * Unwrap DEK using AES-256-GCM
 */
async function unwrapDek(kek, slot, exportId) {
    const wrappedDek = base64ToArray(slot.wrapped_dek);
    const nonce = base64ToArray(slot.nonce);
    const exportIdBytes = base64ToArray(exportId);

    // Build AAD: export_id || slot_id
    const aad = new Uint8Array(exportIdBytes.length + 1);
    aad.set(exportIdBytes);
    aad[exportIdBytes.length] = slot.id;

    // Import KEK
    const kekKey = await crypto.subtle.importKey(
        'raw',
        kek,
        { name: 'AES-GCM' },
        false,
        ['decrypt']
    );

    // Unwrap DEK
    const dekBytes = await crypto.subtle.decrypt(
        {
            name: 'AES-GCM',
            iv: nonce,
            additionalData: aad,
        },
        kekKey,
        wrappedDek
    );

    return new Uint8Array(dekBytes);
}

/**
 * Handle database decryption
 */
async function handleDecryptDatabase(dekBase64, cfg, opfsEnabled, requestId) {
    config = cfg;
    validateSupportedPayloadFormat(config);
    dek = base64ToArray(dekBase64);
    if (dek.byteLength !== 32) {
        throw new Error('Invalid data encryption key length');
    }
    const { payload } = config;
    const totalChunks = payload.chunk_count;
    const baseNonce = base64ToArray(config.base_nonce);
    const exportId = base64ToArray(config.export_id);

    self.postMessage({ type: 'PROGRESS', phase: 'Decrypting...', percent: 0, requestId });

    // Import DEK for decryption
    const dekKey = await crypto.subtle.importKey(
        'raw',
        dek,
        { name: 'AES-GCM' },
        false,
        ['decrypt']
    );

    // Decrypt and decompress each chunk. Rust writes one independent deflate
    // stream per encrypted chunk, so concatenating compressed streams before
    // inflate would drop data in browsers/engines that stop at the first stream.
    const plaintextChunks = [];
    let totalDecrypted = 0;
    let totalCiphertext = 0;

    for (let i = 0; i < totalChunks; i++) {
        const chunkName = `chunk-${String(i).padStart(5, '0')}.bin`;
        const expectedChunkPath = `payload/${chunkName}`;
        if (payload.files[i] !== expectedChunkPath) {
            throw new Error(`Invalid payload file entry ${i}: expected ${expectedChunkPath}`);
        }
        const chunkUrl = `./payload/${chunkName}`;

        try {
            const response = await fetch(chunkUrl);
            if (!response.ok) {
                throw new Error(`Failed to fetch chunk ${i}: ${response.status}`);
            }
            const encryptedChunk = await readResponseBytesBounded(
                response,
                maxCiphertextChunkSize(payload.chunk_size),
                `encrypted chunk ${i}`
            );
            totalCiphertext += encryptedChunk.byteLength;
            if (
                !Number.isSafeInteger(totalCiphertext)
                || totalCiphertext > payload.total_compressed_size
            ) {
                throw new Error('Encrypted payload exceeds its declared total size');
            }

            // Derive chunk nonce: first 8 bytes from base_nonce, last 4 bytes are counter
            const chunkNonce = deriveChunkNonce(baseNonce, i);

            // Build chunk AAD: export_id || chunk_index (big-endian u32)
            const aad = buildChunkAad(exportId, i);

            // Decrypt chunk
            const decrypted = await crypto.subtle.decrypt(
                {
                    name: 'AES-GCM',
                    iv: chunkNonce,
                    additionalData: aad,
                },
                dekKey,
                encryptedChunk
            );

            const plaintext = await decompressDeflate(
                new Uint8Array(decrypted),
                payload.chunk_size
            );
            plaintextChunks.push(plaintext);
            totalDecrypted += plaintext.byteLength;
            if (
                !Number.isSafeInteger(totalDecrypted)
                || totalDecrypted > payload.total_plaintext_size
            ) {
                throw new Error('Decrypted payload exceeds its declared total size');
            }

            // Report progress
            const percent = Math.round(((i + 1) / totalChunks) * 90);
            self.postMessage({
                type: 'PROGRESS',
                phase: `Decrypting chunk ${i + 1}/${totalChunks}...`,
                percent: percent,
                requestId,
            });
        } catch (error) {
            throw new Error(`Failed to decrypt chunk ${i}: ${error.message}`);
        }
    }

    if (totalCiphertext !== payload.total_compressed_size) {
        throw new Error(
            `Encrypted payload size mismatch: got ${totalCiphertext}, expected ${payload.total_compressed_size}`
        );
    }
    if (totalDecrypted !== payload.total_plaintext_size) {
        throw new Error(
            `Decrypted payload size mismatch: got ${totalDecrypted}, expected ${payload.total_plaintext_size}`
        );
    }

    self.postMessage({ type: 'PROGRESS', phase: 'Loading database...', percent: 95, requestId });

    // Store in OPFS or memory
    const dbBytes = concatenateChunks(plaintextChunks);

    const transfer = dbBytes.buffer.slice(
        dbBytes.byteOffset,
        dbBytes.byteOffset + dbBytes.byteLength
    );

    self.postMessage(
        {
            type: 'DECRYPT_SUCCESS',
            dbSize: dbBytes.byteLength,
            dbBytes: transfer,
            requestId,
        },
        [transfer]
    );
}

function validateSupportedPayloadFormat(cfg) {
    if (!cfg || typeof cfg !== 'object') {
        throw new Error('Invalid archive config');
    }

    if (cfg.version !== 2) {
        throw new Error(`Unsupported archive schema version: ${cfg.version ?? 'missing'}`);
    }

    if (cfg.compression !== 'deflate') {
        throw new Error(`Unsupported archive compression: ${cfg.compression ?? 'missing'}`);
    }

    const payload = cfg.payload;
    if (!payload || typeof payload !== 'object') {
        throw new Error('Invalid archive payload metadata');
    }

    if (!Number.isSafeInteger(payload.chunk_size) || payload.chunk_size <= 0) {
        throw new Error(`Invalid archive chunk_size: ${payload.chunk_size ?? 'missing'}`);
    }

    if (payload.chunk_size > MAX_ARCHIVE_CHUNK_SIZE) {
        throw new Error(`Invalid archive chunk_size: ${payload.chunk_size} exceeds maximum ${MAX_ARCHIVE_CHUNK_SIZE}`);
    }

    if (!Number.isSafeInteger(payload.chunk_count) || payload.chunk_count < 0) {
        throw new Error(`Invalid archive chunk_count: ${payload.chunk_count ?? 'missing'}`);
    }

    if (payload.chunk_count > MAX_ARCHIVE_CHUNKS) {
        throw new Error(`Invalid archive chunk_count: ${payload.chunk_count} exceeds maximum`);
    }

    if (!Array.isArray(payload.files) || payload.files.length !== payload.chunk_count) {
        throw new Error('Invalid archive payload files list');
    }

    if (!Number.isSafeInteger(payload.total_plaintext_size) || payload.total_plaintext_size < 0) {
        throw new Error('Invalid archive total_plaintext_size');
    }
    if (!Number.isSafeInteger(payload.total_compressed_size) || payload.total_compressed_size < 0) {
        throw new Error('Invalid archive total_compressed_size');
    }

    const expectedChunkCount = Math.ceil(payload.total_plaintext_size / payload.chunk_size);
    if (expectedChunkCount !== payload.chunk_count) {
        throw new Error(
            `Invalid archive plaintext size: expected ${expectedChunkCount} chunks, got ${payload.chunk_count}`
        );
    }

    const maximumCiphertextSize = payload.chunk_count * maxCiphertextChunkSize(payload.chunk_size);
    if (payload.total_compressed_size > maximumCiphertextSize) {
        throw new Error('Invalid archive total_compressed_size: exceeds chunk bounds');
    }
    if (payload.chunk_count === 0 && payload.total_compressed_size !== 0) {
        throw new Error('Invalid empty archive ciphertext size');
    }
    if (payload.chunk_count > 0 && payload.total_compressed_size === 0) {
        throw new Error('Invalid non-empty archive ciphertext size');
    }

    for (let i = 0; i < payload.files.length; i++) {
        const expectedPath = `payload/chunk-${String(i).padStart(5, '0')}.bin`;
        if (payload.files[i] !== expectedPath) {
            throw new Error(`Invalid payload file entry ${i}: expected ${expectedPath}`);
        }
    }

    decodeBase64Field('export_id', cfg.export_id, 16);
    decodeBase64Field('base_nonce', cfg.base_nonce, 12);
    validateArgon2Params(cfg.kdf_defaults, 'kdf_defaults');

    if (!Array.isArray(cfg.key_slots) || cfg.key_slots.length === 0) {
        throw new Error('Encrypted archive must contain at least one key slot');
    }

    const slotIds = new Set();
    let validSlotCount = 0;
    let firstSlotError = null;
    for (const slot of cfg.key_slots) {
        if (!slot || typeof slot !== 'object') {
            throw new Error('Invalid archive key slot');
        }
        if (!Number.isSafeInteger(slot.id) || slot.id < 0 || slot.id > 255) {
            throw new Error('Invalid archive key slot id');
        }
        if (slotIds.has(slot.id)) {
            throw new Error(`Duplicate archive key slot id ${slot.id}`);
        }
        slotIds.add(slot.id);

        try {
            validateKeySlotMetadata(slot);
            validSlotCount += 1;
        } catch (error) {
            firstSlotError ||= error;
        }
    }
    if (validSlotCount === 0) {
        throw firstSlotError || new Error('Encrypted archive has no valid key slot');
    }
}

function maxCiphertextChunkSize(chunkSize) {
    return chunkSize + Math.floor(chunkSize / 8) + MAX_DEFLATE_OVERHEAD_ALLOWANCE + AES_GCM_TAG_SIZE;
}

function validateArgon2Params(params, fieldName) {
    if (!params || typeof params !== 'object') {
        throw new Error(`Invalid archive ${fieldName}`);
    }
    for (const field of ['memory_kb', 'iterations', 'parallelism']) {
        if (!Number.isSafeInteger(params[field]) || params[field] !== SUPPORTED_ARGON2_PARAMS[field]) {
            throw new Error(`Unsupported archive ${fieldName}.${field}`);
        }
    }
}

function validateKeySlotMetadata(slot) {
    if (slot.slot_type !== 'password' && slot.slot_type !== 'recovery') {
        throw new Error('Invalid archive key slot type');
    }

    const expectedKdf = slot.slot_type === 'password' ? 'argon2id' : 'hkdf-sha256';
    if (slot.kdf !== expectedKdf) {
        throw new Error('Invalid archive key slot KDF');
    }

    if (slot.slot_type === 'password') {
        validateArgon2Params(slot.argon2_params, 'key_slots.argon2_params');
    } else if (slot.argon2_params !== undefined && slot.argon2_params !== null) {
        throw new Error('Recovery key slot must not contain Argon2 parameters');
    }

    const salt = decodeBase64Field('key_slots.salt', slot.salt);
    if (salt.byteLength === 0) {
        throw new Error('Archive key slot salt must not be empty');
    }
    decodeBase64Field('key_slots.wrapped_dek', slot.wrapped_dek, 48);
    decodeBase64Field('key_slots.nonce', slot.nonce, 12);
}

function isValidKeySlotMetadata(slot) {
    try {
        validateKeySlotMetadata(slot);
        return true;
    } catch {
        return false;
    }
}

function decodeBase64Field(fieldName, encoded, expectedLength = null) {
    if (
        typeof encoded !== 'string'
        || encoded.length % 4 !== 0
        || !/^[A-Za-z0-9+/]*={0,2}$/.test(encoded)
    ) {
        throw new Error(`Invalid archive ${fieldName}`);
    }

    let decoded;
    try {
        decoded = base64ToArray(encoded);
    } catch {
        throw new Error(`Invalid archive ${fieldName} encoding`);
    }
    if (expectedLength !== null && decoded.byteLength !== expectedLength) {
        throw new Error(`Invalid archive ${fieldName} length`);
    }
    return decoded;
}

/**
 * Derive chunk nonce from base nonce and counter.
 * Uses deterministic counter mode: first 8 bytes from base_nonce,
 * last 4 bytes are the chunk index (big-endian).
 */
function deriveChunkNonce(baseNonce, counter) {
    const nonce = new Uint8Array(12);
    // Copy first 8 bytes from base nonce
    nonce.set(baseNonce.subarray(0, 8));

    // Set last 4 bytes to counter (big-endian u32)
    const counterView = new DataView(new ArrayBuffer(4));
    counterView.setUint32(0, counter, false); // big-endian
    const counterBytes = new Uint8Array(counterView.buffer);
    nonce.set(counterBytes, 8);

    return nonce;
}

/**
 * Build chunk AAD: export_id || chunk_index || schema_version
 * Must match Rust's build_chunk_aad for interoperability
 */
function buildChunkAad(exportId, chunkIndex) {
    const SCHEMA_VERSION = 2;
    const aad = new Uint8Array(exportId.length + 4 + 1); // 16 + 4 + 1 = 21 bytes
    aad.set(exportId);

    // Big-endian u32 chunk index
    const view = new DataView(aad.buffer, exportId.length, 4);
    view.setUint32(0, chunkIndex, false);

    // Schema version byte
    aad[exportId.length + 4] = SCHEMA_VERSION;

    return aad;
}

/**
 * Concatenate array of Uint8Arrays
 */
function concatenateChunks(chunks) {
    const totalLength = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
    const result = new Uint8Array(totalLength);

    let offset = 0;
    for (const chunk of chunks) {
        result.set(chunk, offset);
        offset += chunk.byteLength;
    }

    return result;
}

/**
 * Decompress deflate data
 */
async function decompressDeflate(compressed, maximumOutputBytes) {
    // Prefer native streaming decompression when available so expansion can be
    // stopped before an archive-controlled stream allocates unbounded memory.
    if (self.DecompressionStream) {
        const ds = new self.DecompressionStream('deflate-raw');
        const writer = ds.writable.getWriter();
        const reader = ds.readable.getReader();
        const chunks = [];
        let totalLength = 0;
        const writePromise = writer.write(compressed).then(() => writer.close());

        try {
            while (true) {
                const { done, value } = await reader.read();
                if (done) break;
                totalLength += value.byteLength;
                if (totalLength > maximumOutputBytes) {
                    await reader.cancel('archive chunk exceeds declared plaintext bound');
                    throw new Error(
                        `Decompressed chunk exceeds ${maximumOutputBytes}-byte archive limit`
                    );
                }
                chunks.push(value);
            }
            await writePromise;
        } catch (error) {
            try {
                await writer.abort(error);
            } catch {
                // The stream may already be closed or errored.
            }
            try {
                await writePromise;
            } catch {
                // Preserve the original bounded-decompression error.
            }
            throw error;
        }
        return concatenateChunks(chunks);
    }

    if (!self.fflate?.Inflate) {
        await loadFflate();
    }
    if (!self.fflate?.Inflate) {
        throw new Error('Streaming decompression library is unavailable');
    }

    const chunks = [];
    let totalLength = 0;
    let finalChunkSeen = false;
    const limitError = new Error(
        `Decompressed chunk exceeds ${maximumOutputBytes}-byte archive limit`
    );
    const inflater = new self.fflate.Inflate((chunk, final) => {
        totalLength += chunk.byteLength;
        if (totalLength > maximumOutputBytes) {
            throw limitError;
        }
        chunks.push(chunk);
        finalChunkSeen ||= final;
    });
    try {
        inflater.push(compressed, true);
    } catch (error) {
        if (error === limitError) {
            throw limitError;
        }
        throw new Error(`Compressed chunk is invalid: ${error?.message || String(error)}`);
    }
    if (!finalChunkSeen) {
        throw new Error('Compressed chunk ended before the final DEFLATE block');
    }
    return concatenateChunks(chunks);
}

async function readResponseBytesBounded(response, maximumBytes, label) {
    const contentLengthHeader = response.headers?.get?.('content-length');
    if (contentLengthHeader && /^\d+$/.test(contentLengthHeader)) {
        const contentLength = Number(contentLengthHeader);
        if (!Number.isSafeInteger(contentLength) || contentLength > maximumBytes) {
            throw new Error(`${label} exceeds the ${maximumBytes}-byte download limit`);
        }
    }

    if (!response.body?.getReader) {
        const bytes = new Uint8Array(await response.arrayBuffer());
        if (bytes.byteLength > maximumBytes) {
            throw new Error(`${label} exceeds the ${maximumBytes}-byte download limit`);
        }
        return bytes;
    }

    const reader = response.body.getReader();
    const chunks = [];
    let totalLength = 0;
    while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        totalLength += value.byteLength;
        if (totalLength > maximumBytes) {
            await reader.cancel(`${label} exceeds its download limit`);
            throw new Error(`${label} exceeds the ${maximumBytes}-byte download limit`);
        }
        chunks.push(value);
    }
    return concatenateChunks(chunks);
}

/**
 * Initialize sqlite-wasm with decrypted database
 */
async function initDatabase(dbBytes, opfsEnabled, requestId) {
    // Load sqlite-wasm if not loaded
    if (!self.sqlite3) {
        await loadSqlite();
    }

    try {
        // Initialize sqlite-wasm
        const sqlite3 = await self.sqlite3InitModule();

        // Try OPFS first (persistent, better performance) if user opted in
        let db;
        if (opfsEnabled && sqlite3.oo1.OpfsDb) {
            try {
                const opfsDbName = getArchiveOpfsDbName();
                // Write to OPFS
                const opfs = await navigator.storage.getDirectory();
                const fileHandle = await opfs.getFileHandle(opfsDbName, { create: true });
                const writable = await fileHandle.createWritable();
                await writable.write(dbBytes);
                await writable.close();

                db = new sqlite3.oo1.OpfsDb(opfsDbName);
            } catch (opfsError) {
                console.warn('OPFS not available, using in-memory:', opfsError);
                db = new sqlite3.oo1.DB();
                db.deserialize(dbBytes);
            }
        } else {
            // In-memory database
            db = new sqlite3.oo1.DB();
            db.deserialize(dbBytes);
        }

        // Store database reference
        self.cassDb = db;

        self.postMessage({
            type: 'DB_READY',
            conversationCount: getConversationCount(db),
            messageCount: getMessageCount(db),
            requestId,
        });
    } catch (error) {
        throw new Error(`Failed to initialize database: ${error.message}`);
    }
}

/**
 * Get conversation count from database
 */
function getConversationCount(db) {
    try {
        const result = db.exec('SELECT COUNT(*) FROM conversations');
        return result[0]?.values[0][0] || 0;
    } catch {
        return 0;
    }
}

/**
 * Get message count from database
 */
function getMessageCount(db) {
    try {
        const result = db.exec('SELECT COUNT(*) FROM messages');
        return result[0]?.values[0][0] || 0;
    } catch {
        return 0;
    }
}

/**
 * Clear keys from memory
 */
function clearKeys() {
    if (dek) {
        // Zero out the DEK
        dek.fill(0);
        dek = null;
    }
    config = null;

    // Close database
    if (self.cassDb) {
        try {
            self.cassDb.close();
        } catch {
            // Ignore
        }
        self.cassDb = null;
    }
}

/**
 * Load Argon2 library
 */
async function loadArgon2() {
    try {
        importScripts('./vendor/argon2-wasm.js');
    } catch (error) {
        throw new Error('Failed to load Argon2 library. Ensure argon2-wasm.js is in the vendor folder.');
    }
}

/**
 * Load fflate library
 */
async function loadFflate() {
    try {
        importScripts('./vendor/fflate.min.js');
    } catch (error) {
        throw new Error('Failed to load decompression library.');
    }
}

/**
 * Load sqlite-wasm library
 */
async function loadSqlite() {
    try {
        importScripts('./vendor/sqlite3.js');
    } catch (error) {
        throw new Error('Failed to load SQLite library.');
    }
}

/**
 * Convert base64 to Uint8Array
 */
function base64ToArray(base64) {
    const normalized = normalizeBase64(base64);
    const binary = atob(normalized);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i);
    }
    return bytes;
}

function normalizeBase64(base64) {
    const trimmed = base64.trim().replace(/-/g, '+').replace(/_/g, '/');
    const padding = trimmed.length % 4;
    if (padding === 0) {
        return trimmed;
    }
    return trimmed + '='.repeat(4 - padding);
}

/**
 * Convert Uint8Array to base64
 */
function arrayToBase64(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.length; i++) {
        binary += String.fromCharCode(bytes[i]);
    }
    return btoa(binary);
}
