//! Documentation Generation for pages export.
//!
//! Automatically generates comprehensive, deployment-specific documentation
//! that is included with each published site.
//!
//! # Overview
//!
//! Generated documentation includes:
//! - **README.md**: Main archive description for repository root
//! - **SECURITY.md**: Detailed security model and threat analysis
//! - **help.html**: In-app help accessible from web viewer
//! - **recovery.html**: Password recovery instructions
//! - **about.txt**: Simple text explanation for non-technical users

use crate::pages::summary::{KeySlotType, PrePublishSummary};
use chrono::{DateTime, Utc};

const CASS_VERSION: &str = env!("CARGO_PKG_VERSION");
const DOC_DATE_FORMAT: &str = "%Y-%m-%d";

fn format_optional_doc_date(value: Option<DateTime<Utc>>, fallback: &str) -> String {
    value
        .map(|dt| dt.format(DOC_DATE_FORMAT).to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// Location where a generated document should be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocLocation {
    /// Repository root (README.md, SECURITY.md)
    RepoRoot,
    /// Web root alongside index.html (help.html, about.txt)
    WebRoot,
}

/// A generated documentation file.
#[derive(Debug, Clone)]
pub struct GeneratedDoc {
    /// Filename for the document.
    pub filename: String,
    /// Content of the document.
    pub content: String,
    /// Where to place the document.
    pub location: DocLocation,
}

/// Configuration for documentation generation.
#[derive(Debug, Clone, Default)]
pub struct DocConfig {
    /// Target URL where the archive will be hosted.
    pub target_url: Option<String>,
    /// Repository URL for CASS source.
    pub cass_repo_url: String,
    /// Argon2 memory parameter in KB.
    pub argon_memory_kb: u32,
    /// Argon2 time iterations.
    pub argon_iterations: u32,
    /// Argon2 parallelism.
    pub argon_parallelism: u32,
}

impl DocConfig {
    /// Create a new DocConfig with default CASS repo URL.
    pub fn new() -> Self {
        Self {
            target_url: None,
            cass_repo_url: "https://github.com/Dicklesworthstone/coding_agent_session_search"
                .to_string(),
            argon_memory_kb: 65536,
            argon_iterations: 3,
            argon_parallelism: 4,
        }
    }

    /// Set the target URL.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.target_url = Some(url.into());
        self
    }

    /// Set Argon2 parameters.
    pub fn with_argon_params(mut self, memory_kb: u32, iterations: u32, parallelism: u32) -> Self {
        self.argon_memory_kb = memory_kb;
        self.argon_iterations = iterations;
        self.argon_parallelism = parallelism;
        self
    }
}

/// Generator for export documentation.
pub struct DocumentationGenerator {
    config: DocConfig,
    summary: PrePublishSummary,
}

impl DocumentationGenerator {
    /// Create a new documentation generator.
    pub fn new(config: DocConfig, summary: PrePublishSummary) -> Self {
        Self { config, summary }
    }

    /// Generate all documentation files.
    pub fn generate_all(&self) -> Vec<GeneratedDoc> {
        vec![
            self.generate_readme(),
            self.generate_security_doc(),
            self.generate_help_html(),
            self.generate_recovery_html(),
            self.generate_about_txt(),
        ]
    }

    /// Generate README.md for repository root.
    pub fn generate_readme(&self) -> GeneratedDoc {
        let agent_list = self
            .summary
            .agents
            .iter()
            .map(|a| format!("- {} ({} conversations)", a.name, a.conversation_count))
            .collect::<Vec<_>>()
            .join("\n");

        let url_display = self
            .config
            .target_url
            .as_deref()
            .unwrap_or("[deployment URL]");

        let start_date = format_optional_doc_date(self.summary.earliest_timestamp, "Unknown");
        let end_date = format_optional_doc_date(self.summary.latest_timestamp, "Unknown");

        let argon_params = format!(
            "m={}KB, t={}, p={}",
            self.config.argon_memory_kb,
            self.config.argon_iterations,
            self.config.argon_parallelism
        );

        let slot_count = self.summary.key_slots.len();
        let date = Utc::now().format("%Y-%m-%d");

        let content = README_TEMPLATE
            .replace("{url}", url_display)
            .replace(
                "{conversation_count}",
                &self.summary.total_conversations.to_string(),
            )
            .replace("{agent_list}", &agent_list)
            .replace("{start_date}", &start_date)
            .replace("{end_date}", &end_date)
            .replace("{argon_params}", &argon_params)
            .replace("{slot_count}", &slot_count.to_string())
            .replace("{version}", CASS_VERSION)
            .replace("{date}", &date.to_string());

        GeneratedDoc {
            filename: "README.md".to_string(),
            content,
            location: DocLocation::RepoRoot,
        }
    }

