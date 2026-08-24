/**
 * cass Archive Crypto Worker
 *
 * Handles key derivation, DEK unwrapping, and chunk decryption in a Web Worker.
 * All expensive cryptographic operations run here to keep the main thread responsive.
 */

// Key state is retained only so CLEAR_KEYS can explicitly zero the most
// recently returned DEK. All request processing uses request-local config and
// key bytes so overlapping messages cannot cross-contaminate archive context.
let currentDek = null;
let activeUnlockGeneration = 0;
let argon2LoadPromise = null;

const MAX_ARCHIVE_CHUNK_SIZE = 32 * 1024 * 1024;
const MAX_ARCHIVE_CHUNKS = 0xFFFFFFFF;
// The viewer materializes the full plaintext database and a WASM copy. Bound
// browser work independently of the wider on-disk format's u32 nonce space.
const MAX_BROWSER_ARCHIVE_CHUNKS = 4096;
const MAX_BROWSER_ARCHIVE_PLAINTEXT_SIZE = 512 * 1024 * 1024;
const MAX_BROWSER_ARCHIVE_CIPHERTEXT_SIZE = 640 * 1024 * 1024;
const MIN_RECOVERY_SECRET_BYTES = 24;
const AES_GCM_TAG_SIZE = 16;
const MAX_DEFLATE_OVERHEAD_ALLOWANCE = 64 * 1024;
const FFLATE_INPUT_SLICE_SIZE = 16 * 1024;
const MAX_ARGON2_WASM_SIZE = 1024 * 1024;
const SUPPORTED_ARGON2_PARAMS = Object.freeze({
    memory_kb: 65536,
    iterations: 3,
    parallelism: 4,
});

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
                await handleDecryptDatabase(data.dek, data.config, requestId);
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

function clearCurrentDek() {
    if (currentDek instanceof Uint8Array) {
        currentDek.fill(0);
    }
    currentDek = null;
}

function beginUnlockAttempt() {
    activeUnlockGeneration += 1;
    clearCurrentDek();
    return activeUnlockGeneration;
}

function invalidateUnlockAttempts() {
    activeUnlockGeneration += 1;
    clearCurrentDek();
}

function ensureCurrentUnlockAttempt(generation) {
    if (generation !== activeUnlockGeneration) {
        throw new Error('Unlock request was superseded');
    }
}

function commitUnlockResult(generation, unwrappedDek, requestId) {
    if (!(unwrappedDek instanceof Uint8Array) || unwrappedDek.byteLength !== 32) {
        unwrappedDek?.fill?.(0);
        throw new Error('Unwrapped data encryption key has an invalid length');
    }
    try {
        ensureCurrentUnlockAttempt(generation);
    } catch (error) {
        unwrappedDek.fill(0);
        throw error;
    }

    clearCurrentDek();
    currentDek = unwrappedDek;
    try {
        self.postMessage({
            type: 'UNLOCK_SUCCESS',
            dek: arrayToBase64(currentDek),
            requestId,
        });
    } catch (error) {
        clearCurrentDek();
        throw error;
    }
}

/**
 * Handle password-based unlock
 */
async function handleUnlockPassword(password, cfg, requestId) {
    const generation = beginUnlockAttempt();
    let committed = false;
    try {
        validateSupportedPayloadFormat(cfg);
        if (typeof password !== 'string' || password.trim().length === 0) {
            throw new Error('Please enter a password');
        }

        const passwordSlots = cfg.key_slots.filter(
            slot => slot.slot_type === 'password' && isValidKeySlotMetadata(slot)
        );
        if (passwordSlots.length === 0) {
            throw new Error('No password slot found in archive');
        }

        self.postMessage({ type: 'PROGRESS', phase: 'Deriving key...', percent: 10, requestId });

        for (const slot of passwordSlots) {
            ensureCurrentUnlockAttempt(generation);
            const kek = await deriveKekFromPassword(password, slot);
            let unwrappedDek = null;
            try {
                ensureCurrentUnlockAttempt(generation);
                self.postMessage({ type: 'PROGRESS', phase: 'Unwrapping key...', percent: 80, requestId });
                unwrappedDek = await unwrapDek(kek, slot, cfg.export_id);
            } catch (error) {
                ensureCurrentUnlockAttempt(generation);
                if (error?.name !== 'OperationError') {
                    throw error;
                }
                console.debug('Password slot authentication failed:', error);
            } finally {
                kek.fill(0);
            }
            if (unwrappedDek) {
                commitUnlockResult(generation, unwrappedDek, requestId);
                committed = true;
                return;
            }
        }

        throw new Error('Incorrect password');
    } finally {
        if (!committed && generation === activeUnlockGeneration) {
            clearCurrentDek();
        }
    }
}

