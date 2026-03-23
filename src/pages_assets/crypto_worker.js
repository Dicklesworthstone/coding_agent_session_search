/**
 * cass Archive Crypto Worker
 *
 * Handles key derivation, DEK unwrapping, and chunk decryption in a Web Worker.
 * All expensive cryptographic operations run here to keep the main thread responsive.
 */

// State
let dek = null;
let config = null;

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
    const { type, ...data } = event.data;
    const { requestId } = data;

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
                console.warn('Unknown message type:', type);
        }
    } catch (error) {
        console.error('Worker error:', error);
        self.postMessage({
            type: type.replace('UNLOCK', 'UNLOCK_FAILED').replace('DECRYPT', 'DECRYPT_FAILED'),
            error: error.message,
            requestId,
        });
    }
};

/**
 * Handle password-based unlock
 */
async function handleUnlockPassword(password, cfg, requestId) {
    config = cfg;

    // Find password slot
    const passwordSlots = config.key_slots.filter(s => s.slot_type === 'password');
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

    // Find recovery slot
    const recoverySlots = config.key_slots.filter(s => s.slot_type === 'recovery');
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
    dek = base64ToArray(dekBase64);
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

    // Decrypt each chunk
    const decryptedChunks = [];
    let totalDecrypted = 0;

    for (let i = 0; i < totalChunks; i++) {
        const chunkName = `chunk-${String(i).padStart(5, '0')}.bin`;
        const chunkUrl = `./payload/${chunkName}`;

        try {
            const response = await fetch(chunkUrl);
            if (!response.ok) {
                throw new Error(`Failed to fetch chunk ${i}: ${response.status}`);
            }
            const encryptedChunk = await response.arrayBuffer();

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

            decryptedChunks.push(new Uint8Array(decrypted));
            totalDecrypted += decrypted.byteLength;

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

    self.postMessage({ type: 'PROGRESS', phase: 'Decompressing...', percent: 92, requestId });

    // Concatenate chunks
    const compressed = concatenateChunks(decryptedChunks);

    // Decompress
    let decompressed;
    if (config.compression === 'deflate') {
        decompressed = await decompressDeflate(compressed);
    } else {
        // No compression
        decompressed = compressed;
    }

    self.postMessage({ type: 'PROGRESS', phase: 'Loading database...', percent: 95, requestId });

    // Store in OPFS or memory
    const dbBytes = decompressed;

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
async function decompressDeflate(compressed) {
    // Use fflate if available, otherwise DecompressionStream
    if (self.fflate?.inflateSync) {
        return self.fflate.inflateSync(compressed);
    }

    // Try native DecompressionStream (Chrome 80+, Firefox 113+, Safari 16.4+)
    if (self.DecompressionStream) {
        const ds = new DecompressionStream('deflate-raw');
        const writer = ds.writable.getWriter();
        const reader = ds.readable.getReader();

        writer.write(compressed);
        writer.close();

        const chunks = [];
        while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            chunks.push(value);
        }

        return concatenateChunks(chunks);
    }

    // Fallback: load fflate
    await loadFflate();
    return self.fflate.inflateSync(compressed);
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
