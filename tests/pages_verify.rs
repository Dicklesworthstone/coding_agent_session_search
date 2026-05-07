use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pages_verify")
        .join(name)
}

#[test]
fn test_pages_verify_valid_bundle_json() {
    let fixture = fixture_root("valid");

    let output = cargo_bin_cmd!("cass")
        .args(["pages", "--verify"])
        .arg(&fixture)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("run cass pages --verify (valid)");

    assert!(output.status.success(), "verify should succeed");

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(json.get("status").and_then(Value::as_str), Some("valid"));

    let checks = json.get("checks").expect("checks field");
    assert_eq!(
        checks
            .get("required_files")
            .and_then(|c| c.get("passed"))
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn test_pages_verify_missing_required_file_fails() {
    let fixture = fixture_root("missing_required_no_viewer");

    let output = cargo_bin_cmd!("cass")
        .args(["pages", "--verify"])
        .arg(&fixture)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("run cass pages --verify (missing required)");

    assert!(!output.status.success(), "verify should fail");

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(json.get("status").and_then(Value::as_str), Some("invalid"));

    let checks = json.get("checks").expect("checks field");
    assert_eq!(
        checks
            .get("required_files")
            .and_then(|c| c.get("passed"))
            .and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn test_pages_verify_secret_leak_fails() {
    let fixture = fixture_root("secret_leak");

    let output = cargo_bin_cmd!("cass")
        .args(["pages", "--verify"])
        .arg(&fixture)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .output()
        .expect("run cass pages --verify (secret leak)");

    assert!(!output.status.success(), "verify should fail on secrets");

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(json.get("status").and_then(Value::as_str), Some("invalid"));

    let checks = json.get("checks").expect("checks field");
    assert_eq!(
        checks
            .get("no_secrets_in_site")
            .and_then(|c| c.get("passed"))
            .and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn test_attachment_object_urls_are_cached_per_mime_type() {
    let script = r#"
        import { loadBlobAsUrl, reset } from './src/pages_assets/attachments.js';

        const hash = 'b'.repeat(64);
        const dek = new Uint8Array([1, 2, 3, 4]);
        const exportId = new Uint8Array([5, 6, 7, 8]);

        let fetchCalls = 0;
        let decryptCalls = 0;
        const createdTypes = [];

        const originalFetch = globalThis.fetch;
        const originalImportKey = globalThis.crypto.subtle.importKey;
        const originalDeriveBits = globalThis.crypto.subtle.deriveBits;
        const originalDecrypt = globalThis.crypto.subtle.decrypt;
        const originalCreateObjectURL = URL.createObjectURL;
        const originalRevokeObjectURL = URL.revokeObjectURL;

        globalThis.fetch = async (url) => {
            fetchCalls += 1;
            if (!String(url).endsWith(`/${hash}.bin`)) {
                throw new Error(`unexpected fetch url: ${url}`);
            }
            return {
                ok: true,
                status: 200,
                arrayBuffer: async () => new Uint8Array([9, 8, 7, 6]).buffer,
            };
        };

        globalThis.crypto.subtle.importKey = async () => ({});
        globalThis.crypto.subtle.deriveBits = async () => new Uint8Array(12).buffer;
        globalThis.crypto.subtle.decrypt = async () => {
            decryptCalls += 1;
            return new Uint8Array([1, 2, 3]).buffer;
        };

        URL.createObjectURL = (blob) => {
            createdTypes.push(blob.type);
            return `blob:${blob.type}:${createdTypes.length}`;
        };
        URL.revokeObjectURL = () => {};

        try {
            reset();

            const imageUrlA = await loadBlobAsUrl(hash, 'image/png', dek, exportId);
            const imageUrlB = await loadBlobAsUrl(hash, 'image/png', dek, exportId);
            const textUrlA = await loadBlobAsUrl(hash, 'text/plain', dek, exportId);
            const textUrlB = await loadBlobAsUrl(hash, 'text/plain', dek, exportId);

            if (imageUrlA !== imageUrlB) {
                throw new Error(`expected same-MIME image URLs to be reused, got ${imageUrlA} vs ${imageUrlB}`);
            }
            if (textUrlA !== textUrlB) {
                throw new Error(`expected same-MIME text URLs to be reused, got ${textUrlA} vs ${textUrlB}`);
            }
            if (imageUrlA === textUrlA) {
                throw new Error(`expected different MIME types to get distinct object URLs, got ${imageUrlA}`);
            }
            if (fetchCalls !== 1 || decryptCalls !== 1) {
                throw new Error(`expected blob bytes to remain hash-deduplicated, got fetch=${fetchCalls} decrypt=${decryptCalls}`);
            }
            if (
                createdTypes.length !== 2
                || createdTypes[0] !== 'image/png'
                || !createdTypes[1].startsWith('text/plain')
            ) {
                throw new Error(`expected object URLs to preserve requested MIME types, got ${JSON.stringify(createdTypes)}`);
            }
        } finally {
            reset();
            globalThis.fetch = originalFetch;
            globalThis.crypto.subtle.importKey = originalImportKey;
            globalThis.crypto.subtle.deriveBits = originalDeriveBits;
            globalThis.crypto.subtle.decrypt = originalDecrypt;
            URL.createObjectURL = originalCreateObjectURL;
            URL.revokeObjectURL = originalRevokeObjectURL;
        }
    "#;

    let output = Command::new("node")
        .args(["--input-type=module", "--eval", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run node module assertions");

    assert!(
        output.status.success(),
        "node module assertions failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