/**
 * Handle recovery secret-based unlock
 */
async function handleUnlockRecovery(recoverySecret, cfg, requestId) {
    const generation = beginUnlockAttempt();
    let committed = false;
    let secretBytes = null;
    try {
        validateSupportedPayloadFormat(cfg);

        const recoverySlots = cfg.key_slots.filter(
            slot => slot.slot_type === 'recovery' && isValidKeySlotMetadata(slot)
        );
        if (recoverySlots.length === 0) {
            throw new Error('No recovery slot found in archive');
        }

        self.postMessage({ type: 'PROGRESS', phase: 'Deriving key...', percent: 10, requestId });

        if (typeof recoverySecret === 'string') {
            try {
                secretBytes = base64ToArray(recoverySecret);
            } catch {
                secretBytes = new TextEncoder().encode(recoverySecret);
            }
        } else if (recoverySecret instanceof Uint8Array) {
            secretBytes = new Uint8Array(recoverySecret);
        }
        if (!secretBytes || secretBytes.byteLength < MIN_RECOVERY_SECRET_BYTES) {
            throw new Error('Recovery secret must contain at least 24 bytes');
        }

        for (const slot of recoverySlots) {
            ensureCurrentUnlockAttempt(generation);
            const kek = await deriveKekFromRecovery(secretBytes, slot);
            let unwrappedDek = null;
            try {
                ensureCurrentUnlockAttempt(generation);
                self.postMessage({ type: 'PROGRESS', phase: 'Unwrapping key...', percent: 80, requestId });
                unwrappedDek = await unwrapDek(kek, slot, cfg.export_id);
            } catch (error) {
                ensureCurrentUnlockAttempt(generation);
                if (error?.name !== 'OperationError') {
                    throw error;
                }
                console.debug('Recovery slot authentication failed:', error);
            } finally {
                kek.fill(0);
            }
            if (unwrappedDek) {
                commitUnlockResult(generation, unwrappedDek, requestId);
                committed = true;
                return;
            }
        }

        throw new Error('Invalid recovery code');
    } finally {
        secretBytes?.fill(0);
        if (!committed && generation === activeUnlockGeneration) {
            clearCurrentDek();
        }
    }
}

/**
 * Derive KEK from password using Argon2id
 */
async function deriveKekFromPassword(password, slot) {
    const params = slot.argon2_params;
    const salt = base64ToArray(slot.salt);
    try {
        if (!self.argon2) {
            await loadArgon2();
        }

        const result = await self.argon2.hash({
            pass: password,
            salt,
            time: params.iterations,
            mem: params.memory_kb,
            parallelism: params.parallelism,
            hashLen: 32,
            type: self.argon2.ArgonType.Argon2id,
        });

        return new Uint8Array(result.hash);
    } finally {
        salt.fill(0);
    }
}

/**
 * Derive KEK from recovery secret using HKDF-SHA256
 */
async function deriveKekFromRecovery(secretBytes, slot) {
    const salt = base64ToArray(slot.salt);
    const info = new TextEncoder().encode('cass-pages-kek-v2');
    try {
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
                salt,
                info,
            },
            baseKey,
            256
        );

        return new Uint8Array(kekBits);
    } finally {
        salt.fill(0);
        info.fill(0);
    }
}

/**
 * Unwrap DEK using AES-256-GCM
 */
async function unwrapDek(kek, slot, exportId) {
    const wrappedDek = base64ToArray(slot.wrapped_dek);
    const nonce = base64ToArray(slot.nonce);
    const exportIdBytes = base64ToArray(exportId);
    const aad = new Uint8Array(exportIdBytes.length + 1);
    try {
        // Build AAD: export_id || slot_id
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
    } finally {
        wrappedDek.fill(0);
        nonce.fill(0);
        exportIdBytes.fill(0);
        aad.fill(0);
    }
}

/**
 * Handle database decryption
 */
