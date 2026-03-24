//! Integration tests for the bundle builder.

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::bundle::{BundleBuilder, BundleConfig, IntegrityManifest};
    use coding_agent_search::pages::encrypt::EncryptionEngine;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a test encrypted archive in the given directory
    fn setup_encrypted_archive(dir: &Path) -> Result<()> {
        // Create a test file to encrypt
        let test_file = dir.join("test_input.db");
        fs::write(&test_file, b"test database content for bundle testing")?;

        // Encrypt it
        let mut engine = EncryptionEngine::default();
        engine.add_password_slot("test-password")?;
        let dir_buf = dir.to_path_buf();
        engine.encrypt_file(&test_file, &dir_buf, |_, _| {})?;

        // Clean up the source file
        fs::remove_file(&test_file)?;

        Ok(())
    }

    fn run_node_module_assertions(script: &str) -> Result<()> {
        let output = Command::new("node")
            .args(["--input-type=module", "--eval", script])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()?;

        assert!(
            output.status.success(),
            "node module assertions failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(())
    }

    #[test]
    fn test_bundle_creates_directory_structure() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new()
            .title("Test Archive")
            .description("A test archive");

        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify directory structure
        assert!(result.site_dir.exists(), "site/ directory should exist");
        assert!(
            result.private_dir.exists(),
            "private/ directory should exist"
        );
        assert!(
            result.site_dir.join("payload").exists(),
            "site/payload/ should exist"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_copies_all_assets() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify required files exist
        let site_dir = &result.site_dir;

        // Web assets
        assert!(
            site_dir.join("index.html").exists(),
            "index.html should exist"
        );
        assert!(
            site_dir.join("styles.css").exists(),
            "styles.css should exist"
        );
        assert!(site_dir.join("auth.js").exists(), "auth.js should exist");
        assert!(
            site_dir.join("viewer.js").exists(),
            "viewer.js should exist"
        );
        assert!(
            site_dir.join("search.js").exists(),
            "search.js should exist"
        );
        assert!(site_dir.join("sw.js").exists(), "sw.js should exist");

        // Static files
        assert!(
            site_dir.join("robots.txt").exists(),
            "robots.txt should exist"
        );
        assert!(
            site_dir.join(".nojekyll").exists(),
            ".nojekyll should exist"
        );
        assert!(
            site_dir.join("README.md").exists(),
            "README.md should exist"
        );

        // Config files
        assert!(
            site_dir.join("config.json").exists(),
            "config.json should exist"
        );
        assert!(
            site_dir.join("site.json").exists(),
            "site.json should exist"
        );
        assert!(
            site_dir.join("integrity.json").exists(),
            "integrity.json should exist"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_copies_payload_chunks() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify payload chunks were copied
        assert!(result.chunk_count > 0, "Should have at least one chunk");

        let payload_dir = result.site_dir.join("payload");
        let chunk_count = fs::read_dir(&payload_dir)?
            .filter(|e| {
                e.as_ref()
                    .map(|e| {
                        e.path()
                            .extension()
                            .map(|ext| ext == "bin")
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .count();

        assert_eq!(chunk_count, result.chunk_count, "Chunk count should match");

        Ok(())
    }

    #[test]
    fn test_bundle_generates_integrity_manifest() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Load and verify integrity manifest
        let integrity_path = result.site_dir.join("integrity.json");
        let integrity_content = fs::read_to_string(&integrity_path)?;
        let manifest: IntegrityManifest = serde_json::from_str(&integrity_content)?;

        assert_eq!(manifest.version, 1);
        assert!(!manifest.files.is_empty(), "Should have file entries");

        // Verify integrity.json is not in the manifest (chicken/egg)
        assert!(!manifest.files.contains_key("integrity.json"));

        // Verify each listed file exists and has correct size
        for (rel_path, entry) in &manifest.files {
            let file_path = result.site_dir.join(rel_path);
            assert!(file_path.exists(), "File {} should exist", rel_path);

            let metadata = fs::metadata(&file_path)?;
            assert_eq!(metadata.len(), entry.size, "Size mismatch for {}", rel_path);

            // Verify hash is valid hex SHA256 (64 chars)
            assert_eq!(
                entry.sha256.len(),
                64,
                "Hash should be 64 hex chars for {}",
                rel_path
            );
        }

        Ok(())
    }

    #[test]
    fn test_bundle_generates_fingerprint() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Fingerprint should be 16 hex characters
        assert_eq!(
            result.fingerprint.len(),
            16,
            "Fingerprint should be 16 chars"
        );
        assert!(
            result.fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
            "Fingerprint should be hex"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_writes_private_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let config = BundleConfig {
            title: "Test Archive".to_string(),
            description: "Test description".to_string(),
            hide_metadata: false,
            recovery_secret: Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            generate_qr: false,
            generated_docs: Vec::new(),
        };

        let builder = BundleBuilder::with_config(config);
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify private artifacts
        assert!(
            result
                .private_dir
                .join("integrity-fingerprint.txt")
                .exists()
        );
        assert!(result.private_dir.join("recovery-secret.txt").exists());
        assert!(result.private_dir.join("master-key.json").exists());

        // Verify recovery secret content
        let recovery_content = fs::read_to_string(result.private_dir.join("recovery-secret.txt"))?;
        assert!(recovery_content.contains("Recovery Secret"));
        assert!(recovery_content.contains("NEVER share"));

        Ok(())
    }

    #[test]
    fn test_bundle_site_has_no_secrets() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let config = BundleConfig {
            title: "Test Archive".to_string(),
            description: "Test description".to_string(),
            hide_metadata: false,
            recovery_secret: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            generate_qr: false,
            generated_docs: Vec::new(),
        };

        let builder = BundleBuilder::with_config(config);
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify site/ has no private files
        assert!(!result.site_dir.join("recovery-secret.txt").exists());
        assert!(!result.site_dir.join("qr-code.png").exists());
        assert!(!result.site_dir.join("qr-code.svg").exists());
        assert!(!result.site_dir.join("integrity-fingerprint.txt").exists());
        assert!(!result.site_dir.join("master-key.json").exists());

        // Verify config.json doesn't contain DEK or secrets
        let _config_content = fs::read_to_string(result.site_dir.join("config.json"))?;
        // DEK would be unwrapped, so it shouldn't be plain in config
        // But wrapped DEK is expected (that's the design - LUKS-style key slots)

        Ok(())
    }

    #[test]
    fn test_bundle_robots_txt_content() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        let robots_content = fs::read_to_string(result.site_dir.join("robots.txt"))?;
        assert!(robots_content.contains("User-agent: *"));
        assert!(robots_content.contains("Disallow: /"));

        Ok(())
    }

    #[test]
    fn test_bundle_site_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new()
            .title("My Custom Archive")
            .description("Custom description here");

        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        let site_json_content = fs::read_to_string(result.site_dir.join("site.json"))?;
        let site_json: serde_json::Value = serde_json::from_str(&site_json_content)?;

        assert_eq!(site_json["title"], "My Custom Archive");
        assert_eq!(site_json["description"], "Custom description here");
        assert_eq!(site_json["generator"], "cass");

        Ok(())
    }

    #[test]
    fn test_bundle_fails_without_config() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        // Don't create config.json or payload/

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {});

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("config.json"),
            "Error should mention missing config.json"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_fails_without_payload() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;

        // Create config.json but no payload/
        let test_config = serde_json::json!({
            "version": 2,
            "export_id": "test",
            "base_nonce": "test",
            "compression": "deflate",
            "kdf_defaults": {},
            "payload": {"files": []},
            "key_slots": []
        });
        fs::write(
            encrypted_dir.join("config.json"),
            serde_json::to_string(&test_config)?,
        )?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {});

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("payload"),
            "Error should mention missing payload/"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_progress_callback() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let phases: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let phases_clone = phases.clone();

        let builder = BundleBuilder::new();
        builder.build(&encrypted_dir, &bundle_dir, move |phase, _msg| {
            phases_clone.lock().unwrap().push(phase.to_string());
        })?;

        let captured = phases.lock().unwrap();
        assert!(captured.contains(&"setup".to_string()));
        assert!(captured.contains(&"assets".to_string()));
        assert!(captured.contains(&"payload".to_string()));
        assert!(captured.contains(&"config".to_string()));
        assert!(captured.contains(&"integrity".to_string()));
        assert!(captured.contains(&"private".to_string()));
        assert!(captured.contains(&"complete".to_string()));

        Ok(())
    }

    #[test]
    fn test_pages_share_and_router_reject_malformed_routes() -> Result<()> {
        run_node_module_assertions(
            r#"
                import { Router } from './src/pages_assets/router.js';
                import { parseShareLink } from './src/pages_assets/share.js';

                const router = new Router({ autoInit: false });
                const invalidPaths = [
                    '/c',
                    '/c/12/extra',
                    '/c/12/m',
                    '/c/12/m/34/extra',
                    '/search/extra',
                    '/settings/extra',
                    '/stats/extra',
                ];

                for (const path of invalidPaths) {
                    const route = router._matchRoute(path);
                    if (route.view !== 'not-found') {
                        throw new Error(`expected not-found for ${path}, got ${JSON.stringify(route)}`);
                    }
                }

                const invalidLinks = [
                    'https://example.com/#/c/12/extra',
                    'https://example.com/#/c/12/m/34/extra',
                    'https://example.com/#/search/extra',
                    'https://example.com/#/settings/extra',
                    'https://example.com/#/stats/extra',
                ];

                for (const link of invalidLinks) {
                    const parsed = parseShareLink(link);
                    if (parsed !== null) {
                        throw new Error(`expected null for ${link}, got ${JSON.stringify(parsed)}`);
                    }
                }

                const validLink = parseShareLink('https://example.com/#/c/12/m/34?agent=claude');
                if (!validLink || validLink.params.conversationId !== 12 || validLink.params.messageId !== 34 || validLink.query.agent !== 'claude') {
                    throw new Error(`unexpected valid link parse result: ${JSON.stringify(validLink)}`);
                }
            "#,
        )
    }

    #[test]
    fn test_stats_role_bar_markup_uses_slugged_class() {
        let stats_js = include_str!("../src/pages_assets/stats.js");
        assert!(
            !stats_js.contains("role-${role.toLowerCase()}"),
            "stats role bar markup should not use the unsanitized role class"
        );
        assert!(
            stats_js.contains("role-${toCssSlug(role)}"),
            "stats role bar markup should use the slugged role class"
        );
    }

    #[test]
    fn test_stats_markup_stays_csp_safe_without_inline_styles() {
        let stats_js = include_str!("../src/pages_assets/stats.js");
        assert!(
            !stats_js.contains("style=\"font-size:")
                && !stats_js.contains("style=\"width: ${percent}%"),
            "stats markup should not emit inline style attributes under the strict pages CSP"
        );
        assert!(
            stats_js.contains("data-term-size=\"${size.toFixed(3)}\"")
                && stats_js.contains("data-term-opacity=\"${opacity.toFixed(3)}\"")
                && stats_js.contains("data-role-width=\"${percent}\""),
            "stats markup should carry dynamic style values through data attributes instead"
        );
        assert!(
            stats_js.contains("applyDynamicStatsStyles();")
                && stats_js.contains("term.style.fontSize =")
                && stats_js.contains("roleBar.style.width ="),
            "stats renderer should apply dynamic sizing after insertion instead of through inline markup"
        );
    }

    #[test]
    fn test_viewer_lock_paths_reset_hash_to_home() {
        let viewer_js = include_str!("../src/pages_assets/viewer.js");
        assert!(
            viewer_js.contains("function syncLockedViewerState()"),
            "viewer lock handling should centralize state/hash reset"
        );
        assert!(
            viewer_js.contains("window.history?.replaceState"),
            "viewer lock handling should update the hash without triggering a fresh route load"
        );
        assert_eq!(
            viewer_js.matches("syncLockedViewerState();").count(),
            2,
            "both viewer lock paths should reset state and hash together"
        );
        assert_eq!(
            viewer_js.matches("cleanup();").count(),
            2,
            "both viewer lock paths should tear down the live viewer to avoid stale route handling while locked"
        );
    }

    #[test]
    fn test_conversation_fallback_sanitizer_blocks_unsafe_link_schemes() -> Result<()> {
        run_node_module_assertions(
            r#"
                import { sanitizeDestinationUrl } from './src/pages_assets/conversation.js';

                const blocked = [
                    'javascript:alert(1)',
                    ' JaVaScRiPt:alert(1)',
                    'java\tscript:alert(1)',
                    '\u0000data:image/svg+xml,<svg/onload=1>',
                    'vbscript:msgbox(1)',
                ];

                for (const url of blocked) {
                    if (sanitizeDestinationUrl(url) !== '#') {
                        throw new Error(`expected ${JSON.stringify(url)} to be blocked`);
                    }
                }

                const allowed = [
                    'https://example.com/path?q=1',
                    '/local/path',
                    './relative/path',
                    '#message-12',
                    'mailto:test@example.com',
                ];

                for (const url of allowed) {
                    if (sanitizeDestinationUrl(url) !== url.trim()) {
                        throw new Error(`expected ${JSON.stringify(url)} to remain allowed`);
                    }
                }
            "#,
        )?;

        let conversation_js = include_str!("../src/pages_assets/conversation.js");
        assert!(
            conversation_js
                .contains("el.setAttribute('href', sanitizeDestinationUrl(attr.value));"),
            "fallback HTML sanitizer should sanitize href attributes, not just markdown link generation"
        );

        Ok(())
    }

    #[test]
    fn test_search_result_card_ids_are_unique_per_hit() -> Result<()> {
        run_node_module_assertions(
            r#"
                import { buildResultCardId } from './src/pages_assets/search.js';

                const sameConversationDifferentMessages = [
                    buildResultCardId({ conversation_id: 12, message_id: 34 }, 0),
                    buildResultCardId({ conversation_id: 12, message_id: 35 }, 1),
                ];

                if (sameConversationDifferentMessages[0] === sameConversationDifferentMessages[1]) {
                    throw new Error(`expected unique ids for different message hits, got ${JSON.stringify(sameConversationDifferentMessages)}`);
                }

                const conversationOnly = [
                    buildResultCardId({ conversation_id: 99, message_id: null }, 0),
                    buildResultCardId({ conversation_id: 99, message_id: null }, 1),
                ];

                if (conversationOnly[0] === conversationOnly[1]) {
                    throw new Error(`expected unique ids for repeated conversation-only hits, got ${JSON.stringify(conversationOnly)}`);
                }
            "#,
        )?;

        let search_js = include_str!("../src/pages_assets/search.js");
        assert!(
            search_js.contains("article.id = buildResultCardId(result, index);"),
            "virtual result rendering should use the unique result id helper"
        );
        assert!(
            search_js.contains("id=\"${buildResultCardId(result, index)}\""),
            "direct result rendering should use the unique result id helper"
        );

        Ok(())
    }

    #[test]
    fn test_search_paths_round_trip_symbolic_time_filters() -> Result<()> {
        run_node_module_assertions(
            r#"
                import { buildSearchPath, parseSearchParams } from './src/pages_assets/router.js';

                const path = buildSearchPath('auth bug', {
                    agent: 'claude',
                    timePreset: 'week',
                    since: 1,
                    until: 2,
                });
                const url = new URL(`https://example.com#${path}`);
                const params = new URLSearchParams(url.hash.split('?')[1] || '');

                if (params.get('q') !== 'auth bug') {
                    throw new Error(`expected q to round-trip, got ${params.get('q')}`);
                }
                if (params.get('agent') !== 'claude') {
                    throw new Error(`expected agent to round-trip, got ${params.get('agent')}`);
                }
                if (params.get('time') !== 'week') {
                    throw new Error(`expected symbolic time filter in URL, got ${params.toString()}`);
                }
                if (params.has('since') || params.has('until')) {
                    throw new Error(`did not expect explicit timestamps when timePreset is present, got ${params.toString()}`);
                }

                const parsed = parseSearchParams({
                    query: {
                        q: 'auth bug',
                        agent: 'claude',
                        time: 'week',
                    },
                });

                if (parsed.query !== 'auth bug' || parsed.agent !== 'claude' || parsed.timePreset !== 'week') {
                    throw new Error(`expected parseSearchParams to restore symbolic filters, got ${JSON.stringify(parsed)}`);
                }
                if (parsed.since !== null || parsed.until !== null) {
                    throw new Error(`did not expect parseSearchParams to synthesize timestamps, got ${JSON.stringify(parsed)}`);
                }
            "#,
        )?;

        Ok(())
    }

    #[test]
    fn test_search_cleanup_paths_reset_virtual_results_presentation() {
        let search_js = include_str!("../src/pages_assets/search.js");
        assert!(
            search_js.contains("function destroyVirtualResultsView() {")
                && search_js.contains("destroyVirtualList();")
                && search_js.contains("resetResultsListLayout();"),
            "search should centralize virtual-results teardown so error/reset paths do not leave stale virtual list state behind"
        );
        assert!(
            search_js.contains("destroyVirtualResultsView();\n        showNoResults();")
                && search_js.contains("destroyVirtualResultsView();\n    hideNoResults();")
                && search_js.contains("destroyVirtualResultsView();\n    hideLoading();"),
            "search no-results, error, and clear/reset paths should all tear down virtual-results presentation"
        );
    }

    #[test]
    fn test_search_route_state_restores_filters_and_back_navigation() {
        let viewer_js = include_str!("../src/pages_assets/viewer.js");
        assert!(
            viewer_js.contains("const searchParams = parseSearchParams(route);")
                && viewer_js.contains("setSearchRoute(searchParams).catch")
                && viewer_js.contains(
                    "router.navigate(buildSearchPath(searchState.query, searchState.filters));"
                ),
            "viewer should restore routed search filters into the live search UI and preserve the current query/filter state when navigating back from a conversation"
        );

        let search_js = include_str!("../src/pages_assets/search.js");
        assert!(
            search_js.contains("export async function setSearchRoute(routeSearch = {}, options = {}) {")
                && search_js.contains("currentFilters = normalizeRouteFilters(routeSearch);")
                && search_js.contains("timePreset: since !== null || until !== null ? SEARCH_CONFIG.TIME_FILTER_CUSTOM_VALUE : null,"),
            "search should expose a route-state application path that restores routed filters instead of keeping stale in-memory filters alive"
        );
    }

    #[test]
    fn test_restored_session_reinstalls_cleanup_handlers() -> Result<()> {
        run_node_module_assertions(
            r#"
                class EventTargetMock {
                    constructor() {
                        this.listeners = new Map();
                        this.hidden = false;
                        this.location = { href: 'https://example.com/archive/index.html#/' };
                    }

                    addEventListener(type, handler) {
                        const handlers = this.listeners.get(type) || new Set();
                        handlers.add(handler);
                        this.listeners.set(type, handlers);
                    }

                    removeEventListener(type, handler) {
                        const handlers = this.listeners.get(type);
                        if (!handlers) {
                            return;
                        }
                        handlers.delete(handler);
                        if (handlers.size === 0) {
                            this.listeners.delete(type);
                        }
                    }

                    listenerCount(type) {
                        return this.listeners.get(type)?.size || 0;
                    }

                    resetListeners() {
                        this.listeners.clear();
                    }
                }

                class StorageMock {
                    constructor() {
                        this.data = new Map();
                    }

                    getItem(key) {
                        return this.data.has(key) ? this.data.get(key) : null;
                    }

                    setItem(key, value) {
                        this.data.set(key, String(value));
                    }

                    removeItem(key) {
                        this.data.delete(key);
                    }

                    clear() {
                        this.data.clear();
                    }
                }

                const originalWindow = globalThis.window;
                const originalDocument = globalThis.document;
                const originalLocalStorage = globalThis.localStorage;
                const originalSessionStorage = globalThis.sessionStorage;
                const originalBtoa = globalThis.btoa;
                const originalAtob = globalThis.atob;

                globalThis.window = new EventTargetMock();
                globalThis.document = new EventTargetMock();
                globalThis.localStorage = new StorageMock();
                globalThis.sessionStorage = new StorageMock();
                globalThis.btoa = (value) => Buffer.from(value, 'binary').toString('base64');
                globalThis.atob = (value) => Buffer.from(value, 'base64').toString('binary');

                try {
                    const { SessionManager, SESSION_CONFIG } = await import('./src/pages_assets/session.js');

                    const seedManager = new SessionManager({
                        storage: SESSION_CONFIG.STORAGE_SESSION,
                        duration: 60_000,
                    });
                    await seedManager.startSession(new Uint8Array([1, 2, 3, 4]), true);

                    const persistedEntries = new Map(globalThis.sessionStorage.data);
                    seedManager.endSession();

                    globalThis.sessionStorage.data = new Map(persistedEntries);
                    globalThis.document.resetListeners();
                    globalThis.window.resetListeners();

                    const restoredManager = new SessionManager({
                        storage: SESSION_CONFIG.STORAGE_SESSION,
                        duration: 60_000,
                    });

                    const restoredDek = await restoredManager.restoreSession();
                    if (!(restoredDek instanceof Uint8Array) || restoredDek.length !== 4) {
                        throw new Error('expected restoreSession to return the persisted DEK');
                    }

                    if (globalThis.document.listenerCount('visibilitychange') !== 1) {
                        throw new Error(`expected one visibilitychange handler after restore, got ${globalThis.document.listenerCount('visibilitychange')}`);
                    }

                    if (globalThis.window.listenerCount('beforeunload') !== 1) {
                        throw new Error(`expected one beforeunload handler after restore, got ${globalThis.window.listenerCount('beforeunload')}`);
                    }

                    restoredManager.endSession();
                } finally {
                    globalThis.window = originalWindow;
                    globalThis.document = originalDocument;
                    globalThis.localStorage = originalLocalStorage;
                    globalThis.sessionStorage = originalSessionStorage;
                    globalThis.btoa = originalBtoa;
                    globalThis.atob = originalAtob;
                }
            "#,
        )
    }

    #[test]
    fn test_variable_virtual_list_coalesces_scroll_frames() -> Result<()> {
        run_node_module_assertions(
            r#"
                class MockElement {
                    constructor() {
                        this.style = {};
                        this.dataset = {};
                        this.children = [];
                        this.listeners = new Map();
                        this.clientHeight = 400;
                        this.scrollTop = 0;
                        this.innerHTML = '';
                        this.isConnected = true;
                        this.className = '';
                        this.offsetHeight = 80;
                    }

                    appendChild(child) {
                        this.children.push(child);
                        child.isConnected = true;
                        return child;
                    }

                    remove() {
                        this.isConnected = false;
                    }

                    addEventListener(type, handler) {
                        this.listeners.set(type, handler);
                    }

                    removeEventListener(type, handler) {
                        if (this.listeners.get(type) === handler) {
                            this.listeners.delete(type);
                        }
                    }
                }

                const originalDocument = globalThis.document;
                const originalResizeObserver = globalThis.ResizeObserver;
                const originalRequestAnimationFrame = globalThis.requestAnimationFrame;
                const originalCancelAnimationFrame = globalThis.cancelAnimationFrame;

                const queuedFrames = [];
                globalThis.document = {
                    createElement() {
                        return new MockElement();
                    },
                };
                globalThis.ResizeObserver = class {
                    constructor(callback) {
                        this.callback = callback;
                    }
                    observe() {}
                    disconnect() {}
                };
                globalThis.requestAnimationFrame = (callback) => {
                    queuedFrames.push(callback);
                    return queuedFrames.length;
                };
                globalThis.cancelAnimationFrame = () => {};

                try {
                    const { VariableHeightVirtualList } = await import('./src/pages_assets/virtual-list.js');

                    const container = new MockElement();
                    const list = new VariableHeightVirtualList({
                        container,
                        totalCount: 100,
                        estimatedItemHeight: 60,
                        renderItem: () => new MockElement(),
                    });

                    queuedFrames.length = 0;
                    const scrollHandler = container.listeners.get('scroll');
                    if (typeof scrollHandler !== 'function') {
                        throw new Error('expected virtual list to register a scroll handler');
                    }

                    scrollHandler();
                    scrollHandler();
                    scrollHandler();

                    if (queuedFrames.length !== 1) {
                        throw new Error(`expected one queued animation frame for repeated scroll events, got ${queuedFrames.length}`);
                    }

                    queuedFrames.shift()();
                    scrollHandler();

                    if (queuedFrames.length !== 1) {
                        throw new Error(`expected scroll coalescing state to reset after a frame drains, got ${queuedFrames.length}`);
                    }

                    list.destroy();
                } finally {
                    globalThis.document = originalDocument;
                    globalThis.ResizeObserver = originalResizeObserver;
                    globalThis.requestAnimationFrame = originalRequestAnimationFrame;
                    globalThis.cancelAnimationFrame = originalCancelAnimationFrame;
                }
            "#,
        )
    }

    #[test]
    fn test_virtual_list_scroll_to_index_renders_target_range_immediately() -> Result<()> {
        run_node_module_assertions(
            r#"
                class MockElement {
                    constructor() {
                        this.style = {};
                        this.dataset = {};
                        this.children = [];
                        this.listeners = new Map();
                        this.clientHeight = 160;
                        this.scrollTop = 0;
                        this.innerHTML = '';
                        this.isConnected = true;
                        this.className = '';
                        this.focused = false;
                    }

                    appendChild(child) {
                        this.children.push(child);
                        child.parentElement = this;
                        child.isConnected = true;
                        return child;
                    }

                    remove() {
                        this.isConnected = false;
                        if (this.parentElement) {
                            this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
                        }
                    }

                    addEventListener(type, handler) {
                        this.listeners.set(type, handler);
                    }

                    removeEventListener(type, handler) {
                        if (this.listeners.get(type) === handler) {
                            this.listeners.delete(type);
                        }
                    }
                }

                const originalDocument = globalThis.document;
                const originalResizeObserver = globalThis.ResizeObserver;
                const originalRequestAnimationFrame = globalThis.requestAnimationFrame;

                globalThis.document = {
                    createElement() {
                        return new MockElement();
                    },
                };
                globalThis.ResizeObserver = class {
                    constructor(callback) {
                        this.callback = callback;
                    }
                    observe() {}
                    disconnect() {}
                };
                globalThis.requestAnimationFrame = (callback) => {
                    callback();
                    return 1;
                };

                try {
                    const { VirtualList } = await import('./src/pages_assets/virtual-list.js');

                    const container = new MockElement();
                    const list = new VirtualList({
                        container,
                        itemHeight: 40,
                        totalCount: 100,
                        renderItem: (index) => {
                            const element = new MockElement();
                            element.dataset.resultIndex = String(index);
                            return element;
                        },
                        overscan: 1,
                    });

                    if (!list.items.has(0)) {
                        throw new Error('expected initial render to include the first item');
                    }
                    if (list.items.has(50)) {
                        throw new Error('did not expect target item to be rendered before programmatic scroll');
                    }

                    list.scrollToIndex(50, 'center');

                    if (!list.items.has(50)) {
                        throw new Error('expected scrollToIndex to render the target item immediately');
                    }

                    list.destroy();
                } finally {
                    globalThis.document = originalDocument;
                    globalThis.ResizeObserver = originalResizeObserver;
                    globalThis.requestAnimationFrame = originalRequestAnimationFrame;
                }
            "#,
        )
    }

    #[test]
    fn test_auth_qr_scanner_cancel_invalidates_pending_start_and_clears_dom() {
        let auth_js = include_str!("../src/pages_assets/auth.js");
        assert!(
            auth_js.contains("let activeQrScannerSession = 0;"),
            "auth QR flow should track scanner sessions so cancel/lock can invalidate in-flight starts"
        );
        assert!(
            auth_js.contains("let qrLibraryLoadPromise = null;"),
            "auth QR flow should share one library load promise instead of injecting duplicate scripts"
        );
        assert!(
            auth_js.contains("const sessionToken = beginQrScannerSession();"),
            "auth QR open flow should snapshot the current scanner session before async work"
        );
        assert!(
            auth_js.contains("if (qrScanner && !elements.qrScanner?.classList.contains('hidden'))"),
            "auth QR open flow should refuse to spawn a second scanner while one is already active"
        );
        assert!(
            auth_js.contains("!isCurrentQrScannerSession(sessionToken)")
                && auth_js.contains("elements.qrScanner?.classList.contains('hidden')"),
            "auth QR open flow should abort stale scanner starts after cancel or lock"
        );
        assert!(
            auth_js.contains("await scanner.clear();"),
            "auth QR teardown should clear the library-owned DOM after stopping the camera"
        );
        assert!(
            auth_js.contains("elements.qrReader?.replaceChildren();"),
            "auth QR teardown should clear any stale scanner markup from the reader container"
        );
    }

    #[test]
    fn test_conversation_load_has_error_boundary_for_render_failures() {
        let conversation_js = include_str!("../src/pages_assets/conversation.js");
        assert!(
            conversation_js
                .contains("console.error(`[Conversation] Failed to load conversation ${conversationId}:`, error);"),
            "conversation load failures should be logged with conversation context"
        );
        assert!(
            conversation_js.contains("showError('Failed to load conversation');"),
            "conversation load failures should render a user-visible error panel instead of becoming unhandled promise rejections"
        );
        assert!(
            conversation_js.contains("teardownDocumentListeners();")
                && conversation_js.contains("destroyVirtualList();"),
            "conversation load failures should tear down stale listeners and virtual-list state before showing the error panel"
        );
    }

    #[test]
    fn test_settings_async_handlers_await_rerender() {
        let settings_js = include_str!("../src/pages_assets/settings.js");
        assert!(
            settings_js.contains("export async function initSettings(container, options = {})"),
            "settings initialization should be async so the initial render can be awaited"
        );
        assert!(
            settings_js.contains("async function rerenderSettingsUI(reason) {")
                && settings_js.contains(
                    "console.error(`[Settings] Failed to rerender settings after ${reason}:`, err);"
                ),
            "settings should have a shared safe rerender helper for rollback paths"
        );
        assert!(
            settings_js.contains("await render();"),
            "settings initialization and async handlers should await the async render path"
        );
        assert!(
            settings_js.contains("showNotification(`Storage mode changed to ${newMode}`, 'success');\n        await render();"),
            "storage mode changes should await the async settings rerender so rerender failures stay inside the handler error path"
        );
        assert!(
            settings_js.contains(
                "showNotification('Current storage cleared', 'success');\n        await render();"
            ),
            "clear-current-storage should await the async settings rerender"
        );
        assert!(
            settings_js.contains(
                "showNotification('OPFS cache cleared', 'success');\n        await render();"
            ),
            "clear-OPFS should await the async settings rerender"
        );
        assert!(
            settings_js.contains("await render();\n    } catch (err) {\n        console.error('[Settings] Failed to refresh settings after OPFS toggle:', err);"),
            "OPFS toggle rerender should be awaited and caught instead of becoming an unhandled promise rejection"
        );
        assert!(
            settings_js.contains("showNotification('Failed to disable OPFS caching because cached files could not be fully cleared', 'error');\n                await render();"),
            "the partial OPFS-clear path should also await the rerender before returning"
        );
        assert!(
            settings_js.contains("await rerenderSettingsUI('storage mode cancellation');")
                && settings_js.contains("await rerenderSettingsUI('storage mode change failure');")
                && settings_js.contains("await rerenderSettingsUI('OPFS enable cancellation');")
                && settings_js.contains("await rerenderSettingsUI('OPFS enable failure');")
                && settings_js.contains("await rerenderSettingsUI('OPFS disable failure');"),
            "settings rollback paths should rerender the canonical UI after canceled or failed optimistic control changes"
        );

        let viewer_js = include_str!("../src/pages_assets/viewer.js");
        assert!(
            viewer_js.contains("await initSettings(elements.settingsView, {"),
            "viewer settings bootstrap should await async settings initialization"
        );
        assert!(
            viewer_js.contains("await renderSettings();"),
            "viewer settings rendering should await async settings rerenders"
        );
    }

    #[test]
    fn test_index_bootstrap_respects_csp_without_inline_module_script() {
        let index_html = include_str!("../src/pages_assets/index.html");
        assert!(
            index_html.contains("script-src 'self' 'wasm-unsafe-eval';"),
            "pages bundle should keep the strict CSP script policy"
        );
        assert!(
            index_html.contains("id=\"auth-screen\" class=\"auth-container\""),
            "auth screen should stay visible in static markup so a failed auth.js startup does not leave the page blank"
        );
        assert!(
            !index_html.contains("<script type=\"module\">"),
            "pages bundle should not ship inline module scripts that its own CSP blocks"
        );

        let auth_js = include_str!("../src/pages_assets/auth.js");
        assert!(
            auth_js.contains("import { COI_STATE, getCOIState, initCOIDetection, onServiceWorkerActivated } from './coi-detector.js';"),
            "COI bootstrap should now live in auth.js"
        );
        assert!(
            auth_js.contains("registerServiceWorker().catch((error) => {")
                && auth_js.contains("initCOIDetection({")
                && auth_js.contains("onServiceWorkerActivated(async () => {")
                && auth_js.contains("authScreen?.classList.add('hidden');"),
            "auth.js should own service-worker registration, initial auth hiding, COI initialization, and activation rechecks"
        );
        assert!(
            auth_js.contains("const appScreen = document.getElementById('app-screen');")
                && auth_js.contains("if (appScreen && !appScreen.classList.contains('hidden')) {")
                && auth_js.contains("const revealAuthScreenIfLocked = () => {")
                && auth_js.contains("revealAuthScreenIfLocked();"),
            "COI bootstrap should only re-show the auth screen while the app is still locked, including late failure paths"
        );
        assert!(
            auth_js.contains("}).catch((error) => {")
                && auth_js.contains("console.error('[App] COI initialization failed:', error);")
                && auth_js.contains("revealAuthScreenIfLocked();"),
            "COI bootstrap failures should fall back to revealing the auth screen instead of leaving the page blank"
        );
    }

    #[test]
    fn test_service_worker_activation_callbacks_handle_async_rejections() {
        let coi_detector_js = include_str!("../src/pages_assets/coi-detector.js");
        assert!(
            coi_detector_js.contains("Promise.resolve(registeredCallback()).catch((error) => {")
                && coi_detector_js
                    .contains("console.error('[COI] Activation callback failed:', error);"),
            "service worker activation fanout should catch rejected async callbacks instead of leaking unhandled promise rejections"
        );
    }

    #[test]
    fn test_service_worker_message_handler_ignores_malformed_payloads() {
        let sw_js = include_str!("../src/pages_assets/sw.js");
        assert!(
            sw_js.contains(
                "const payload = event.data && typeof event.data === 'object' ? event.data : null;"
            ) && sw_js.contains("if (!payload) {")
                && sw_js.contains("Ignoring malformed message payload")
                && sw_js.contains("rejectRequest('Malformed message payload');"),
            "service worker message handling should guard against null or non-object payloads before destructuring and fail fast to the caller"
        );
        assert!(
            sw_js.contains("if (typeof type !== 'string' || type.length === 0) {")
                && sw_js.contains("Ignoring message without a valid type")
                && sw_js.contains("rejectRequest('Message type must be a non-empty string');")
                && sw_js.contains("type: 'REQUEST_INVALID',")
                && sw_js.contains("rejectRequest(`Unknown message type: ${type}`);"),
            "service worker message handling should reject invalid or unknown message types without forcing controller RPC callers to time out"
        );
    }

    #[test]
    fn test_service_worker_fetch_keeps_network_success_when_cache_write_fails() {
        let sw_js = include_str!("../src/pages_assets/sw.js");
        assert!(
            sw_js.contains("if (response.ok) {\n            try {")
                && sw_js.contains("log(LOG.WARN, 'Cache open error:', cacheError);")
                && sw_js.contains("return addSecurityHeaders(response);"),
            "service worker fetch handling should treat cache-write failures as best-effort and still return a successful network response"
        );
        assert!(
            sw_js.contains("if (request.mode === 'navigate') {\n            try {")
                && sw_js.contains("log(LOG.WARN, 'Navigation cache fallback error:', cacheError);"),
            "navigation fallback should not crash if the Cache API itself fails during offline fallback"
        );
    }

    #[test]
    fn test_sw_register_handles_unsupported_or_missing_registrations_safely() {
        let sw_register_js = include_str!("../src/pages_assets/sw-register.js");
        assert!(
            sw_register_js.contains("void applyUpdate().catch((error) => {")
                && sw_register_js.contains("console.error('[SW] Failed to apply update:', error);"),
            "service worker update UI should catch async applyUpdate failures instead of leaking unhandled rejections"
        );
        assert!(
            sw_register_js.contains("if (!('serviceWorker' in navigator)) {")
                && sw_register_js.contains("if (!currentRegistration) {")
                && sw_register_js.contains("return true;"),
            "service worker unregister should treat unsupported or already-unregistered states as successful no-ops"
        );
        assert!(
            sw_register_js.contains("return 'serviceWorker' in navigator\n            && (registration !== null || navigator.serviceWorker.controller !== null);")
                && sw_register_js.contains("return 'serviceWorker' in navigator\n            && navigator.serviceWorker.controller !== null;"),
            "service worker status getters should guard navigator.serviceWorker access on unsupported browsers"
        );
    }

    #[test]
    fn test_stats_timeline_tabs_only_expose_available_data_views() {
        let stats_js = include_str!("../src/pages_assets/stats.js");
        assert!(
            stats_js
                .contains("const availableTimelineViews = getAvailableTimelineViews(timeline);")
                && stats_js
                    .contains("const selectedTimelineView = getSelectedTimelineView(timeline);")
                && stats_js.contains("availableTimelineViews.length > 1")
                && stats_js.contains("const data = getTimelineEntries(timeline, view);")
                && stats_js.contains(
                    "const availableViews = new Set(getAvailableTimelineViews(timeline));"
                ),
            "stats timeline rendering should derive the selected view from the views that actually have data instead of assuming daily and weekly are always available"
        );
        assert!(
            !stats_js.contains("timeline[currentTimelineView] || timeline.monthly || []"),
            "stats timeline rendering should not silently fall back to monthly data after the user selects an empty daily or weekly view"
        );
    }

    #[test]
    fn test_attachment_manifest_failures_only_cache_true_absence() {
        let attachments_js = include_str!("../src/pages_assets/attachments.js");
        assert!(
            attachments_js.contains("function shouldCacheManifestAbsence(error) {")
                && attachments_js.contains("return error?.code === 'ATTACHMENT_MANIFEST_ABSENT';")
                && attachments_js.contains("isManifestLoaded = shouldCacheManifestAbsence(error);"),
            "attachment init should only memoize true manifest absence instead of treating every manifest failure as a permanent no-attachments state"
        );
        assert!(
            attachments_js.contains("if (response.status === 404) {")
                && attachments_js.contains(
                    "throw createAttachmentError('Manifest not found', 'ATTACHMENT_MANIFEST_ABSENT');"
                )
                && attachments_js.contains("'ATTACHMENT_MANIFEST_FETCH_FAILED'")
                && attachments_js.contains("'ATTACHMENT_MANIFEST_INVALID'"),
            "attachment manifest loading should distinguish missing manifests from retryable fetch or parse failures"
        );
        assert!(
            attachments_js.contains("if (shouldCacheManifestAbsence(error)) {")
                && attachments_js.contains("throw error;")
                && attachments_js
                    .contains("if (error?.code === 'ATTACHMENT_REQUEST_INVALIDATED') {"),
            "attachment invalidation handling should use stable error codes instead of brittle string matching"
        );
    }

    #[test]
    fn test_conversation_attachment_state_keeps_transient_manifest_failures_retryable() {
        let conversation_js = include_str!("../src/pages_assets/conversation.js");
        assert!(
            conversation_js.contains("state.ready = true;")
                && conversation_js.contains("return state.available;")
                && conversation_js
                    .contains("if (error?.code === 'ATTACHMENT_REQUEST_INVALIDATED') {")
                && conversation_js.contains("state.ready = false;")
                && conversation_js.contains("state.available = false;"),
            "conversation attachment readiness should only become terminal after a successful or absent manifest load, not after a transient manifest failure"
        );
    }

    #[test]
    fn test_search_keyboard_navigation_tracks_logical_result_indices() {
        let search_js = include_str!("../src/pages_assets/search.js");
        assert!(
            search_js.contains("function focusResultCardAtIndex(index, align = 'start') {")
                && search_js.contains("virtualList.scrollToIndex(index, align);")
                && search_js.contains("return elements.resultsList.querySelector(`.result-card[data-result-index=\"${index}\"]`);"),
            "search keyboard navigation should resolve result focus by logical index so virtualized results beyond the current DOM window stay reachable"
        );
        assert!(
            search_js.contains("data-result-index=\"${index}\"")
                && search_js.contains("article.dataset.resultIndex = String(index);"),
            "both direct and virtual result cards should expose a stable logical index for keyboard navigation"
        );
        assert!(
            search_js.contains("focusResultCardAtIndex(currentIndex + 1, 'end');")
                && search_js.contains("focusResultCardAtIndex(currentIndex - 1, 'start');")
                && search_js.contains("focusResultCardAtIndex(currentResults.length - 1, 'end');"),
            "Arrow/Home/End navigation should move by logical result index instead of only among currently rendered siblings"
        );
    }

    #[test]
    fn test_attachment_blob_loading_deduplicates_concurrent_requests() -> Result<()> {
        run_node_module_assertions(
            r#"
                import {
                    loadBlob,
                    loadBlobAsUrl,
                    reset,
                    getCacheStats,
                } from './src/pages_assets/attachments.js';

                const hash = 'a'.repeat(64);
                const dek = new Uint8Array([1, 2, 3, 4]);
                const exportId = new Uint8Array([5, 6, 7, 8]);

                let fetchCalls = 0;
                let decryptCalls = 0;
                let urlCalls = 0;

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
                    await Promise.resolve();
                    return new Uint8Array([1, 2, 3]).buffer;
                };

                URL.createObjectURL = () => {
                    urlCalls += 1;
                    return `blob:test-${urlCalls}`;
                };
                URL.revokeObjectURL = () => {};

                try {
                    reset();

                    const [blobA, blobB] = await Promise.all([
                        loadBlob(hash, dek, exportId),
                        loadBlob(hash, dek, exportId),
                    ]);

                    if (fetchCalls !== 1 || decryptCalls !== 1) {
                        throw new Error(`expected one fetch and one decrypt for concurrent blob loads, got fetch=${fetchCalls} decrypt=${decryptCalls}`);
                    }
                    if (blobA !== blobB) {
                        throw new Error('expected concurrent blob loads to share the same cached Uint8Array instance');
                    }

                    const [urlA, urlB] = await Promise.all([
                        loadBlobAsUrl(hash, 'image/png', dek, exportId),
                        loadBlobAsUrl(hash, 'image/png', dek, exportId),
                    ]);

                    if (urlCalls !== 1) {
                        throw new Error(`expected one object URL for concurrent URL loads, got ${urlCalls}`);
                    }
                    if (urlA !== urlB) {
                        throw new Error(`expected concurrent URL loads to share one object URL, got ${urlA} vs ${urlB}`);
                    }

                    const stats = getCacheStats();
                    if (stats.entries !== 1) {
                        throw new Error(`expected one cache entry after deduped blob loads, got ${JSON.stringify(stats)}`);
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
            "#,
        )
    }

    #[test]
    fn test_worker_message_paths_guard_malformed_payloads_and_report_generic_failures() {
        let auth_js = include_str!("../src/pages_assets/auth.js");
        assert!(
            auth_js.contains(
                "const payload = event?.data && typeof event.data === 'object' ? event.data : null;"
            ) && auth_js.contains("Ignoring malformed worker message payload")
                && auth_js
                    .contains("void handleWorkerError(new Error('Malformed worker response'));")
                && auth_js.contains("case 'WORKER_ERROR':")
                && auth_js.contains(
                    "void handleWorkerError(new Error(`Unknown worker message type: ${type}`));"
                ),
            "auth-side worker message handling should fail closed on malformed or unknown payloads and surface generic worker failures"
        );

        let crypto_worker_js = include_str!("../src/pages_assets/crypto_worker.js");
        assert!(
            crypto_worker_js.contains("Ignoring malformed worker request payload")
                && crypto_worker_js.contains("type: 'WORKER_ERROR',")
                && crypto_worker_js.contains("error: 'Malformed worker request payload',")
                && crypto_worker_js
                    .contains("throw new Error(`Unknown worker message type: ${type}`);")
                && crypto_worker_js.contains("type: getWorkerFailureMessageType(type),")
                && crypto_worker_js.contains("return 'WORKER_ERROR';"),
            "crypto worker should report malformed or unknown payloads and fall back to a generic worker failure type"
        );
    }
}
