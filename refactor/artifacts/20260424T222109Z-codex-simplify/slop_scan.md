# AI slop scan — 20260424T222109Z-codex-simplify

Generated 2026-04-24T22:21:55Z
Scope: `src`

(See references/VIBE-CODED-PATHOLOGIES.md for P1-P40 catalog.)


## P1 over-defensive try/catch (Python: ≥3 except Exception per file)

_none found_

## P1 over-defensive try/catch (TS: catch blocks per file)

_none found_

## P2 long nullish/optional chains (three+ `?.`)

_none found_

## P2 double-nullish coalescing

_none found_

## P3 orphaned _v2/_new/_old/_improved/_copy files

_none found_

## P4 utils/helpers/misc/common files > 500 LOC

_none found_

## P5 abstract Base/Abstract class hierarchy

_none found_

## P5 abstract class in Rust (rare idiom; often AI-generated)

_none found_

## P6 feature flags (review each for whether it is still toggling)

```
ENABLE_FOOTNOTES
ENABLE_SMART_PUNCTUATION
ENABLE_STRIKETHROUGH
ENABLE_TABLES
ENABLE_TASKLISTS
LEGACY_COMPAT_PAGE_SIZE
LEGACY_OPFS_DB_FILES
LEGACY_PREF_KEYS
LEGACY_SESSION_KEYS
```

## P7 re-export barrel files (`export * from`)

_none found_

## P8 pass-through wrappers (function whose sole body returns another call)

_none found_

## P9 functions with ≥5 optional parameters

_none found_

## P10 swallowed catch (empty or `return null`)

_none found_

## P10 Python: except ... : pass

_none found_

## P11 Step/Phase/TODO comments (per-file counts)

```
src/pages/wizard.rs:14
src/sources/setup.rs:8
src/pages/deploy_github.rs:7
src/pages/deploy_cloudflare.rs:6
src/indexer/refresh_ledger.rs:4
src/ui/app.rs:3
src/search/two_tier_search.rs:2
src/search/query.rs:2
src/indexer/responsiveness.rs:2
src/lib.rs:1
src/indexer/mod.rs:1
```

## P12 many-import files (top 20)

_none found_

## P14 mocks (jest.mock, vi.mock, sinon.stub, __mocks__)

_none found_

## P15 TS `any` usage (per-file counts, top 20)

_none found_

## P16 *Error enums in Rust (often duplicate variants)

```
src/storage/sqlite.rs:48:pub enum LazyDbError {
src/storage/sqlite.rs:766:pub enum MigrationError {
src/html_export/encryption.rs:20:pub enum EncryptionError {
src/html_export/template.rs:20:pub enum TemplateError {
src/analytics/types.rs:15:pub enum AnalyticsError {
src/html_export/renderer.rs:26:pub enum RenderError {
src/sources/sync.rs:178:pub enum SyncError {
src/sources/config.rs:92:pub enum ConfigError {
src/sources/index.rs:60:pub enum IndexError {
src/sources/interactive.rs:603:pub enum InteractiveError {
src/sources/setup.rs:209:pub enum SetupError {
src/ui/style_system.rs:315:pub enum ThemeConfigError {
src/pages/size.rs:239:pub enum SizeError {
src/search/model_download.rs:918:pub enum DownloadError {
src/sources/install.rs:65:pub enum InstallError {
src/search/two_tier_search.rs:784:pub enum TwoTierError {
src/pages/preview.rs:19:pub enum PreviewError {
src/pages/errors.rs:21:pub enum DecryptError {
src/pages/errors.rs:120:pub enum DbError {
src/pages/errors.rs:190:pub enum BrowserError {
src/pages/errors.rs:252:pub enum NetworkError {
src/pages/errors.rs:303:pub enum ExportError {
src/pages/config_input.rs:51:pub enum ConfigError {
src/search/semantic_manifest.rs:693:pub enum ManifestError {
```

## P17 heavily drilled props (top 10 most-passed via JSX)

_none found_

## P18 everything hook (custom hook file with many useState/useEffect)

_none found_

## P19 N+1 pattern (await inside for loop)

_none found_

## P19 Python N+1 (for ... : await)

_none found_

## P20 config files (candidates for unification)

```
./tests/e2e/.env.test
```

## P22 stringly-typed status/state comparisons

_none found_

## P22 Rust stringly-typed status/state comparisons

```
src/lib.rs:6279:    if status == "error" && reason.contains("quarantin") {
src/lib.rs:6282:    if status == "stale"
src/lib.rs:15453:    let fail_count = checks.iter().filter(|c| c.status == "fail").count();
src/lib.rs:15454:    let warn_count = checks.iter().filter(|c| c.status == "warn").count();
src/lib.rs:15459:    let all_pass = checks.iter().all(|c| c.status == "pass");
src/lib.rs:15520:            if check.status == "pass" && !verbose {
src/lib.rs:26410:        let passed = checks.iter().filter(|c| c.status == "pass").count();
src/lib.rs:26411:        let warnings = checks.iter().filter(|c| c.status == "warn").count();
src/lib.rs:26412:        let failed = checks.iter().filter(|c| c.status == "fail").count();
src/pages/verify.rs:1330:    let status_icon = if result.status == "valid" {
src/ui/app.rs:29256:            app.status == "Exporting markdown...",
src/ui/app.rs:29283:            app.status == "Exporting markdown...",
```

## P23 reflex trim/lower/upper normalization