async function handleDecryptDatabase(dekBase64, cfg, requestId) {
    invalidateUnlockAttempts();
    validateSupportedPayloadFormat(cfg);
    const requestDek = base64ToArray(dekBase64);
    let dbBytes = null;
    try {
        if (requestDek.byteLength !== 32) {
            throw new Error('Invalid data encryption key length');
        }
        const { payload } = cfg;
        const totalChunks = payload.chunk_count;
        const baseNonce = base64ToArray(cfg.base_nonce);
        const exportId = base64ToArray(cfg.export_id);
        dbBytes = new Uint8Array(payload.total_plaintext_size);

        self.postMessage({ type: 'PROGRESS', phase: 'Decrypting...', percent: 0, requestId });

        // Import DEK for decryption. The CryptoKey and all archive context are
        // request-local so overlapping messages cannot swap export metadata.
        const dekKey = await crypto.subtle.importKey(
            'raw',
            requestDek,
            { name: 'AES-GCM' },
            false,
            ['decrypt']
        );

        // Decrypt and decompress each chunk. Rust writes one independent deflate
        // stream per encrypted chunk, so concatenating compressed streams before
        // inflate would drop data in browsers/engines that stop at the first stream.
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

                const decrypted = await crypto.subtle.decrypt(
                    {
                        name: 'AES-GCM',
                        iv: chunkNonce,
                        additionalData: aad,
                    },
                    dekKey,
                    encryptedChunk
                );
                const compressedChunk = new Uint8Array(decrypted);
                try {
                    const plaintext = await decompressDeflate(
                        compressedChunk,
                        payload.chunk_size
                    );
                    try {
                        const nextTotal = totalDecrypted + plaintext.byteLength;
                        if (
                            !Number.isSafeInteger(nextTotal)
                            || nextTotal > payload.total_plaintext_size
                        ) {
                            throw new Error('Decrypted payload exceeds its declared total size');
                        }
                        dbBytes.set(plaintext, totalDecrypted);
                        totalDecrypted = nextTotal;
                    } finally {
                        plaintext.fill(0);
                    }
                } finally {
                    compressedChunk.fill(0);
                }

                // Report progress
                const percent = Math.round(((i + 1) / totalChunks) * 90);
                self.postMessage({
                    type: 'PROGRESS',
                    phase: `Decrypting chunk ${i + 1}/${totalChunks}...`,
                    percent,
                    requestId,
                });
            } catch (error) {
                throw new Error(
                    `Failed to decrypt chunk ${i}: ${error?.message || String(error)}`
                );
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

        const dbSize = dbBytes.byteLength;
        // dbBytes is the exact preallocated database buffer. Transfer it
        // directly instead of retaining a second plaintext database copy.
        const transfer = dbBytes.buffer;

        self.postMessage(
            {
                type: 'DECRYPT_SUCCESS',
                dbSize,
                dbBytes: transfer,
                requestId,
            },
            [transfer]
        );
    } finally {
        requestDek.fill(0);
        if (dbBytes?.byteLength) {
            dbBytes.fill(0);
        }
    }
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
    if (payload.chunk_count > MAX_BROWSER_ARCHIVE_CHUNKS) {
        throw new Error(
            `Archive has ${payload.chunk_count} chunks; browser limit is ${MAX_BROWSER_ARCHIVE_CHUNKS}`
        );
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
    if (payload.total_plaintext_size > MAX_BROWSER_ARCHIVE_PLAINTEXT_SIZE) {
        throw new Error(
            `Archive plaintext exceeds the ${MAX_BROWSER_ARCHIVE_PLAINTEXT_SIZE}-byte browser limit`
        );
    }
    if (payload.total_compressed_size > MAX_BROWSER_ARCHIVE_CIPHERTEXT_SIZE) {
        throw new Error(
            `Archive ciphertext exceeds the ${MAX_BROWSER_ARCHIVE_CIPHERTEXT_SIZE}-byte browser limit`
        );
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

function concatenateAndZeroChunks(chunks) {
    try {
        return concatenateChunks(chunks);
    } finally {
        for (const chunk of chunks) {
            chunk.fill(0);
        }
        chunks.length = 0;
    }
}

/**
 * Decompress deflate data
 */
async function decompressDeflate(compressed, maximumOutputBytes) {
    // Prefer native streaming decompression when available so expansion can be
    // stopped before an archive-controlled stream allocates unbounded memory.
    let nativeStream = null;
    if (typeof self.DecompressionStream === 'function') {
        try {
            nativeStream = new self.DecompressionStream('deflate-raw');
        } catch (error) {
            // Safari and older Chromium builds may expose the constructor but
            // reject deflate-raw. Continue through the bounded fflate path.
            console.debug('Native deflate-raw decompression unavailable:', error);
        }
    }
    if (nativeStream) {
        const ds = nativeStream;
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
                    value.fill(0);
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
            for (const chunk of chunks) {
                chunk.fill(0);
            }
            chunks.length = 0;
            throw error;
        }
        return concatenateAndZeroChunks(chunks);
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
            chunk.fill(0);
            throw limitError;
        }
        chunks.push(chunk);
        finalChunkSeen ||= final;
    });
    try {
        if (compressed.byteLength === 0) {
            inflater.push(compressed, true);
        } else {
            const sliceSize = Math.min(
                FFLATE_INPUT_SLICE_SIZE,
                Math.max(1, maximumOutputBytes)
            );
            for (let offset = 0; offset < compressed.byteLength; offset += sliceSize) {
                const end = Math.min(offset + sliceSize, compressed.byteLength);
                inflater.push(compressed.subarray(offset, end), end === compressed.byteLength);
            }
        }
    } catch (error) {
        for (const chunk of chunks) {
            chunk.fill(0);
        }
        chunks.length = 0;
        if (error === limitError) {
            throw limitError;
        }
        throw new Error(`Compressed chunk is invalid: ${error?.message || String(error)}`);
    }
    if (!finalChunkSeen) {
        for (const chunk of chunks) {
            chunk.fill(0);
        }
        throw new Error('Compressed chunk ended before the final DEFLATE block');
    }
    return concatenateAndZeroChunks(chunks);
}

