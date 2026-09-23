(function (root, factory) {
    if (typeof define === 'function' && define.amd) {
        define([], factory);
    } else if (typeof module === 'object' && module.exports) {
        module.exports = factory();
    } else {
        root.argon2 = factory();
    }
})(typeof self !== 'undefined' ? self : this, function () {
    const global = typeof self !== 'undefined' ? self : this;

    /**
     * @enum
     */
    const ArgonType = {
        Argon2d: 0,
        Argon2i: 1,
        Argon2id: 2,
    };

    function loadModule(mem) {
        if (loadModule._promise) {
            return loadModule._promise;
        }
        if (loadModule._module) {
            return Promise.resolve(loadModule._module);
        }
        let promise;
        if (
            global.process &&
            global.process.versions &&
            global.process.versions.node
        ) {
            promise = loadWasmModule().then(
                (Module) =>
                    new Promise((resolve) => {
                        Module.postRun = () => resolve(Module);
                    })
            );
        } else {
            promise = loadWasmBinary().then((wasmBinary) => {
                const wasmMemory = mem ? createWasmMemory(mem) : undefined;
                return initWasm(wasmBinary, wasmMemory);
            });
        }
        loadModule._promise = promise;
        return promise.then((Module) => {
            loadModule._module = Module;
            delete loadModule._promise;
            return Module;
        });
    }

    function initWasm(wasmBinary, wasmMemory) {
        return new Promise((resolve) => {
            global.Module = {
                wasmBinary,
                wasmMemory,
                postRun() {
                    resolve(Module);
                },
            };
            return loadWasmModule();
        });
    }

    function loadWasmModule() {
        if (global.loadArgon2WasmModule) {
            return global.loadArgon2WasmModule();
        }
        if (typeof require === 'function') {
            return Promise.resolve(require('../dist/argon2.js'));
        }
        return import('../dist/argon2.js');
    }

    function loadWasmBinary() {
        if (global.loadArgon2WasmBinary) {
            return global.loadArgon2WasmBinary();
        }
        if (typeof require === 'function') {
            return Promise.resolve(require('../dist/argon2.wasm')).then(
                (wasmModule) => {
                    return decodeWasmBinary(wasmModule);
                }
            );
        }
        const wasmPath =
            global.argon2WasmPath ||
            'node_modules/argon2-browser/dist/argon2.wasm';
        return fetch(wasmPath)
            .then((response) => response.arrayBuffer())
            .then((ab) => new Uint8Array(ab));
    }

    function decodeWasmBinary(base64) {
        const text = atob(base64);
        const binary = new Uint8Array(new ArrayBuffer(text.length));
        for (let i = 0; i < text.length; i++) {
            binary[i] = text.charCodeAt(i);
        }
        return binary;
    }

    function createWasmMemory(mem) {
        const KB = 1024;
        const MB = 1024 * KB;
        const GB = 1024 * MB;
        const WASM_PAGE_SIZE = 64 * KB;

        const totalMemory = (2 * GB - 64 * KB) / WASM_PAGE_SIZE;
        const initialMemory = Math.min(
            Math.max(Math.ceil((mem * KB) / WASM_PAGE_SIZE), 256) + 256,
            totalMemory
        );

        return new WebAssembly.Memory({
            initial: initialMemory,
            maximum: totalMemory,
        });
    }

    function allocateArray(Module, arr) {
        return Module.allocate(arr, 'i8', Module.ALLOC_NORMAL);
    }

    function allocateArrayStr(Module, arr) {
        const nullTerminatedArray = new Uint8Array([...arr, 0]);
        return allocateArray(Module, nullTerminatedArray);
    }

    function encodeUtf8(str) {
        if (typeof str !== 'string') {
            return str;
        }
        if (typeof TextEncoder === 'function') {
            return new TextEncoder().encode(str);
        } else if (typeof Buffer === 'function') {
            return Buffer.from(str);
        } else {
            throw new Error("Don't know how to encode UTF8");
        }
    }

    /**
     * Argon2 hash
     * @param {string|Uint8Array} params.pass - password string
     * @param {string|Uint8Array} params.salt - salt string
     * @param {number} [params.time=1] - the number of iterations
     * @param {number} [params.mem=1024] - used memory, in KiB
     * @param {number} [params.hashLen=24] - desired hash length
     * @param {number} [params.parallelism=1] - desired parallelism
     * @param {number} [params.type=argon2.ArgonType.Argon2d] - hash type:
     *      argon2.ArgonType.Argon2d
     *      argon2.ArgonType.Argon2i
     *      argon2.ArgonType.Argon2id
     *
     * @return Promise
     *
     * @example
     *  argon2.hash({ pass: 'password', salt: 'somesalt' })
     *      .then(h => console.log(h.hash, h.hashHex, h.encoded))
     *      .catch(e => console.error(e.message, e.code))
     */
    function argon2Hash(params) {
        const mCost = params.mem || 1024;
        return loadModule(mCost).then((Module) => {
            const tCost = params.time || 1;
            const parallelism = params.parallelism || 1;
            const pwdEncoded = encodeUtf8(params.pass);
            const pwd = allocateArrayStr(Module, pwdEncoded);
            const pwdlen = pwdEncoded.length;
            const saltEncoded = encodeUtf8(params.salt);
            const salt = allocateArrayStr(Module, saltEncoded);
            const saltlen = saltEncoded.length;
            const argon2Type = params.type || ArgonType.Argon2d;
            const hash = Module.allocate(
                new Array(params.hashLen || 24),
                'i8',
                Module.ALLOC_NORMAL
            );
            const secret = params.secret
                ? allocateArray(Module, params.secret)
                : 0;
            const secretlen = params.secret ? params.secret.byteLength : 0;
            const ad = params.ad ? allocateArray(Module, params.ad) : 0;
            const adlen = params.ad ? params.ad.byteLength : 0;
            const hashlen = params.hashLen || 24;
            const encodedlen = Module._argon2_encodedlen(
                tCost,
                mCost,
                parallelism,
                saltlen,
                hashlen,
                argon2Type
            );
            const encoded = Module.allocate(
                new Array(encodedlen + 1),
                'i8',
                Module.ALLOC_NORMAL
            );
            const version = 0x13;
            let err;
            let res;
            try {
                res = Module._argon2_hash_ext(
                    tCost,
                    mCost,
                    parallelism,
                    pwd,
                    pwdlen,
                    salt,
                    saltlen,
                    hash,
                    hashlen,
                    encoded,
                    encodedlen,
                    argon2Type,
                    secret,
                    secretlen,
                    ad,
                    adlen,
                    version
                );
            } catch (e) {
                err = e;
            }
            let result;
            if (res === 0 && !err) {
                let hashStr = '';
                const hashArr = new Uint8Array(hashlen);
                for (let i = 0; i < hashlen; i++) {
                    const byte = Module.HEAP8[hash + i];
                    hashArr[i] = byte;
                    hashStr += ('0' + (0xff & byte).toString(16)).slice(-2);
                }
                const encodedStr = Module.UTF8ToString(encoded);
                result = {
                    hash: hashArr,
                    hashHex: hashStr,
                    encoded: encodedStr,
                };
            } else {
                try {
                    if (!err) {
                        err = Module.UTF8ToString(
                            Module._argon2_error_message(res)
                        );
                    }
                } catch (e) {}
                result = { message: err, code: res };
            }
            try {
                Module._free(pwd);
                Module._free(salt);
                Module._free(hash);
                Module._free(encoded);
                if (ad) {
                    Module._free(ad);
                }
                if (secret) {
                    Module._free(secret);
                }
            } catch (e) {}
            if (err) {
                throw result;
            } else {
                return result;
            }
        });
    }

    /**
     * Argon2 verify function
     * @param {string} params.pass - password string
     * @param {string|Uint8Array} params.encoded - encoded hash
     * @param {number} [params.type=argon2.ArgonType.Argon2d] - hash type:
     *      argon2.ArgonType.Argon2d
     *      argon2.ArgonType.Argon2i
     *      argon2.ArgonType.Argon2id
     *
     * @returns Promise
     *
     * @example
     *  argon2.verify({ pass: 'password', encoded: 'encoded-hash' })
     *      .then(() => console.log('OK'))
     *      .catch(e => console.error(e.message, e.code))
     */
    function argon2Verify(params) {
        return loadModule().then((Module) => {
            const pwdEncoded = encodeUtf8(params.pass);
            const pwd = allocateArrayStr(Module, pwdEncoded);
            const pwdlen = pwdEncoded.length;
            const secret = params.secret
                ? allocateArray(Module, params.secret)
                : 0;
            const secretlen = params.secret ? params.secret.byteLength : 0;
            const ad = params.ad ? allocateArray(Module, params.ad) : 0;
            const adlen = params.ad ? params.ad.byteLength : 0;
            const encEncoded = encodeUtf8(params.encoded);
            const enc = allocateArrayStr(Module, encEncoded);
            let argon2Type = params.type;
            if (argon2Type === undefined) {
                let typeStr = params.encoded.split('$')[1];
                if (typeStr) {
                    typeStr = typeStr.replace('a', 'A');
                    argon2Type = ArgonType[typeStr] || ArgonType.Argon2d;
                }
            }
            let err;
            let res;
            try {
                res = Module._argon2_verify_ext(
                    enc,
                    pwd,
                    pwdlen,
                    secret,
                    secretlen,
                    ad,
                    adlen,
                    argon2Type
                );
            } catch (e) {
                err = e;
            }
            let result;
            if (res || err) {
                try {
                    if (!err) {
                        err = Module.UTF8ToString(
                            Module._argon2_error_message(res)
                        );
                    }
                } catch (e) {}
                result = { message: err, code: res };
            }
            try {
                Module._free(pwd);
                Module._free(enc);
            } catch (e) {}
            if (err) {
                throw result;
            } else {
                return result;
            }
        });
    }

    function unloadRuntime() {
        if (loadModule._module) {
            loadModule._module.unloadRuntime();
            delete loadModule._promise;
            delete loadModule._module;
        }
    }

    return {
        ArgonType,
        hash: argon2Hash,
        verify: argon2Verify,
        unloadRuntime,
    };
});