```
src/tui_asciicast.rs:207:            .and_then(|raw| raw.trim().parse::<u16>().ok())
src/ftui_harness.rs:207:        let profile = profile.trim();
src/bookmarks.rs:226:            .to_lowercase()
src/html_export/renderer.rs:729:    let content_section = if content_html.trim().is_empty() {
src/html_export/renderer.rs:856:    let popover_input = if !formatted_input.trim().is_empty() {
src/html_export/renderer.rs:865:    let popover_output = if !formatted_output.trim().is_empty() {
src/html_export/renderer.rs:908:    match tool_name.to_lowercase().as_str() {
src/html_export/renderer.rs:1042:    let content_section = if content_html.trim().is_empty() {
src/html_export/renderer.rs:1154:    let trimmed = dest_url.trim();
src/html_export/renderer.rs:1206:    let tool_icon = match tool_call.name.to_lowercase().as_str() {
src/html_export/renderer.rs:1231:    let popover_input = if !input_preview.trim().is_empty() {
src/update_check.rs:1223:        let err_chain = format!("{:?}", err).to_lowercase();
src/main.rs:4:        .map(|value| value.trim().to_ascii_lowercase())
src/main.rs:13:        .map(|value| value.trim().to_ascii_lowercase())
src/main.rs:47:    if err.message.trim().starts_with('{') {
src/html_export/scripts.rs:201:        const query = this.input.value.trim().toLowerCase();
src/html_export/scripts.rs:218:                const text = node.textContent.toLowerCase();
src/html_export/scripts.rs:418:            btn.dataset.originalText = btn.textContent.trim();
src/html_export/scripts.rs:511:        if (input && input.trim()) {
src/html_export/scripts.rs:515:        if (output && output.trim()) {
src/html_export/filename.rs:395:    match agent.to_lowercase().replace(['-', '_'], "").as_str() {
src/lib.rs:48:                .filter(|s| !s.trim().is_empty())
src/lib.rs:1489:    let trimmed = agent.trim();
src/lib.rs:1664:        match s.to_lowercase().as_str() {
src/lib.rs:2036:            let flag_lower = flag_part.to_lowercase();
src/lib.rs:2057:            let flag_lower = flag_part.to_lowercase();
src/lib.rs:2103:            let lower = normalized_arg.to_lowercase();
src/lib.rs:2246:    let raw_str = raw.join(" ").to_lowercase();
src/lib.rs:2714:                        if friendly.trim().starts_with('{') {
src/lib.rs:2729:                if friendly.trim().starts_with('{') {
src/lib.rs:3411:                        let target_name = pages_config.deployment.target.to_lowercase();
src/lib.rs:3703:                                    if confirm.trim().to_lowercase() != "y" {
src/lib.rs:4215:        let trimmed = w.trim();
src/lib.rs:4241:        .map(|workspace| workspace.trim())
src/lib.rs:6448:                v.trim().to_ascii_lowercase().as_str(),
src/lib.rs:6473:    if term.trim().eq_ignore_ascii_case("dumb") && dotenvy::var("TUI_HEADLESS").is_err() {
src/lib.rs:6793:    if line.trim().is_empty() || width.is_none() {
src/lib.rs:6825:        for lower_ch in ch.to_lowercase() {
src/lib.rs:6894:        let lower_term = term.to_lowercase();
src/lib.rs:7715:                AggregateField::MatchType => format!("{:?}", hit.match_type).to_lowercase(),
src/lib.rs:7760:        let trimmed = field.trim();
src/lib.rs:8902:    if hit.origin_kind.trim().eq_ignore_ascii_case("local") {
src/lib.rs:8905:    hit.source_id.trim().to_string()
src/lib.rs:8909:    let trimmed = hit.origin_kind.trim();
src/lib.rs:9223:        .and_then(|val| match val.trim().to_ascii_lowercase().as_str() {
src/lib.rs:9233:                match val.trim().to_ascii_lowercase().as_str() {
src/lib.rs:9282:        Ok(v) if !v.trim().is_empty() => match v.parse::<usize>() {
src/lib.rs:9293:        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
src/lib.rs:10258:                    .unwrap_or_else(|| id.trim().to_string()),
src/lib.rs:10291:                    .unwrap_or_else(|| id.trim().to_string())
src/lib.rs:12047:        .and_then(|value| value.trim().parse::<usize>().ok())
src/lib.rs:13244:        let message = err.to_string().to_lowercase();
src/lib.rs:14942:                        Some(status) if status.trim().eq_ignore_ascii_case("ok") => {
src/lib.rs:14988:                                    status.trim(),
src/lib.rs:18233:    let trimmed = source_id?.trim();
src/lib.rs:18315:    let trimmed = source_id.trim();
src/lib.rs:18449:        if line.trim().is_empty() {
src/lib.rs:19134:            let v = v.trim().to_ascii_lowercase();
src/lib.rs:19429:            .and_then(|v| v.trim().parse::<u64>().ok())
src/lib.rs:19709:        let trimmed = dir.trim();
src/lib.rs:19735:        let trimmed = line.trim();
src/lib.rs:19776:    if !matches!(input.trim(), "y" | "Y") {
src/lib.rs:19818:        let w0 = window[0].to_string_lossy().to_lowercase();
src/lib.rs:19819:        let w1 = window[1].to_string_lossy().to_lowercase();
src/lib.rs:19820:        let w2 = window[2].to_string_lossy().to_lowercase();
src/lib.rs:20041:                        && !text.trim().is_empty()
src/lib.rs:20049:                        && !output.trim().is_empty()
src/lib.rs:20056:                        && !text.trim().is_empty()
src/lib.rs:20063:                        && !text.trim().is_empty()
src/lib.rs:20073:        if assembled_content.trim().is_empty() {
src/lib.rs:20177:        let normalized = raw.trim().to_ascii_lowercase();
src/lib.rs:20368:        let trimmed = line.trim();
src/lib.rs:21983:    let has_content = !msg.content.trim().is_empty();
src/lib.rs:22801:                .any(|(source_id, _)| source_id.trim() != source_id || source_id.is_empty()),
src/lib.rs:25039:                        let icon = match name.to_lowercase().as_str() {
src/lib.rs:25195:        if raw_line.trim().is_empty() {
src/lib.rs:25385:    let trimmed = content.trim();
src/lib.rs:25392:        let after = trimmed[close_idx + 1..].trim();
src/lib.rs:25409:    let s = s.trim();
src/lib.rs:26115:    if host.trim().is_empty()
```

## P24 testability wrappers / mutable deps seams

_none found_

## P25 docstrings/comments that may contradict implementation