async function readResponseBytesBounded(response, maximumBytes, label) {
    if (!Number.isSafeInteger(maximumBytes) || maximumBytes <= 0) {
        throw new Error(`${label} has an invalid download limit`);
    }
    const contentLengthHeader = response.headers?.get?.('content-length');
    if (contentLengthHeader && /^\d+$/.test(contentLengthHeader)) {
        const contentLength = Number(contentLengthHeader);
        if (!Number.isSafeInteger(contentLength) || contentLength > maximumBytes) {
            throw new Error(`${label} exceeds the ${maximumBytes}-byte download limit`);
        }
    }

    if (!response.body?.getReader) {
        throw new Error(`${label} cannot be read safely without streaming response support`);
    }

    const reader = response.body.getReader();
    const chunks = [];
    let totalLength = 0;
    try {
        while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            totalLength += value.byteLength;
            if (totalLength > maximumBytes) {
                value.fill(0);
                throw new Error(`${label} exceeds the ${maximumBytes}-byte download limit`);
            }
            chunks.push(value);
        }
        return concatenateAndZeroChunks(chunks);
    } catch (error) {
        for (const chunk of chunks) {
            chunk.fill(0);
        }
        chunks.length = 0;
        try {
            await reader.cancel(error?.message || String(error));
        } catch {
            // The response stream may already be closed or errored.
        }
        throw error;
    }
}

/**
 * Clear keys from memory
 */
function clearKeys() {
    invalidateUnlockAttempts();
}

/**
 * Load Argon2 library
 */
async function loadArgon2() {
    if (self.argon2) {
        return;
    }
    if (argon2LoadPromise) {
        return argon2LoadPromise;
    }

    argon2LoadPromise = (async () => {
        self.loadArgon2WasmBinary = async () => {
            const wasmUrl = new URL('./vendor/argon2.wasm', self.location.href);
            const response = await fetch(wasmUrl);
            if (!response.ok) {
                throw new Error(`Argon2 WASM request failed with status ${response.status}`);
            }
            return readResponseBytesBounded(
                response,
                MAX_ARGON2_WASM_SIZE,
                'Argon2 WASM runtime'
            );
        };
        self.loadArgon2WasmModule = () => {
            importScripts('./vendor/argon2-wasm.js');
            return Promise.resolve(self.Module);
        };

        // The package's high-level API supplies self.argon2. Its two hooks
        // above adapt the package's ESM-oriented defaults to a classic worker.
        importScripts('./vendor/argon2.js');
        if (!self.argon2?.hash || self.argon2?.ArgonType?.Argon2id !== 2) {
            throw new Error('Argon2 high-level API did not initialize');
        }
    })();

    try {
        await argon2LoadPromise;
    } catch (error) {
        argon2LoadPromise = null;
        self.argon2 = null;
        throw new Error(`Failed to load Argon2 runtime: ${error?.message || String(error)}`);
    }
}

/**
 * Load fflate library
 */
async function loadFflate() {
    try {
        importScripts('./vendor/fflate.js');
        if (!self.fflate?.Inflate) {
            throw new Error('fflate did not expose its streaming Inflate API');
        }
    } catch (error) {
        throw new Error(`Failed to load decompression runtime: ${error?.message || String(error)}`);
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