    /// Generate SECURITY.md with threat model documentation.
    pub fn generate_security_doc(&self) -> GeneratedDoc {
        let slot_descriptions = self
            .summary
            .key_slots
            .iter()
            .map(|slot| {
                let slot_type_label = match slot.slot_type {
                    KeySlotType::Password => "Password-derived",
                    KeySlotType::QrCode => "QR code (direct key)",
                    KeySlotType::Recovery => "Recovery phrase",
                };
                let created_str = format_optional_doc_date(slot.created_at, "N/A");
                format!(
                    "- Slot {}: {} (created {})",
                    slot.slot_index + 1,
                    slot_type_label,
                    created_str
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let slot_descriptions = if slot_descriptions.is_empty() {
            "No key slots configured yet.".to_string()
        } else {
            slot_descriptions
        };

        let argon_memory = self.config.argon_memory_kb.to_string();
        let argon_iterations = self.config.argon_iterations.to_string();
        let argon_parallelism = self.config.argon_parallelism.to_string();
        let slot_count = self.summary.key_slots.len().to_string();

        let content = SECURITY_TEMPLATE
            .replace("{memory}", &argon_memory)
            .replace("{iterations}", &argon_iterations)
            .replace("{parallelism}", &argon_parallelism)
            .replace("{slot_count}", &slot_count)
            .replace("{slot_descriptions}", &slot_descriptions)
            .replace("{repo_url}", &self.config.cass_repo_url)
            .replace("{version}", CASS_VERSION);

        GeneratedDoc {
            filename: "SECURITY.md".to_string(),
            content,
            location: DocLocation::RepoRoot,
        }
    }

    /// Generate help.html for in-app help.
    pub fn generate_help_html(&self) -> GeneratedDoc {
        GeneratedDoc {
            filename: "help.html".to_string(),
            content: HELP_HTML_TEMPLATE.to_string(),
            location: DocLocation::WebRoot,
        }
    }

    /// Generate recovery.html with password recovery instructions.
    pub fn generate_recovery_html(&self) -> GeneratedDoc {
        let has_recovery_slot = self
            .summary
            .key_slots
            .iter()
            .any(|s| s.slot_type == KeySlotType::Recovery);

        let recovery_section = if has_recovery_slot {
            RECOVERY_WITH_KEY_SECTION
        } else {
            RECOVERY_NO_KEY_SECTION
        };

        let content = RECOVERY_HTML_TEMPLATE.replace("{recovery_section}", recovery_section);

        GeneratedDoc {
            filename: "recovery.html".to_string(),
            content,
            location: DocLocation::WebRoot,
        }
    }

    /// Generate about.txt for non-technical users.
    pub fn generate_about_txt(&self) -> GeneratedDoc {
        let url_display = self
            .config
            .target_url
            .as_deref()
            .unwrap_or("[deployment URL]");

        let conversation_count = self.summary.total_conversations.to_string();
        let date = Utc::now().format("%Y-%m-%d");

        let content = ABOUT_TXT_TEMPLATE
            .replace("{url}", url_display)
            .replace("{conversation_count}", &conversation_count)
            .replace("{date}", &date.to_string())
            .replace("{version}", CASS_VERSION);

        GeneratedDoc {
            filename: "about.txt".to_string(),
            content,
            location: DocLocation::WebRoot,
        }
    }
}

// =============================================================================
// Template Constants
// =============================================================================

const README_TEMPLATE: &str = r#"# Encrypted Coding Session Archive

This repository contains an encrypted archive of coding session histories,
created with [CASS](https://github.com/Dicklesworthstone/coding_agent_session_search).

## Quick Access

Open the web viewer: [{url}]({url})

## What This Contains

This archive includes {conversation_count} conversations from the following sources:
{agent_list}

Date range: {start_date} to {end_date}

## Accessing the Archive

### Option 1: Password
Enter the password at the web viewer to decrypt and browse the archive.

### Option 2: QR Code (if configured)
Scan the QR code with your phone camera to auto-fill the decryption key.

## Security

This archive is protected with:
- **Encryption**: AES-256-GCM (authenticated encryption)
- **Key Derivation**: Argon2id with {argon_params}
- **Key Slots**: {slot_count} independent decryption key(s)

The encrypted archive can be safely hosted publicly. Only someone with a valid
password or QR code can decrypt the contents.

For detailed security information, see [SECURITY.md](SECURITY.md).

## Recovery

If you forget your password:
- Use the recovery key (if you saved one during setup)
- The archive owner may have additional key slots

Without a valid key, the archive cannot be decrypted.

---
Generated by CASS v{version} on {date}
"#;

const SECURITY_TEMPLATE: &str = r#"# Security Model

## Overview

This document describes the security properties of this encrypted archive.

## Threat Model

### What This Protects Against

- **Casual access**: Random visitors cannot read content
- **Server compromise**: GitHub/hosting provider cannot read your data
- **Network interception**: Content is encrypted before transmission
- **Brute force (with strong password)**: Argon2id makes guessing expensive

### What This Does NOT Protect Against

- **Weak passwords**: Short or common passwords can be cracked
- **Password sharing**: Anyone with the password can decrypt
- **Endpoint compromise**: Malware on your device can capture passwords
- **Targeted attacks**: Determined attackers with resources may succeed
- **Quantum computers**: AES-256 may be weakened by future advances

## Encryption Details

### Envelope Encryption

The archive uses envelope encryption:
1. A random 256-bit Data Encryption Key (DEK) encrypts the data
2. The DEK is encrypted with a Key Encryption Key (KEK) derived from your password
3. Multiple key slots allow different passwords to decrypt the same data

### Algorithms

| Component | Algorithm | Parameters |
|-----------|-----------|------------|
| Data Encryption | AES-256-GCM | 96-bit nonce, 128-bit tag |
| Key Derivation | Argon2id | m={memory}KB, t={iterations}, p={parallelism} |
| DEK Encryption | AES-256-GCM | Same as data |
| Nonce Generation | Counter-based | Prevents reuse |

### Key Slots

This archive has {slot_count} key slot(s):
{slot_descriptions}

Each slot contains the same DEK encrypted with a different KEK.

## Verification

### Checking Archive Integrity

The AES-GCM authentication tag ensures:
- Data has not been modified
- Decryption used the correct key

If decryption fails, the archive was either:
- Corrupted in transit
- Modified by an attacker
- Decrypted with wrong key

### Verifying Implementation

This archive was created with CASS, an open-source tool. You can:
1. Review the source code at {repo_url}
2. Verify the implementation uses standard libraries
3. Audit the cryptographic construction

## Recommendations

1. **Use a strong password**: 16+ characters, or 5+ random words
2. **Store recovery key safely**: It is the only backup
3. **Rotate passwords periodically**: Generate new archive with new key
4. **Limit distribution**: Share URL only with intended recipients

## Contact

For security issues with CASS, see {repo_url}/security

---
Generated by CASS v{version}
"#;

const HELP_HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Help - CASS Archive</title>
    <style>
        :root {
            --bg-primary: #1a1a2e;
            --bg-secondary: #16213e;
            --text-primary: #eee;
            --text-secondary: #aaa;
            --accent: #e94560;
            --border: #333;
        }
        * { box-sizing: border-box; }
        body {
            font-family: system-ui, -apple-system, sans-serif;
            max-width: 800px;
            margin: 0 auto;
            padding: 20px;
            background: var(--bg-primary);
            color: var(--text-primary);
            line-height: 1.6;
        }
        h1, h2, h3 {
            color: var(--text-primary);
            border-bottom: 1px solid var(--border);
            padding-bottom: 0.5em;
        }
        h1 { font-size: 1.8rem; }
        h2 { font-size: 1.4rem; margin-top: 2em; }
        h3 { font-size: 1.1rem; margin-top: 1.5em; border: none; }
        code {
            background: var(--bg-secondary);
            padding: 2px 6px;
            border-radius: 3px;
            font-family: 'SF Mono', Monaco, monospace;
            font-size: 0.9em;
        }
        ul { padding-left: 1.5em; }
        li { margin: 0.5em 0; }
        .warning {
            background: #3d2f00;
            padding: 15px;
            border-left: 4px solid #ffc107;
            border-radius: 4px;
            margin: 1em 0;
        }
        .info {
            background: #0d3a5c;
            padding: 15px;
            border-left: 4px solid #17a2b8;
            border-radius: 4px;
            margin: 1em 0;
        }
        a { color: var(--accent); }
        .back-link {
            display: inline-block;
            margin-top: 2em;
            padding: 10px 20px;
            background: var(--accent);
            color: white;
            text-decoration: none;
            border-radius: 4px;
        }
        .back-link:hover { opacity: 0.9; }
    </style>
</head>
<body>
    <h1>Help</h1>

    <h2>Accessing the Archive</h2>
    <p>Enter your password in the unlock screen. The password was set by whoever created this archive.</p>

    <h3>Password Tips</h3>
    <ul>
        <li>Passwords are case-sensitive</li>
        <li>Check for leading/trailing spaces</li>
        <li>If using a passphrase, ensure correct word separators</li>
    </ul>

    <h3>QR Code Access</h3>
    <p>If a QR code was provided, scanning it will auto-fill the decryption key.</p>

    <h2>Searching</h2>
    <p>Use the search box to find conversations:</p>
    <ul>
        <li><code>keyword</code> - Simple text search</li>
        <li><code>"exact phrase"</code> - Match exact phrase</li>
        <li><code>agent:claude_code</code> - Filter by agent</li>
        <li><code>workspace:/projects/myapp</code> - Filter by workspace</li>
    </ul>

    <h2>Keyboard Shortcuts</h2>
    <ul>
        <li><code>/</code> - Focus search box</li>
        <li><code>Esc</code> - Clear search / close dialogs</li>
        <li><code>j</code> / <code>k</code> - Navigate conversation list</li>
        <li><code>Enter</code> - Open selected conversation</li>
    </ul>

    <h2>Troubleshooting</h2>

    <h3>Decryption Failed</h3>
    <div class="warning">
        <p>This usually means the password is incorrect. Double-check:</p>
        <ul>
            <li>Correct password (case-sensitive)</li>
            <li>No extra spaces</li>
            <li>Correct keyboard layout</li>
        </ul>
    </div>

    <h3>Slow Loading</h3>
    <p>Large archives may take time to decrypt. This happens locally in your browser - no data is sent to any server.</p>

    <h3>Browser Compatibility</h3>
    <p>Requires a modern browser with WebCrypto support:</p>
    <ul>
        <li>Chrome 60+</li>
        <li>Firefox 57+</li>
        <li>Safari 11+</li>
        <li>Edge 79+</li>
    </ul>

    <h2>Privacy</h2>
    <div class="info">
        <p>All decryption happens in your browser. Your password is never sent to any server. The encrypted data is fetched and decrypted entirely client-side.</p>
    </div>

    <h2>More Information</h2>
    <p>For technical details about the encryption, see <a href="./SECURITY.md">SECURITY.md</a>.</p>

    <a href="./" class="back-link">Back to Archive</a>
</body>
</html>
"#;

const RECOVERY_HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Password Recovery - CASS Archive</title>
    <style>
        :root {
            --bg-primary: #1a1a2e;
            --bg-secondary: #16213e;
            --text-primary: #eee;
            --text-secondary: #aaa;
            --accent: #e94560;
            --border: #333;
            --success: #28a745;
            --danger: #dc3545;
        }
        * { box-sizing: border-box; }
        body {
            font-family: system-ui, -apple-system, sans-serif;
            max-width: 800px;
            margin: 0 auto;
            padding: 20px;
            background: var(--bg-primary);
            color: var(--text-primary);
            line-height: 1.6;
        }
        h1, h2 {
            color: var(--text-primary);
            border-bottom: 1px solid var(--border);
            padding-bottom: 0.5em;
        }
        h1 { font-size: 1.8rem; }
        h2 { font-size: 1.4rem; margin-top: 2em; }
        .warning {
            background: #3d2f00;
            padding: 15px;
            border-left: 4px solid #ffc107;
            border-radius: 4px;
            margin: 1em 0;
        }
        .danger {
            background: #3d1f1f;
            padding: 15px;
            border-left: 4px solid var(--danger);
            border-radius: 4px;
            margin: 1em 0;
        }
        .success {
            background: #1f3d2f;
            padding: 15px;
            border-left: 4px solid var(--success);
            border-radius: 4px;
            margin: 1em 0;
        }
        ol { padding-left: 1.5em; }
        li { margin: 0.5em 0; }
        a { color: var(--accent); }
        .back-link {
            display: inline-block;
            margin-top: 2em;
            padding: 10px 20px;
            background: var(--accent);
            color: white;
            text-decoration: none;
            border-radius: 4px;
        }
        .back-link:hover { opacity: 0.9; }
    </style>
</head>
<body>
    <h1>Password Recovery</h1>

    <p>If you've forgotten your password, here are your options for recovering access to this encrypted archive.</p>

{recovery_section}

    <h2>Prevention for the Future</h2>
    <ol>
        <li>Use a password manager to store complex passwords</li>
        <li>Write down and securely store your recovery key</li>
        <li>Consider using a memorable passphrase (5+ random words)</li>
        <li>Share access with a trusted person who can help recover</li>
    </ol>

    <h2>Technical Reality</h2>
    <div class="danger">
        <p><strong>Important:</strong> The encryption used (AES-256-GCM with Argon2id) is designed to be unbreakable without the correct password. There is no backdoor, no master key, and no way to recover data without a valid key.</p>
        <p>This is a feature, not a bug - it ensures your data remains private even if the hosting service is compromised.</p>
    </div>

    <a href="./" class="back-link">Back to Archive</a>
</body>
</html>
"#;

const RECOVERY_WITH_KEY_SECTION: &str = r#"    <h2>Using Your Recovery Key</h2>
    <div class="success">
        <p>Good news! This archive was configured with a recovery key. If you saved your recovery key during setup, you can use it to access the archive.</p>
    </div>
    <ol>
        <li>Find your saved recovery key (a series of words or characters)</li>
        <li>Go to the main archive page</li>
        <li>Click "Use Recovery Key" or similar option</li>
        <li>Enter the recovery key exactly as saved</li>
        <li>The archive will decrypt using the recovery key</li>
    </ol>

    <h2>If You Don't Have the Recovery Key</h2>
    <div class="warning">
        <p>Without either the password or recovery key, there is no way to decrypt this archive. The encryption is designed to be unbreakable.</p>
    </div>
"#;

const RECOVERY_NO_KEY_SECTION: &str = r#"    <h2>Recovery Options</h2>
    <div class="warning">
        <p>This archive was not configured with a recovery key. Your options are limited.</p>
    </div>

    <h3>Try These Steps</h3>
    <ol>
        <li>Check your password manager for saved credentials</li>
        <li>Try common password variations you might have used</li>
        <li>Contact the person who created this archive - they may have additional key slots</li>
        <li>Check if you have the original data to re-export with a new password</li>
    </ol>
"#;

const ABOUT_TXT_TEMPLATE: &str = r#"ENCRYPTED CODING SESSION ARCHIVE
================================

This is an encrypted archive of coding session histories - conversations
between a human developer and AI coding assistants like Claude, Copilot,
or Aider.

WHAT'S INSIDE
-------------
This archive contains {conversation_count} conversations. The contents are
encrypted and can only be viewed with the correct password.

HOW TO ACCESS
-------------
1. Open the web viewer at: {url}
2. Enter the password when prompted
3. Browse and search your conversations

PRIVACY
-------
- All decryption happens in your web browser
- Your password is never sent to any server
- The encrypted data cannot be read without the password
- Even the hosting service cannot see your conversations

FORGOT YOUR PASSWORD?
---------------------
See the "recovery.html" file for options. Without the correct password
or a recovery key, the archive cannot be decrypted.

MORE INFORMATION
----------------
This archive was created with CASS (Coding Agent Session Search).
For technical details, see SECURITY.md.

---
Created: {date}
Version: CASS v{version}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::summary::{
        AgentSummaryItem, DateRange, EncryptionSummary, KeySlotSummary, PrePublishSummary,
        ScanReportSummary, WorkspaceSummaryItem,
    };

    fn create_test_summary() -> PrePublishSummary {
        PrePublishSummary {
            total_conversations: 42,
            total_messages: 1000,
            total_characters: 500_000,
            estimated_size_bytes: 200_000,
            earliest_timestamp: Some(Utc::now() - chrono::Duration::days(30)),
            latest_timestamp: Some(Utc::now()),
            date_histogram: vec![],
            workspaces: vec![WorkspaceSummaryItem {
                path: "/home/user/project".to_string(),
                display_name: "project".to_string(),
                conversation_count: 20,
                message_count: 500,
                date_range: DateRange {
                    earliest: None,
                    latest: None,
                },
                sample_titles: vec!["Fix bug".to_string()],
                included: true,
            }],
            agents: vec![
                AgentSummaryItem {
                    name: "claude-code".to_string(),
                    conversation_count: 30,
                    message_count: 700,
                    percentage: 71.4,
                    included: true,
                },
                AgentSummaryItem {
                    name: "aider".to_string(),
                    conversation_count: 12,
                    message_count: 300,
                    percentage: 28.6,
                    included: true,
                },
            ],
            secret_scan: ScanReportSummary::default(),
            encryption_config: Some(EncryptionSummary::default()),
            key_slots: vec![
                KeySlotSummary {
                    slot_index: 0,
                    slot_type: KeySlotType::Password,
                    hint: None,
                    created_at: Some(Utc::now()),
                },
                KeySlotSummary {
                    slot_index: 1,
                    slot_type: KeySlotType::Recovery,
                    hint: None,
                    created_at: Some(Utc::now()),
                },
            ],
            generated_at: Utc::now(),
        }
    }

    #[test]
    fn test_generate_readme() {
        let config = DocConfig::new().with_url("https://example.github.io/archive");
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_readme();

        assert_eq!(doc.filename, "README.md");
        assert_eq!(doc.location, DocLocation::RepoRoot);
        assert!(doc.content.contains("Encrypted Coding Session Archive"));
        assert!(doc.content.contains("42 conversations"));
        assert!(doc.content.contains("claude-code"));
        assert!(doc.content.contains("aider"));
        assert!(doc.content.contains("https://example.github.io/archive"));
        assert!(doc.content.contains("2 independent decryption key(s)"));
    }

    #[test]
    fn test_generate_security_doc() {
        let config = DocConfig::new().with_argon_params(131072, 4, 8);
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_security_doc();

        assert_eq!(doc.filename, "SECURITY.md");
        assert_eq!(doc.location, DocLocation::RepoRoot);
        assert!(doc.content.contains("Security Model"));
        assert!(doc.content.contains("AES-256-GCM"));
        assert!(doc.content.contains("Argon2id"));
        assert!(doc.content.contains("m=131072KB"));
        assert!(doc.content.contains("t=4"));
        assert!(doc.content.contains("p=8"));
        assert!(doc.content.contains("2 key slot(s)"));
        assert!(doc.content.contains("Password-derived"));
        assert!(doc.content.contains("Recovery phrase"));
    }

    #[test]
    fn test_generate_help_html() {
        let config = DocConfig::new();
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_help_html();

        assert_eq!(doc.filename, "help.html");
        assert_eq!(doc.location, DocLocation::WebRoot);
        assert!(doc.content.contains("<!DOCTYPE html>"));
        assert!(doc.content.contains("<title>Help - CASS Archive</title>"));
        assert!(doc.content.contains("Accessing the Archive"));
        assert!(doc.content.contains("Searching"));
        assert!(doc.content.contains("Troubleshooting"));
    }

    #[test]
    fn test_generate_recovery_html_with_key() {
        let config = DocConfig::new();
        let summary = create_test_summary(); // Has recovery key slot
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_recovery_html();

        assert_eq!(doc.filename, "recovery.html");
        assert_eq!(doc.location, DocLocation::WebRoot);
        assert!(doc.content.contains("Password Recovery"));
        assert!(doc.content.contains("Using Your Recovery Key"));
        assert!(doc.content.contains("Good news!"));
    }

    #[test]
    fn test_generate_recovery_html_without_key() {
        let config = DocConfig::new();
        let mut summary = create_test_summary();
        // Remove recovery key slot
        summary.key_slots = vec![KeySlotSummary {
            slot_index: 0,
            slot_type: KeySlotType::Password,
            hint: None,
            created_at: Some(Utc::now()),
        }];
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_recovery_html();

        assert!(doc.content.contains("not configured with a recovery key"));
        assert!(!doc.content.contains("Good news!"));
    }

    #[test]
    fn test_generate_about_txt() {
        let config = DocConfig::new().with_url("https://example.com/archive");
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_about_txt();

        assert_eq!(doc.filename, "about.txt");
        assert_eq!(doc.location, DocLocation::WebRoot);
        assert!(doc.content.contains("ENCRYPTED CODING SESSION ARCHIVE"));
        assert!(doc.content.contains("42 conversations"));
        assert!(doc.content.contains("https://example.com/archive"));
    }

    #[test]
    fn test_generate_all() {
        let config = DocConfig::new();
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let docs = generator.generate_all();

        assert_eq!(docs.len(), 5);

        let filenames: Vec<_> = docs.iter().map(|d| d.filename.as_str()).collect();
        assert!(filenames.contains(&"README.md"));
        assert!(filenames.contains(&"SECURITY.md"));
        assert!(filenames.contains(&"help.html"));
        assert!(filenames.contains(&"recovery.html"));
        assert!(filenames.contains(&"about.txt"));

        // Check locations
        let repo_root_count = docs
            .iter()
            .filter(|d| d.location == DocLocation::RepoRoot)
            .count();
        let web_root_count = docs
            .iter()
            .filter(|d| d.location == DocLocation::WebRoot)
            .count();
        assert_eq!(repo_root_count, 2); // README.md, SECURITY.md
        assert_eq!(web_root_count, 3); // help.html, recovery.html, about.txt
    }

    #[test]
    fn test_doc_config_builder() {
        let config = DocConfig::new()
            .with_url("https://example.com")
            .with_argon_params(65536, 3, 4);

        assert_eq!(config.target_url, Some("https://example.com".to_string()));
        assert_eq!(config.argon_memory_kb, 65536);
        assert_eq!(config.argon_iterations, 3);
        assert_eq!(config.argon_parallelism, 4);
    }

    #[test]
    fn test_empty_key_slots() {
        let config = DocConfig::new();
        let mut summary = create_test_summary();
        summary.key_slots = vec![];
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_security_doc();

        assert!(doc.content.contains("0 key slot(s)"));
        assert!(doc.content.contains("No key slots configured yet"));
    }

    #[test]
    fn test_readme_without_url() {
        let config = DocConfig::new(); // No URL set
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let doc = generator.generate_readme();

        assert!(doc.content.contains("[deployment URL]"));
    }

    #[test]
    fn test_no_placeholders_remain() {
        let config = DocConfig::new().with_url("https://test.com");
        let summary = create_test_summary();
        let generator = DocumentationGenerator::new(config, summary);

        let docs = generator.generate_all();

        for doc in docs {
            // Check that common placeholders are filled
            assert!(
                !doc.content.contains("{url}") || doc.filename == "help.html",
                "Unfilled {{url}} in {}",
                doc.filename
            );
            assert!(
                !doc.content.contains("{conversation_count}"),
                "Unfilled {{conversation_count}} in {}",
                doc.filename
            );
            assert!(
                !doc.content.contains("{version}"),
                "Unfilled {{version}} in {}",
                doc.filename
            );
        }
    }
}