```
src/lib.rs:26141:    // Auto-generated remote names must not collide with the built-in local source ID.
src/pages/bundle.rs:946:Generated by cass v{}
src/pages/docs.rs:325:Generated by CASS v{version} on {date}
src/pages/docs.rs:408:Generated by CASS v{version}
src/pages_assets/attachments.js:62: * @returns {Promise<object|null>} Manifest or null if no attachments
src/pages_assets/attachments.js:164: * @returns {boolean}
src/pages_assets/attachments.js:172: * @returns {object|null}
src/pages_assets/attachments.js:181: * @returns {Array} Attachment entries for this message
src/pages_assets/attachments.js:196: * @returns {Promise<Uint8Array>} Decrypted blob data
src/pages_assets/attachments.js:281: * @returns {Promise<string>} Object URL
src/pages_assets/attachments.js:541: * @returns {object} Cache stats
src/pages_assets/attachments.js:558: * @returns {HTMLElement} DOM element for the attachment
src/pages_assets/password-strength.js:41: * @returns {ValidationResult} Validation result with strength and suggestions
src/pages_assets/password-strength.js:123: * @returns {number} Estimated entropy in bits
src/pages_assets/password-strength.js:153: * @returns {string} CSS color value
src/pages_assets/password-strength.js:169: * @returns {number} Percentage (25, 50, 75, or 100)
src/pages_assets/password-strength.js:185: * @returns {string} Capitalized label
src/pages_assets/password-strength.js:200: * @returns {Object} Meter controller with update() and destroy() methods
src/pages_assets/password-strength.js:274: * @returns {string} Escaped string
src/pages_assets/index.html:123:            <p>Generated by <a href="https://github.com/Dicklesworthstone/coding_agent_session_search" target="_blank" rel="noopener">cass</a></p>
src/pages_assets/conversation.js:992: * @returns {Object} Cache stats
src/pages_assets/share.js:16: * @returns {string} Base URL
src/pages_assets/share.js:30: * @returns {string} Shareable URL
src/pages_assets/share.js:42: * @returns {string} Shareable URL
src/pages_assets/share.js:52: * @returns {string} Shareable URL
src/pages_assets/share.js:61: * @returns {string} Shareable URL
src/pages_assets/share.js:70: * @returns {string} Shareable URL
src/pages_assets/share.js:80: * @returns {Promise<boolean>} True if successful
src/pages_assets/share.js:124: * @returns {Promise<boolean>} True if successful
src/pages_assets/share.js:134: * @returns {Promise<{success: boolean, link: string}>} Result
src/pages_assets/share.js:146: * @returns {Promise<{success: boolean, link: string}>} Result
src/pages_assets/share.js:160: * @returns {Promise<boolean>} True if shared successfully
src/pages_assets/share.js:185: * @returns {Promise<boolean>} True if shared successfully
src/pages_assets/share.js:201: * @returns {boolean} True if available
src/pages_assets/share.js:210: * @returns {Object|null} Parsed route info or null if invalid
src/pages_assets/coi-detector.js:50: * @returns {boolean}
src/pages_assets/coi-detector.js:96: * @returns {boolean}
src/pages_assets/coi-detector.js:104: * @returns {Promise<boolean>}
src/pages_assets/coi-detector.js:113: * @returns {Promise<boolean>}
src/pages_assets/coi-detector.js:121: * @returns {boolean}
src/pages_assets/coi-detector.js:129: * @returns {boolean}
src/pages_assets/coi-detector.js:142: * @returns {Promise<string>} One of COI_STATE values
src/pages_assets/coi-detector.js:173: * @returns {Object} Configuration object
src/pages_assets/virtual-list.js:264:     * @returns {{start: number, end: number}} Visible item range
src/pages_assets/virtual-list.js:272:     * @returns {Object} Metrics object
src/pages_assets/router.js:180:     * @returns {Object} Current route
src/pages_assets/router.js:208:     * @returns {Object} Parsed route
src/pages_assets/router.js:240:     * @returns {Object} Matched view and params
src/pages_assets/router.js:287: * @returns {Router} Router instance
src/pages_assets/router.js:301: * @returns {Router|null} Router instance or null
src/pages_assets/router.js:322: * @returns {Object} Current route
src/pages_assets/router.js:332: * @returns {string} Path string
src/pages_assets/router.js:345: * @returns {string} Path string
src/pages_assets/router.js:378: * @returns {Object} Search parameters
src/pages_assets/auth.js:432: * Returns: { valid: true, isFirstVisit: boolean } or { valid: false, reason: string, previousFingerprint: string }
src/pages_assets/stats.js:38: * @returns {Promise<Object>} Analytics data
src/pages_assets/stats.js:82: * @returns {Promise<Object>} Analytics bundle
src/pages_assets/stats.js:125: * @returns {Object} Analytics data
src/pages_assets/stats.js:545: * @returns {string} HTML string
src/pages_assets/stats.js:559: * @returns {string} HTML string
src/pages_assets/stats.js:579: * @returns {Array} Timeline entries
src/pages_assets/stats.js:611: * @returns {string} SVG HTML string
src/pages_assets/stats.js:676: * @returns {string} Label
src/pages_assets/stats.js:688: * @returns {string} HTML string
src/pages_assets/stats.js:713: * @returns {string} HTML string
src/pages_assets/stats.js:793: * @returns {string} Formatted name
src/pages_assets/stats.js:813: * @returns {string} Formatted date
src/pages_assets/stats.js:829: * @returns {string} Relative time string
src/pages_assets/stats.js:854: * @returns {string} Formatted number
src/pages_assets/stats.js:864: * @returns {string} Escaped text
src/pages_assets/stats.js:889: * @returns {Object|null} Analytics data or null
src/pages_assets/database.js:18: * @returns {Promise<void>}
src/pages_assets/database.js:110: * @returns {*} Result from callback
src/pages_assets/database.js:132: * @returns {Array<Object>} Array of row objects
src/pages_assets/database.js:148: * @returns {Object|null} Row object or null
src/pages_assets/database.js:160: * @returns {*} Scalar value or null
src/pages_assets/database.js:172: * @returns {number} Number of affected rows
src/pages_assets/database.js:189: * @returns {Object} Metadata key-value pairs
src/pages_assets/database.js:202: * @returns {Object} Statistics object
src/pages_assets/database.js:216: * @returns {Array<Object>} Conversation objects
```

## P26 TypeScript type assertions

_none found_

## P27 addEventListener sites (audit for cleanup)

_none found_

## P28 timers (audit for clearTimeout/clearInterval cleanup)

_none found_

## P29 regex construction in functions/loops

```
src/bookmarks.rs:503:        let results = store.search("auth").unwrap();
src/bookmarks.rs:539:        let percent_results = store.search("%").unwrap();
src/bookmarks.rs:543:        let underscore_results = store.search("_").unwrap();
src/bookmarks.rs:547:        let backslash_results = store.search("\\").unwrap();
src/pages/summary.rs:352:        let regex = Regex::new(pattern).context("Invalid exclusion pattern")?;
src/pages/summary.rs:396:            let regex = Regex::new(pattern_str)
src/pages/patterns.rs:298:        let regex = Regex::new(self.pattern).ok()?;
src/pages/patterns.rs:415:            let result = Regex::new(pattern.pattern);
src/pages/patterns.rs:468:        let pattern = Regex::new(AWS_ACCESS_KEY.pattern).unwrap();
src/pages/patterns.rs:475:        let pattern = Regex::new(OPENAI_KEY.pattern).unwrap();
src/pages/patterns.rs:482:        let pattern = Regex::new(EMAIL_ADDRESS.pattern).unwrap();
src/pages/patterns.rs:489:        let pattern = Regex::new(EMAIL_ADDRESS.pattern).unwrap();
src/pages/patterns.rs:499:        let pattern = Regex::new(DATABASE_URL.pattern).unwrap();
src/pages/patterns.rs:507:        let pattern = Regex::new(SSH_PRIVATE_KEY.pattern).unwrap();
src/indexer/redact_secrets.rs:33:            regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws access key regex"),
src/indexer/redact_secrets.rs:38:            regex: Regex::new(
src/indexer/redact_secrets.rs:46:            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").expect("github pat regex"),
src/indexer/redact_secrets.rs:51:            regex: Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("openai key regex"),
src/indexer/redact_secrets.rs:56:            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9]{20,}\b").expect("anthropic key regex"),
src/indexer/redact_secrets.rs:61:            regex: Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_\-.]{20,}").expect("bearer token regex"),
src/indexer/redact_secrets.rs:66:            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b")
src/indexer/redact_secrets.rs:72:            regex: Regex::new(r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PGP) PRIVATE KEY-----")
src/indexer/redact_secrets.rs:78:            regex: Regex::new(
src/indexer/redact_secrets.rs:86:            regex: Regex::new(
src/indexer/redact_secrets.rs:94:            regex: Regex::new(r"\bxox[bpsar]-[A-Za-z0-9\-]{10,}").expect("slack token regex"),
src/indexer/redact_secrets.rs:99:            regex: Regex::new(r"\b[spr]k_live_[A-Za-z0-9]{20,}").expect("stripe key regex"),
src/pages/redact.rs:272:    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b")
src/pages/redact.rs:277:    Regex::new(
src/pages/redact.rs:370:    let regex = Regex::new(&pattern).ok()?;
src/pages/redact.rs:487:            pattern: Regex::new(r"Project\s+Falcon").unwrap(),
src/pages/secret_scan.rs:176:            regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws access key regex"),
src/pages/secret_scan.rs:181:            regex: Regex::new(
src/pages/secret_scan.rs:189:            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").expect("github pat regex"),
src/pages/secret_scan.rs:197:            regex: Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("openai key regex"),
src/pages/secret_scan.rs:202:            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9]{20,}\b").expect("anthropic key regex"),
src/pages/secret_scan.rs:207:            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b")
src/pages/secret_scan.rs:213:            regex: Regex::new(
src/pages/secret_scan.rs:221:            regex: Regex::new(r"(?i)\b(postgres|postgresql|mysql|mongodb|redis)://[^\s]+")
src/pages/secret_scan.rs:227:            regex: Regex::new(
src/pages/secret_scan.rs:236:    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{20,}").expect("entropy base64 regex"));
src/pages/secret_scan.rs:238:    Lazy::new(|| Regex::new(r"\b[A-Fa-f0-9]{32,}\b").expect("entropy hex regex"));
src/pages/secret_scan.rs:985:        let regex = Regex::new(pat).with_context(|| format!("Invalid {} regex: {}", label, pat))?;
```

## P30 debug print/log leftovers

```
src/main.rs:49:        println!("{}", err.message);
src/main.rs:61:        println!("{payload}");
src/main.rs:64:        eprintln!("{}", err.message);
src/main.rs:96:            eprintln!(
src/pages_assets/search.js:744:    console.debug(`[Search] Using virtual scrolling for ${currentResults.length} results`);
src/pages_assets/sw.js:64:        console.log(...prefix, `[${levelName}]`, ...args);
src/update_check.rs:342:        eprintln!("Invalid version string: {}", version);
src/update_check.rs:363:        eprintln!("Failed to run installer: {}", err);
src/update_check.rs:387:                eprintln!("Failed to run installer: {}", e);
src/update_check.rs:414:        eprintln!(
src/pages_assets/viewer.js:69:    console.log('[Viewer] Initializing...');
src/pages_assets/viewer.js:81:            console.log('[Viewer] Waiting for database re-open...');
src/pages_assets/viewer.js:91:        console.log('[Viewer] Waiting for database...');
src/pages_assets/viewer.js:112:    console.log('[Viewer] Database ready:', event.detail);
src/pages_assets/viewer.js:170:    console.log('[Viewer] Initialized with hash-based routing');
src/pages_assets/viewer.js:278:    console.debug('[Viewer] Route change:', route);
src/pages_assets/viewer.js:346:        console.debug('[Viewer] Search route from URL:', searchParams);
src/pages_assets/viewer.js:853:    console.log('[Viewer] Cleaned up');
src/pages_assets/conversation.js:102:            console.debug(`[Conversation] Using cached conversation ${conversationId}`);
src/pages_assets/conversation.js:128:            console.debug(`[Conversation] Loaded and cached conversation ${conversationId} (cache size: ${loadedConversations.size})`);
src/pages_assets/conversation.js:303:    console.debug(`[Conversation] Using virtual scrolling for ${messages.length} messages`);
src/pages_assets/conversation.js:896:        console.debug(`[Conversation] Unloaded oldest conversation ${oldest} (cache size: ${loadedConversations.size})`);
src/pages_assets/conversation.js:913:        console.debug(`[Conversation] Cleared ${toRemove} old conversations (cache size: ${loadedConversations.size})`);
src/pages_assets/conversation.js:976:    console.debug('[Conversation] Memory monitoring started');
src/pages_assets/conversation.js:986:        console.debug('[Conversation] Memory monitoring stopped');
src/pages_assets/conversation.js:1030:    console.debug('[Conversation] All cached conversations cleared');
src/pages_assets/auth.js:760:        console.debug('QR scan:', error);
src/pages_assets/auth.js:778:        console.debug('Ignoring stale worker message:', type, data.requestId);
src/pages_assets/auth.js:1531:        console.log('[App] COI initialization complete, state:', state);
src/pages_assets/storage.js:195:    console.log('[Storage] Initializing...');
src/pages_assets/storage.js:211:    console.log('[Storage] Restored mode:', currentMode);
src/pages_assets/storage.js:303:    console.log('[Storage] Mode changed:', oldMode, '->', mode);
src/pages_assets/storage.js:323:    console.log('[Storage] OPFS initialized');
src/pages_assets/storage.js:520:            console.log('[Storage] Binary data written to OPFS:', fullKey);
src/pages_assets/storage.js:562:    console.log('[Storage] Migrating from', fromMode, 'to', toMode);
src/pages_assets/storage.js:637:    console.log('[Storage] Migrated', keys.length, 'items');
src/pages_assets/storage.js:705:    console.log('[Storage] Clearing current storage:', currentMode);
src/pages_assets/storage.js:785:        console.log('[Storage] OPFS cleared:', entries.length, 'entries');
src/pages_assets/storage.js:799:    console.log('[Storage] Clearing all storage');
src/pages_assets/storage.js:850:    console.log('[Storage] All storage cleared');
src/pages_assets/storage.js:861:        console.log('[Storage] Cache API not available');
src/pages_assets/storage.js:877:            console.log('[Storage] Service Worker caches cleared:', cassNames);
src/pages_assets/storage.js:905:            console.log('[Storage] Service Workers unregistered');
src/pages_assets/virtual-list.js:86:        console.debug('[VirtualList] Initialized with', this.totalCount, 'items');
src/pages_assets/virtual-list.js:214:        console.debug(`[VirtualList] Rendering ${this.items.size} of ${this.totalCount} items (range: ${start}-${end})`);
src/pages_assets/virtual-list.js:304:        console.debug('[VirtualList] Destroyed. Metrics:', this.metrics);
src/pages_assets/virtual-list.js:402:        console.debug('[VariableVirtualList] Initialized with', this.totalCount, 'items');
src/pages_assets/virtual-list.js:552:        console.debug(`[VariableVirtualList] Rendering ${this.items.size} of ${this.totalCount} items`);
src/pages_assets/virtual-list.js:660:        console.debug('[VariableVirtualList] Destroyed');
src/pages_assets/coi-detector.js:147:        console.log('[COI] Service Workers not supported - degraded mode');
src/pages_assets/coi-detector.js:155:    console.log('[COI] State check:', { swActive, coiEnabled, sabAvailable });
src/pages_assets/coi-detector.js:444:    console.log('[COI] Initial state:', state);
src/pages_assets/coi-detector.js:448:        console.log('[COI] Setup already complete - fast path');
src/pages_assets/coi-detector.js:474:                console.log('[COI] State after SW ready:', state);
src/pages_assets/coi-detector.js:486:            console.log('[COI] Ready - proceeding to auth');
src/pages_assets/coi-detector.js:493:            console.log('[COI] Needs reload - showing prompt');
src/pages_assets/coi-detector.js:497:                onReload: () => console.log('[COI] Reloading...'),
src/pages_assets/coi-detector.js:506:            console.log('[COI] Degraded mode - showing warning and proceeding');
src/pages_assets/coi-detector.js:515:            console.log('[COI] SW still installing - checking fallback');
src/pages_assets/coi-detector.js:533:                    onReload: () => console.log('[COI] Reloading...'),
src/pages_assets/coi-detector.js:565:                console.log('[COI] Service worker activation detected:', reason);
src/pages_assets/sw-register.js:99:        console.log('[SW] Registered, scope:', registration.scope);
src/pages_assets/sw-register.js:107:        console.log('[SW] Ready');
src/pages_assets/sw-register.js:111:            console.log('[SW] SharedArrayBuffer available');
src/pages_assets/sw-register.js:155:                    console.log('[SW] Update available');
src/pages_assets/sw-register.js:160:                    console.log('[SW] First install complete');
src/pages_assets/sw-register.js:169:            console.log('[SW] Controller changed');
src/pages_assets/sw-register.js:293:        console.log('[SW] Unregistered');
src/pages_assets/sw-register.js:306:        console.log('[SW] Cache cleared');
src/pages_assets/database.js:26:    console.log('[DB] Initializing sqlite-wasm...');
src/pages_assets/database.js:37:            console.log('[DB] Loaded from OPFS');
src/pages_assets/database.js:53:        console.log('[DB] Loaded into memory');
src/pages_assets/database.js:510:            console.log('[DB] Closed');
src/pages_assets/router.js:104:        console.debug('[Router] Initialized');
src/pages_assets/router.js:112:        console.debug('[Router] Destroyed');
src/pages_assets/share.js:164:        console.debug('[Share] Web Share API not available');
src/pages_assets/session.js:147:        console.log(`[Session] Started, expires at ${new Date(expiry).toISOString()}`);
src/pages_assets/session.js:164:            console.log('[Session] No valid session to restore');
src/pages_assets/session.js:179:            console.log(`[Session] Restored, expires at ${new Date(expiry).toISOString()}`);
src/pages_assets/session.js:192:        console.log('[Session] Ending session');
```

## P31 JSON.stringify used as key/hash/memo identity

_none found_

## P32 money-like arithmetic (audit integer cents/decimal)

```
src/lib.rs:13000:                    let pct = (current as f64 / total as f64 * 100.0).min(100.0);
src/lib.rs:19315:                    let pct = (current as f64 / total as f64 * 100.0).min(100.0);
src/lib.rs:27343:    let total_size_mb = total_size as f64 / 1_048_576.0;
src/lib.rs:27671:    let total_size_mb = total_size as f64 / 1_048_576.0;
src/lib.rs:28180:        (outcome.conversations_processed as f64 / outcome.total_conversations as f64) * 100.0
src/lib.rs:28294:    let size_mb = total_size as f64 / 1_048_576.0;
src/analytics/query.rs:1080:        (mm.row_count as f64 / total_messages as f64) * 100.0
src/analytics/query.rs:3076:                    Some(tool_call_count as f64 / (api_tokens_total as f64 / 1000.0))
src/analytics/query.rs:3081:                    Some(tool_call_count as f64 / (content_tokens_est_total as f64 / 1000.0))
src/analytics/query.rs:3212:                Some(tool_call_count as f64 / (api_tokens_total as f64 / 1000.0))
src/analytics/query.rs:3217:                Some(tool_call_count as f64 / (content_tokens_est_total as f64 / 1000.0))
src/analytics/query.rs:6313:                let expected = row.tool_call_count as f64 / (row.api_tokens_total as f64 / 1000.0);
src/analytics/query.rs:6843:        assert!((result.totals.estimated_cost_usd - 0.5).abs() < 0.001);
src/analytics/query.rs:6940:        assert!((result.totals.estimated_cost_usd - sum_cost).abs() < 0.001);
src/analytics/query.rs:7067:        assert!((result.totals.estimated_cost_usd - 0.4).abs() < 0.001);
src/analytics/query.rs:7104:        assert!((result.totals.estimated_cost_usd - 0.0).abs() < 0.001);
src/analytics/query.rs:7143:        assert!((result.totals.estimated_cost_usd - 0.5).abs() < 0.001);
src/analytics/query.rs:7175:        assert!((result.totals.estimated_cost_usd - 0.9).abs() < 0.001);
src/analytics/derive.rs:19:        Some(bucket.tool_call_count as f64 / (bucket.api_tokens_total as f64 / 1000.0))
src/analytics/derive.rs:25:        Some(bucket.tool_call_count as f64 / (bucket.content_tokens_est_total as f64 / 1000.0))
src/analytics/types.rs:495:            Some(self.total_tool_calls as f64 / (self.total_api_tokens as f64 / 1000.0))
src/ui/analytics_charts.rs:490:            data.total_plan_messages as f64 / data.total_messages as f64 * 100.0;
src/ui/analytics_charts.rs:495:            data.plan_api_token_share = plan_token_total / data.total_api_tokens as f64 * 100.0;
src/ui/analytics_charts.rs:1131:        format!("{:.1}B", metric_total / 1_000_000_000.0)
src/ui/analytics_charts.rs:1133:        format!("{:.1}M", metric_total / 1_000_000.0)
src/ui/analytics_charts.rs:1135:        format!("{:.1}K", metric_total / 1_000.0)
src/ui/analytics_charts.rs:2602:        let pct_share = (row.tool_call_count as f64 / total_calls) * 100.0;
src/ui/analytics_charts.rs:2748:        (total_plan as f64 / total_msgs as f64) * 100.0
src/search/model_download.rs:1446:                                    ((total_downloaded as f64 / grand_total as f64) * 100.0)
src/pages/size.rs:180:                percentage: (self.total_site_bytes as f64 / MAX_SITE_SIZE_BYTES as f64 * 100.0)
src/pages/size.rs:369:                percentage: (total_size as f64 / MAX_SITE_SIZE_BYTES as f64 * 100.0) as u8,
src/pages/summary.rs:864:                (conv_count as f64 / total_conversations as f64) * 100.0
src/pages/deploy_github.rs:240:                size_check.total_bytes as f64 / (1024.0 * 1024.0),
src/search/semantic_manifest.rs:243:        let pct = (self.conversations_processed as f64 / self.total_conversations as f64) * 100.0;
src/search/model_manager.rs:226:                let mb_total = *total_bytes as f64 / 1_048_576.0;
src/search/model_manager.rs:413:                ((*bytes_present as f64 / *total_bytes as f64) * 100.0).min(100.0) as u8
src/storage/sqlite.rs:9550:            (total_inserted as f64) / (elapsed_ms as f64 / 1000.0)
src/storage/sqlite.rs:10602:        let pct = (self.priced_count as f64 / total as f64) * 100.0;
src/storage/sqlite.rs:10730:            cost += cache_read as f64 * cache_price / 1_000_000.0;
src/storage/sqlite.rs:10733:            cost += cache_creation as f64 * cache_price / 1_000_000.0;
src/indexer/refresh_ledger.rs:712:    let raw = (phase_ms as f64 / total_ms as f64) * 100.0;
src/indexer/refresh_ledger.rs:1751:            (total_share - 100.0).abs() <= 0.05,
src/ui/app.rs:3908:    let total_steps = ((clamped / 10.0) * 24.0).round() as usize;
src/ui/app.rs:18099:                    let total_mb = total as f64 / 1_048_576.0;
```

## P33 local time / UTC drift candidates

```
src/pages_assets/search.js:141:    const now = Date.now();
src/pages_assets/search.js:663:            const until = currentFilters.until || Date.now();
src/pages_assets/search.js:1003:    const now = new Date();
src/pages_assets/sw.js:62:        const prefix = ['[SW]', new Date().toISOString()];
src/pages_assets/conversation.js:126:                loadedAt: Date.now(),
src/pages_assets/stats.js:174:        computed_at: new Date().toISOString()
src/pages_assets/stats.js:835:    const now = new Date();
src/pages_assets/session.js:130:        const expiry = Date.now() + this.duration;
src/pages_assets/session.js:163:        if (!token || Date.now() > expiry) {
src/pages_assets/session.js:229:        const newExpiry = Math.max(Date.now(), currentExpiry) + extension;
src/pages_assets/session.js:265:        return Math.max(0, this.expiryTs - Date.now());
src/pages_assets/session.js:274:        const remaining = expiry - Date.now();
src/pages_assets/session.js:411:        this.lastActivity = Date.now();
src/pages_assets/session.js:452:        const now = Date.now();
src/pages_assets/session.js:467:        return Date.now() - this.lastActivity;
src/pages_assets/auth.js:258:    const remainingMs = activeSessionExpiryTs - Date.now();
src/pages_assets/auth.js:275:    if (Date.now() >= activeSessionExpiryTs) {
src/pages_assets/auth.js:1338:    const expiry = Number.isFinite(Number(expiryTs)) && Number(expiryTs) > Date.now()
src/pages_assets/auth.js:1340:        : Date.now() + SESSION_CONFIG.DEFAULT_DURATION_MS;
src/pages_assets/auth.js:1381:        if (Date.now() > expiry) {
```

## P34 detailed internal errors exposed

```
src/analytics/validate.rs:415:                details: format!("Track A invariant query failed: {err}"),
src/analytics/validate.rs:459:                details: format!("Track A invariant query failed: {err}"),
src/analytics/validate.rs:662:                details: format!("Track B invariant query failed: {err}"),
src/analytics/validate.rs:699:                details: format!("Track B invariant query failed: {err}"),
src/analytics/validate.rs:812:                details: format!("Cross-track drift query failed while reading Track A: {err}"),
src/analytics/validate.rs:849:                details: format!("Cross-track drift query failed while reading Track B: {err}"),
src/analytics/validate.rs:982:                    details: format!("usage_daily negative-counter query failed: {err}"),
src/analytics/validate.rs:1023:                    details: format!("usage_daily coverage query failed: {err}"),
src/analytics/validate.rs:1073:                    details: format!("token_daily_stats negative-counter query failed: {err}"),
src/analytics/validate.rs:1133:            details: format!("Timeseries rollup query: {row_count} day buckets in {elapsed_ms}ms"),
src/analytics/validate.rs:1141:            details: format!("Timeseries rollup query failed after {elapsed_ms}ms: {err}"),
src/analytics/validate.rs:1177:            details: format!("Breakdown query: {row_count} agent groups in {elapsed_ms}ms"),
src/analytics/validate.rs:1185:            details: format!("Breakdown query failed after {elapsed_ms}ms: {err}"),
src/lib.rs:10255:            format!(" WHERE {normalized_source_sql} = ?"),
src/lib.rs:10372:        .map_err(|e| CliError::unknown(format!("query: {e}")))?;
src/lib.rs:10382:        .map_err(|e| CliError::unknown(format!("query: {e}")))?;
src/lib.rs:10420:        .map_err(|e| CliError::unknown(format!("query: {e}")))?
src/lib.rs:16057:        .map_err(|e| CliError::unknown(format!("query: {e}")))?
src/lib.rs:16093:        .map_err(|e| CliError::unknown(format!("query: {e}")))?
src/lib.rs:16125:        .map_err(|e| CliError::unknown(format!("query: {e}")))?
src/lib.rs:22547:        assert_eq!(where_sql, format!(" WHERE {normalized_source_sql} = ?"));
src/lib.rs:22572:        let sql = format!("SELECT COUNT(*) FROM conversations c{where_sql}");
src/lib.rs:22581:        let sql = format!("SELECT COUNT(*) FROM conversations c{where_sql}");
src/lib.rs:22599:        let sql = format!("SELECT COUNT(*) FROM conversations c{where_sql}");
src/lib.rs:22608:        let sql = format!("SELECT COUNT(*) FROM conversations c{where_sql}");
src/lib.rs:25509:            sql.push_str(&format!("?{}", params.len() + 1));
src/analytics/query.rs:651:        &format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
src/analytics/query.rs:680:            format!("SELECT COUNT(*) FROM {from_sql} WHERE {extra}")
src/analytics/query.rs:682:        Some(extra) => format!("SELECT COUNT(*) FROM {from_sql}{where_sql} AND {extra}"),
src/analytics/query.rs:683:        None => format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
src/analytics/query.rs:713:            format!("SELECT COUNT(*) FROM {from_sql} WHERE {extra}")
src/analytics/query.rs:715:        Some(extra) => format!("SELECT COUNT(*) FROM {from_sql}{where_sql} AND {extra}"),
src/analytics/query.rs:716:        None => format!("SELECT COUNT(*) FROM {from_sql}{where_sql}"),
src/analytics/query.rs:1344:        .map_err(|e| AnalyticsError::Db(format!("Analytics query failed: {e}")))?;
src/analytics/query.rs:1435:        .map_err(|e| AnalyticsError::Db(format!("Analytics query failed: {e}")))?
src/analytics/query.rs:1595:        .map_err(|e| AnalyticsError::Db(format!("Analytics query failed: {e}")))?;
src/analytics/query.rs:1824:        .map_err(|e| AnalyticsError::Db(format!("Cost timeseries query failed: {e}")))?;
src/analytics/query.rs:1990:        .map_err(|e| AnalyticsError::Db(format!("Cost timeseries query failed: {e}")))?;
src/analytics/query.rs:2210:        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;
src/analytics/query.rs:2283:        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?
src/analytics/query.rs:2439:        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;
src/analytics/query.rs:2810:        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;
src/analytics/query.rs:2887:        .map_err(|e| AnalyticsError::Db(format!("Breakdown query failed: {e}")))?;
src/analytics/query.rs:2950:        .map_err(|e| AnalyticsError::Db(format!("Tool report query failed: {e}")))?
src/analytics/query.rs:3040:        .map_err(|e| AnalyticsError::Db(format!("Tool report query failed: {e}")))?;
src/analytics/query.rs:3231:        .map_err(|e| AnalyticsError::Db(format!("Tool report query failed: {e}")))?;
src/analytics/query.rs:3342:            format!(" LEFT JOIN {message_metrics_sql} ON mm.message_id = m.id")
src/analytics/query.rs:3510:        .map_err(|e| AnalyticsError::Db(format!("Session scatter query failed: {e}")))?;
src/export.rs:134:        output.push_str(&format!("**Query:** `{}`\n\n", query.replace('`', "")));
src/export.rs:279:        output.push_str(&format!("Query: {query}\n"));
src/daemon/core.rs:734:                            message: format!("failed to query jobs: {e}"),
src/pages_assets/database.js:71:        throw new Error('SQLite library not available. Ensure sqlite3.js is in the vendor folder.');
src/indexer/mod.rs:10289:        .with_context(|| format!("opening frankensqlite db readonly at {}", db_path.display()))?;
src/search/query.rs:1037:        let wildcard_query = format!("*{}*", query.trim_matches('*'));
src/search/query.rs:1040:            message: format!("Try broader search: \"{wildcard_query}\""),
src/search/query.rs:5790:                sql.push_str(&format!(" AND {normalized_source_sql} = ?"));
src/search/query.rs:6125:                sql.push_str(&format!(" AND {normalized_source_sql} = ?"));
src/search/query.rs:8189:        let queries: Vec<String> = (0..100).map(|i| format!("query_{}", i)).collect();
src/search/query.rs:17463:            .map(|i| format!("concurrent_query_{} test search", i))
src/pages/errors.rs:181:            Self::InvalidQuery(detail) => format!("Invalid query: {}", detail),
```

## P35 suspicious ambiguous imports

```
src/sources/index.rs:617:    use std::path::PathBuf;
src/sources/sync.rs:27:use std::path::{Path, PathBuf};
src/sources/sync.rs:1789:    use std::path::{Component, Path};
src/sources/config.rs:40:use std::path::{Component, Path, PathBuf};
src/sources/setup.rs:18:use std::path::PathBuf;
src/model/packet_audit.rs:298:    use std::path::PathBuf;
src/model/conversation_packet.rs:12:use std::path::Path;
src/model/conversation_packet.rs:526:    use std::path::PathBuf;
src/lib.rs:34:use std::path::{Path, PathBuf};
src/lib.rs:6635:    use std::path::PathBuf;
src/lib.rs:22439:    use std::path::Path;
src/lib.rs:22440:    use std::path::PathBuf;
src/lib.rs:22815:    use std::path::PathBuf;
src/model/types.rs:4:use std::path::PathBuf;
src/update_check.rs:12:use std::path::PathBuf;
src/daemon/core.rs:11:use std::path::{Path, PathBuf};
src/daemon/core.rs:799:    use std::path::PathBuf;
src/daemon/worker.rs:7:use std::path::Path;
src/daemon/client.rs:9:use std::path::PathBuf;
src/bookmarks.rs:11:use std::path::{Path, PathBuf};
src/indexer/lexical_generation.rs:41:use std::path::{Path, PathBuf};
src/daemon/models.rs:6:use std::path::{Path, PathBuf};
src/html_export/filename.rs:14:use std::path::{Path, PathBuf};
src/tui_asciicast.rs:7:use std::path::Path;
src/ftui_harness.rs:6:use std::fmt::Write as _;
src/ftui_harness.rs:7:use std::path::{Path, PathBuf};
src/daemon/mod.rs:51:use std::path::{Path, PathBuf};
src/indexer/semantic.rs:4:use std::path::{Path, PathBuf};
src/indexer/semantic.rs:1635:    use std::path::Path;
src/pages/key_management.rs:36:use std::path::Path;
src/bin/cass-pages-perf-bundle.rs:12:use std::path::{Path, PathBuf};
src/pages/encrypt.rs:25:use std::path::Path;
src/ui/data.rs:1360:    use std::path::PathBuf;
src/pages/analytics.rs:33:use std::path::Path;
src/pages/wizard.rs:6:use std::path::PathBuf;
src/pages/mod.rs:9:use std::path::{Path, PathBuf};
src/ui/trace.rs:17:use std::path::{Path, PathBuf};
src/pages/config_input.rs:42:use std::path::PathBuf;
src/pages/preview.rs:11:use std::path::PathBuf;
src/search/model_download.rs:23:use std::path::{Path, PathBuf};
src/pages/verify.rs:15:use std::path::{Path, PathBuf};
src/pages/redact.rs:3:use std::path::PathBuf;
src/pages/export.rs:9:use std::path::{Path, PathBuf};
src/pages/export.rs:813:    use std::path::Path;
src/pages/qr.rs:29:use std::path::Path;
src/ui/components/export_modal.rs:9:use std::path::PathBuf;
src/ui/components/export_modal.rs:395:    use std::path::PathBuf;
src/pages/bundle.rs:14:use std::path::{Path, PathBuf};
src/pages/attachments.rs:28:use std::path::Path;
src/pages/deploy_github.rs:8:use std::path::{Path, PathBuf};
src/ui/style_system.rs:16:use std::path::{Path, PathBuf};
src/indexer/mod.rs:22:use std::path::{Path, PathBuf};
src/pages/secret_scan.rs:11:use std::path::{Path, PathBuf};
src/pages/profiles.rs:16:use std::path::PathBuf;
src/pages/profiles.rs:536:        use std::str::FromStr;
src/storage/sqlite.rs:34:use std::path::{Path, PathBuf};
src/storage/sqlite.rs:11201:        use std::path::PathBuf;
src/storage/sqlite.rs:11986:        use std::path::PathBuf;
src/storage/sqlite.rs:12465:        use std::path::PathBuf;
src/storage/sqlite.rs:12709:        use std::path::PathBuf;
src/storage/sqlite.rs:13160:        use std::path::PathBuf;
src/storage/sqlite.rs:13352:        use std::path::PathBuf;
src/storage/sqlite.rs:13420:        use std::path::PathBuf;
src/storage/sqlite.rs:13526:        use std::path::PathBuf;
src/storage/sqlite.rs:13792:        use std::path::PathBuf;
src/storage/sqlite.rs:13897:        use std::path::PathBuf;
src/storage/sqlite.rs:13985:        use std::path::PathBuf;
src/storage/sqlite.rs:14090:        use std::path::PathBuf;
src/storage/sqlite.rs:14192:        use std::path::PathBuf;
src/storage/sqlite.rs:14283:        use std::path::PathBuf;
src/storage/sqlite.rs:14350:        use std::path::PathBuf;
src/storage/sqlite.rs:14472:        use std::path::PathBuf;
src/storage/sqlite.rs:14602:        use std::path::PathBuf;
src/storage/sqlite.rs:14673:        use std::path::PathBuf;
src/storage/sqlite.rs:14750:        use std::path::PathBuf;
src/storage/sqlite.rs:14850:        use std::path::PathBuf;
src/storage/sqlite.rs:14988:        use std::path::PathBuf;
src/storage/sqlite.rs:15089:        use std::path::PathBuf;
src/storage/sqlite.rs:15197:        use std::path::PathBuf;
src/storage/sqlite.rs:15306:        use std::path::PathBuf;
```

## P36 infra/config surfaces that should not ride with refactor commits

```
./tests/performance/package.json
./tests/package.json
./.github/workflows/acfs-checksums-dispatch.yml
./.github/workflows/fuzz.yml
./.github/workflows/install-test.yml
./.github/workflows/lighthouse.yml
./.github/workflows/perf.yml
./.github/workflows/notify-acfs.yml
./.github/workflows/browser-tests.yml
./.github/workflows/ci.yml
./.github/workflows/coverage.yml
./.github/workflows/bench.yml
./.github/workflows/fresh-clone-build.yml
./.github/workflows/release.yml
./fuzz/Cargo.lock
./fuzz/Cargo.toml
./Cargo.lock
./Cargo.toml
```

## P37 unpinned dependency snippets

```
Cargo.toml:13:anyhow = "*"
Cargo.toml:14:thiserror = "*"
Cargo.toml:15:tracing = "*"
Cargo.toml:16:tracing-subscriber = { version = "*", features = ["env-filter", "fmt", "ansi"] }
Cargo.toml:18:clap = { version = "*", features = ["derive", "cargo", "env", "unicode", "wrap_help"] }
Cargo.toml:19:clap_complete = "*"
Cargo.toml:20:clap_mangen = "*"
Cargo.toml:21:indicatif = "*"
Cargo.toml:22:console = "*"
Cargo.toml:23:colored = "*"
Cargo.toml:24:serde = { version = "*", features = ["derive"] }
Cargo.toml:25:serde_json = "*"
Cargo.toml:26:toon = { version = "*", git = "https://github.com/Dicklesworthstone/toon_rust", rev = "5669b72a", package = "tru" }
Cargo.toml:27:tempfile = "*"
Cargo.toml:28:rmp-serde = "*"  # MessagePack for binary metadata serialization (Opt 3.1)
Cargo.toml:29:toml = "*"
Cargo.toml:30:directories = "*"
Cargo.toml:31:which = "*"
Cargo.toml:32:shell-words = "*"
Cargo.toml:33:dotenvy = "*"
Cargo.toml:34:notify = "*"
Cargo.toml:35:frankensqlite = { version = "*", git = "https://github.com/Dicklesworthstone/frankensqlite", rev = "18a512b6", package = "fsqlite", features = ["fts5"] }
Cargo.toml:36:rayon = "*"
Cargo.toml:37:crossbeam-channel = "*"
Cargo.toml:38:parking_lot = "*"
Cargo.toml:39:fs2 = "*"
Cargo.toml:41:rustc-hash = "*"  # Fast non-cryptographic hashing for cache keys (P1 Opt 1.3), replaces unmaintained fxhash
Cargo.toml:42:xxhash-rust = { version = "*", features = ["xxh3"] }  # Fast content hashing for search-hit dedup (bead sdoxg)
Cargo.toml:43:itoa = "*"  # Zero-allocation integer-to-string for hot paths (bead w32k6)
Cargo.toml:44:smallvec = "*"  # Stack-allocated small vectors for hot paths (Opt 4.4)
Cargo.toml:45:regex = "*"
Cargo.toml:46:portable-pty = "*"
Cargo.toml:49:ftui = { version = "*", git = "https://github.com/Dicklesworthstone/frankentui", rev = "5f78cfa0" }
Cargo.toml:50:ftui-runtime = { version = "*", git = "https://github.com/Dicklesworthstone/frankentui", rev = "5f78cfa0", features = ["native-backend", "crossterm-compat"] }
Cargo.toml:51:ftui-tty = { version = "*", git = "https://github.com/Dicklesworthstone/frankentui", rev = "5f78cfa0" }
Cargo.toml:52:ftui-extras = { version = "*", git = "https://github.com/Dicklesworthstone/frankentui", rev = "5f78cfa0", default-features = false, features = ["markdown", "syntax", "charts", "canvas", "theme", "clipboard", "clipboard-fallback", "export", "visual-fx", "forms", "validation", "help"] }
Cargo.toml:53:dirs = "*"
Cargo.toml:54:walkdir = "*"
Cargo.toml:55:glob = "*"
Cargo.toml:57:blake3 = "*"
Cargo.toml:58:mime_guess = "*"
Cargo.toml:59:pulldown-cmark = { version = "*", default-features = false, features = ["html"] }
Cargo.toml:60:chrono = { version = "*", features = ["serde"] }
Cargo.toml:61:semver = "*"
Cargo.toml:62:tracing-appender = "*"
Cargo.toml:63:strsim = "*"
Cargo.toml:64:once_cell = "*"
Cargo.toml:65:syntect = "*"
Cargo.toml:66:itertools = "*"
Cargo.toml:67:crc32fast = "*"
Cargo.toml:68:unicode-normalization = "*"
Cargo.toml:69:urlencoding = "*"
Cargo.toml:70:half = { version = "*", features = ["bytemuck"] }
Cargo.toml:71:memmap2 = "*"
Cargo.toml:72:bytemuck = "*"
Cargo.toml:73:fastembed = { version = "*", default-features = false, features = ["ort-download-binaries-rustls-tls"] }
Cargo.toml:74:frankensearch = { version = "*", git = "https://github.com/Dicklesworthstone/frankensearch", rev = "3dbab624", default-features = false, features = ["hash", "lexical", "ann", "fastembed-reranker"] }
Cargo.toml:75:franken-agent-detection = { version = "*", git = "https://github.com/Dicklesworthstone/franken_agent_detection", rev = "9ead6659b98c087c4edcef405f2b01d789c22764", features = ["connectors", "cursor", "chatgpt", "opencode", "crush"] }
Cargo.toml:76:wide = "*"  # Portable SIMD for P0 Opt 2: SIMD dot product
Cargo.toml:77:arrayvec = "*"  # Stack-based arrays for P1 Opt 1.4: Edge N-gram optimization
Cargo.toml:78:bloomfilter = "*"  # Probabilistic membership testing for P2 Opt 3.3: Workspace Cache
Cargo.toml:79:hnsw_rs = "*"  # Opt 9: Approximate Nearest Neighbor with HNSW
Cargo.toml:80:ouroboros = "*"  # Safe self-referential wrapper for HNSW loader lifetime
Cargo.toml:89:ring = "*"
Cargo.toml:90:url = "*"
Cargo.toml:91:pbkdf2 = "*"  # PBKDF2 key derivation for Web Crypto compatible HTML export encryption
Cargo.toml:95:hex = "*"
Cargo.toml:98:dialoguer = "*"
Cargo.toml:101:ssh2 = "*"
Cargo.toml:103:openssl = { version = "*", features = ["vendored"] }
Cargo.toml:104:argon2 = "*"
Cargo.toml:106:zeroize = { version = "*", features = ["derive"] }
Cargo.toml:107:flate2 = "*"
Cargo.toml:111:qrcode = { version = "*", optional = true }
Cargo.toml:112:image = { version = "*", optional = true, default-features = false, features = ["png"] }
Cargo.toml:117:security-framework = "*"
Cargo.toml:129:vergen = { version = "*", default-features = false, features = ["build", "cargo"] }
Cargo.toml:130:toml = "*"
Cargo.toml:133:assert_cmd = "*"
Cargo.toml:134:predicates = "*"
```

## P38 wildcard/glob imports

```
src/analytics/query.rs:13:use super::types::*;
src/lib.rs:24:use base64::prelude::*;
src/pages/key_management.rs:29:use base64::prelude::*;
src/pages/encrypt.rs:19:use base64::prelude::*;
src/pages/qr.rs:26:use base64::prelude::*;
src/pages/bundle.rs:7:use base64::prelude::*;
src/pages/deploy_cloudflare.rs:7:use base64::prelude::*;
src/pages/verify.rs:8:use base64::prelude::*;
src/indexer/semantic.rs:16:use rayon::prelude::*;
```

## P39 async functions returning Promise (audit for real await)

_none found_

## P40 await/then in nearby non-async contexts (manual audit)

_none found_

---

## Next steps

1. Review each section; confirm which hits are real vs. false positives.
2. File beads for accepted patterns (one per pathology class).
3. Proceed to `./scripts/dup_scan.sh` for structural duplication.
4. Score candidates via `./scripts/score_candidates.py`.
5. For each accepted candidate: fill isomorphism card, edit, verify, ledger.

Full P1-P40 pathology catalog: `references/VIBE-CODED-PATHOLOGIES.md`.
Attack order (cheap wins first): the "AI-slop refactor playbook" in that file.
