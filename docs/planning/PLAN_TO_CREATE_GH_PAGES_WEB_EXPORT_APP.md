# Proposal: Encrypted GitHub Pages Web Export for cass (Chunked Payload Format)

**Document Version:** 1.5
**Date:** January 2026
**Status:** PROPOSAL - Production-Grade Implementation Design

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Background: What is cass?](#2-background-what-is-cass)
3. [Background: What is bv and its Pages Export?](#3-background-what-is-bv-and-its-pages-export)
4. [Problem Statement](#4-problem-statement)
5. [Requirements](#5-requirements)
6. [Proposed Architecture](#6-proposed-architecture)
7. [Security Model](#7-security-model)
8. [User Experience Flow](#8-user-experience-flow)
9. [Technical Implementation Plan](#9-technical-implementation-plan)
10. [File Structure & Bundle Contents](#10-file-structure--bundle-contents)
11. [Frontend Technology Stack](#11-frontend-technology-stack)
12. [CLI Interface Design](#12-cli-interface-design)
13. [Encryption Implementation Details](#13-encryption-implementation-details)
14. [Safety Guardrails](#14-safety-guardrails)
15. [Migration Path & Compatibility](#15-migration-path--compatibility)
16. [Risk Analysis](#16-risk-analysis)
17. [Implementation Phases](#17-implementation-phases)
18. [Open Questions](#18-open-questions)
19. [Appendix: Original Requirements](#19-appendix-original-requirements)

---

## 1. Executive Summary

This proposal describes adding a **secure, encrypted static website export feature** to `cass` (coding-agent-search), enabling users to publish their AI coding agent conversation history to GitHub Pages while protecting sensitive content with client-side encryption.

### Key Innovation

Unlike bv's existing Pages export (which publishes data in plaintext), cass's implementation will use **envelope encryption**:
- A random **Data Encryption Key (DEK)** encrypts the archive payload (AES-256-GCM)
- One or more **Key Encryption Keys (KEKs)** derived via Argon2id wrap the DEK for password + recovery unlock

The static site will be completely opaque until decrypted in the browser—no conversation content, agent names, project paths, or search indexes will be visible to anyone without the decryption key.

### Why This Matters

AI coding agent logs often contain:
- API keys and secrets (accidentally pasted or logged)
- Internal codenames and architecture details
- Debugging sessions with sensitive data
- Proprietary algorithms and business logic

GitHub Pages can use public repos on Free plans and public/private repos on paid plans—but **the resulting Pages site is always publicly accessible on the internet**. Do NOT assume a private repo makes the Pages site private. Encryption is mandatory for safety, not optional.

---

## 2. Background: What is cass?

### Overview

**cass** (coding-agent-search) is a high-performance Rust application that indexes and searches conversations from 10+ AI coding agents:

| Agent | Storage Format | Location |
|-------|---------------|----------|
| Claude Code | JSONL | `~/.claude/projects` |
| Codex | JSONL (Rollout) | `~/.codex/sessions` |
| Cursor | SQLite + JSONL | `~/Library/Application Support/Cursor/` |
| ChatGPT | JSON (encrypted v2/v3) | `~/Library/Application Support/com.openai.chat` |
| Gemini CLI | JSON | `~/.gemini/tmp` |
| Aider | Markdown | `~/.aider.chat.history.md` |
| Cline | JSON | VS Code global storage |
| OpenCode | SQLite | `.opencode/` directories |
| Pi-Agent | JSONL | `~/.pi/agent/sessions` |
| Amp | SQLite + JSON | `~/.local/share/amp` |

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     Agent Files (10+ formats)                │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│           Connectors (parallel via rayon)                    │
│   Normalize to: NormalizedConversation → NormalizedMessage   │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Dual Storage Layer                        │
│  ┌─────────────────────┐    ┌─────────────────────────────┐ │
│  │   SQLite (v5)       │    │   Tantivy Index             │ │
│  │   - Relational data │    │   - Full-text search        │ │
│  │   - Source of truth │    │   - Edge N-grams            │ │
│  │   - Schema migrations│   │   - BM25 ranking            │ │
│  └─────────────────────┘    └─────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Presentation Layer                        │
│  ┌─────────────────────┐    ┌─────────────────────────────┐ │
│  │   TUI (ratatui)     │    │   Robot Mode (JSON)         │ │
│  │   - Three-pane UI   │    │   - AI agent consumption    │ │
│  │   - Interactive     │    │   - Automation pipelines    │ │
│  └─────────────────────┘    └─────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### Key Data Structures

```rust
pub struct NormalizedConversation {
    pub agent_slug: String,           // "claude-code", "codex", etc.
    pub workspace: Option<PathBuf>,   // Project directory
    pub source_path: PathBuf,         // Original file location
    pub started_at: Option<i64>,      // Milliseconds since epoch
    pub messages: Vec<NormalizedMessage>,
    pub source_id: String,            // Provenance: "local", "laptop"
}

pub struct NormalizedMessage {
    pub idx: i64,
    pub role: String,                 // "user", "assistant", "tool", "system"
    pub content: String,
    pub created_at: Option<i64>,
    pub snippets: Vec<NormalizedSnippet>,
}
```

### Current Capabilities

- **Sub-60ms search latency** via edge N-gram indexing
- **Hybrid search**: Lexical (Tantivy) + optional Semantic (MiniLM embeddings)
- **Multi-machine sync**: SSH/rsync with provenance tracking
- **Robot mode**: JSON output for AI agent consumption
- **Export**: Markdown/JSON/plaintext conversation export

### What cass Does NOT Have (Yet)

- Static website generation
- Client-side search capability
- Encrypted data export
- GitHub Pages deployment

---

## 3. Background: What is bv and its Pages Export?

### Overview

**bv** (Beads Viewer) is a Go-based TUI application for the Beads issue tracking system. It provides:

- Multi-view interface (List, Kanban, Graph, Insights, History)
- Graph analysis engine (PageRank, Betweenness, HITS, Critical Path, etc.)
- AI-ready JSON outputs (`--robot-*` commands)
- **Static site export to GitHub Pages or Cloudflare Pages**

### How bv's Pages Export Works

#### CLI Interface

```bash
# Interactive wizard (recommended)
bv --pages

# Direct export
bv --export-pages ./output-dir \
   --pages-title "My Project" \
   --pages-include-history

# Preview locally
bv --preview-pages ./output-dir

# Interactive graph only
bv --export-graph graph.html
```

#### The --pages Wizard Flow

1. **Configuration**: Include closed issues? Include git history? Site title?
2. **Target Selection**: GitHub Pages / Cloudflare Pages / Local export
3. **Target Config**: Repo name, visibility, description
4. **Prerequisites Check**: Verify `gh` or `wrangler` CLI, authentication
5. **Export Bundle**: Generate database + assets
6. **Preview**: Optional local HTTP server
7. **Deploy**: Push to hosting platform

#### Generated Bundle Structure

```
output-dir/
├── index.html              # Main entry point
├── beads.sqlite3           # Client-side SQLite database
├── beads.sqlite3.0         # (chunked if large)
├── beads.sqlite3.1
├── data/
│   ├── triage.json         # Precomputed recommendations
│   ├── insights.json       # Graph metrics
│   ├── history.json        # Git correlations
│   └── graph-layout.json   # Force-directed positions
├── viewer.js               # Main application (100KB)
├── graph.js                # Graph rendering (121KB)
├── charts.js               # Dashboard charts
├── styles.css              # Tailwind CSS
├── vendor/
│   ├── sql-wasm.js         # SQLite WASM loader
│   ├── sql-wasm.wasm       # SQLite WASM binary (640KB)
│   ├── alpine.min.js       # UI framework
│   ├── d3.v7.min.js        # Visualization
│   ├── force-graph.min.js  # Interactive graphs
│   ├── marked.min.js       # Markdown parsing
│   └── mermaid.min.js      # Diagram rendering
└── README.md               # Project overview
```

#### Frontend Technology Stack

| Purpose | Library | Size |
|---------|---------|------|
| Database | sql.js (SQLite WASM) | 640KB |
| UI Framework | Alpine.js | 44KB |
| CSS | Tailwind (JIT) | 398KB |
| Visualization | D3.js v7 | 273KB |
| Graphs | Force-Graph | 194KB |
| Markdown | Marked.js | 36KB |
| Diagrams | Mermaid | 3.2MB |
| Sanitization | DOMPurify | 20KB |

#### Key Implementation Files (Go)

| File | Purpose | Lines |
|------|---------|-------|
| `pkg/export/wizard.go` | Interactive wizard | 850 |
| `pkg/export/sqlite_export.go` | Database generation | 600+ |
| `pkg/export/github.go` | GitHub Pages deployment | 400+ |
| `pkg/export/cloudflare.go` | Cloudflare deployment | 300+ |
| `pkg/export/viewer_embed.go` | Asset embedding | 200+ |

#### How Data is Embedded

1. **SQLite Database**: Issues, dependencies, metrics exported to `beads.sqlite3`
2. **JSON Precomputation**: Triage, insights, history computed server-side
3. **Asset Embedding**: Go's `//go:embed` includes all frontend files in binary
4. **Title Injection**: `index.html` template has `{{.Title}}` placeholder

#### Deployment Flow (GitHub Pages)

```go
// Simplified flow from github.go
func deployToGitHub(config Config) error {
    // 1. Create repository (if needed)
    gh repo create <name> --public --description "..."

    // 2. Clone locally
    git clone <repo-url> temp-dir

    // 3. Copy bundle contents
    cp -r bundle/* temp-dir/

    // 4. Commit and push
    git add -A && git commit -m "Deploy" && git push

    // 5. Enable GitHub Pages
    gh api repos/<owner>/<repo>/pages -X POST \
       -f source.branch=main -f source.path=/

    return nil
}
```

### Critical Limitation of bv's Approach

**bv exports data in PLAINTEXT**. This works for issue trackers (which are typically not sensitive), but is **completely inappropriate for AI coding agent logs**.

---

## 4. Problem Statement

### The Core Challenge

Users want to share their AI coding agent history for:
- **Collaboration**: Team members reviewing debugging approaches
- **Learning**: Building searchable knowledge bases
- **Documentation**: Preserving institutional knowledge
- **Archival**: Long-term storage with easy access

### Why GitHub Pages is Attractive

- **Free hosting** for public repositories
- **Easy deployment** via git push
- **Global CDN** for fast access
- **Custom domains** supported
- **No server maintenance** required

### Why GitHub Pages is Dangerous for Agent Logs

GitHub Pages is commonly published from public repositories (GitHub Free), and can also be published from private repositories on paid plans. Either way, AI coding agent logs often contain:

| Risk Category | Examples |
|--------------|----------|
| **Secrets** | API keys, tokens, passwords (accidentally logged) |
| **Internal Architecture** | Database schemas, service endpoints, auth flows |
| **Proprietary Code** | Algorithms, business logic, unreleased features |
| **Personal Data** | Usernames, emails, file paths with names |
| **Security Vulnerabilities** | Bug discussions, security fixes before deployment |

### The Solution

**Client-side encryption** that makes the exported data completely opaque:

```
┌─────────────────────────────────────────────────────────────┐
│                    Public GitHub Repository                  │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ index.html (auth page only)                             ││
│  │ payload/ (chunked AEAD encrypted database stream)       ││
│  │   ├── chunk-00000.bin                                   ││
│  │   ├── chunk-00001.bin                                   ││
│  │   └── ...                                               ││
│  │ viewer.js (decryption + UI logic)                       ││
│  │ vendor/* (libraries)                                     ││
│  └─────────────────────────────────────────────────────────┘│
│                                                              │
│  Without password: See only "Enter password" prompt          │
│  With password: Full search + browsing capability            │
└─────────────────────────────────────────────────────────────┘
```

---

## 5. Requirements

### 5.1 Functional Requirements

#### FR-1: Content Selection (Interactive + CLI)

| Filter | Default | CLI Flag | Interactive |
|--------|---------|----------|-------------|
| Agents | ALL | `--agents claude-code,codex` | Multi-select checkbox |
| Time Range | ALL | `--since 2024-01-01 --until 2024-12-31` | Date pickers |
| Projects | ALL | `--workspaces /path/one,/path/two` | Multi-select with search |

#### FR-1.1: Path Privacy Controls

- Default export MUST avoid embedding absolute local paths unless explicitly requested
- Support: `--path-mode relative|basename|full|hash` (default: `relative`)
  - `relative`: Store paths relative to workspace root
  - `basename`: Store only the filename
  - `full`: Store absolute paths (with warning)
  - `hash`: Store opaque SHA256 identifiers

#### FR-2: Encryption (Envelope Encryption)

- **Payload Encryption**: AES-256-GCM using a random per-export DEK (Data Encryption Key)
- **Key Derivation**: Argon2id (memory-hard, GPU-resistant) to derive KEKs that wrap the DEK
- **Authentication Methods**:
  - Password entry (derives KEK that unwraps DEK)
  - QR code scan of high-entropy recovery secret (creates additional key slot; NOT published with site)
- **Scope**: ALL data encrypted (database, metadata, search index, pre-computed analytics)
- **Key Slots**: Support multiple passwords/recovery secrets via envelope encryption

#### FR-3: Static Site Generation

- Self-contained bundle (works offline after initial load)
- Client-side SQLite via sqlite-wasm (OPFS preferred, in-memory fallback—same runtime)
- Full-text search capability (dual FTS: natural language + code/path tokenizers)
- Responsive UI (desktop + mobile)
- Virtualized rendering for large archives
- CSP-safe UI layer (no Alpine.js or eval-dependent frameworks)

#### FR-4: Deployment Options

- GitHub Pages (primary target, defaults to `gh-pages` branch)
- Cloudflare Pages (secondary, supports COOP/COEP headers)
- Local export (manual deployment)

#### FR-4.1: Hosting Limits & Guardrails (Chunked AEAD is Primary Format)

Because encrypted archives are not CDN-compressible, we must respect hosting limits:

**GitHub Pages Limits (hard constraints):**
- Published site size: MUST be ≤ 1 GB
- Source repo recommended limit: ≤ 1 GB
- Per-file hard block: 100 MiB; warnings at 50 MiB
- Soft bandwidth limit: 100 GB/month; deploy timeouts may apply

**Chunked AEAD Architecture:**
- Payload MUST be stored as independently-authenticated encrypted chunks (chunked AEAD), enabling streaming decryption and bounded memory usage
- Default chunk size: 8 MiB (configurable). Hard cap: 32 MiB (avoid GitHub >50 MiB warnings)
- Chunking is ALWAYS used when targeting GitHub Pages (regardless of total size), because it simplifies caching, retries, and file-size compliance

**Compression:**
- Payload compression BEFORE encryption to minimize transfer size
- Supported codecs:
  - `deflate` (default): implemented via streaming JS decompressor (fflate)
  - `zstd` (optional): better ratio for huge exports, requires wasm decoder loaded post-unlock
  - `none` (debug/testing only)

#### FR-5: Safety Guardrails

- Unencrypted export requires typing: `I UNDERSTAND AND ACCEPT THE RISKS`
- Pre-publish summary shows: agents, workspaces, time range, message count
- Confirmation prompt before any deployment

#### FR-6: Redaction & Share Profiles (NEW)

Encryption protects archives from the public internet—but once you share the password with a teammate, they can see everything. Redaction provides an additional layer of protection for safe sharing:

**Export Profiles:**
- `private` (default): no redaction; encryption required
- `team`: redact secrets + usernames + hostnames; keep code/context
- `public-redacted`: aggressive redaction + path hashing + optional message exclusions

**Redaction Capabilities:**
- Built-in secret patterns + entropy heuristics (API keys, tokens, passwords)
- User-provided regex rules (`--redact-regex`, `--redact-replace`)
- Allowlist/denylist per workspace / agent / conversation
- Review summary before export (with option to redact, exclude, or continue)

#### FR-7: Attachment Support (Opt-in)

Large assets (images, PDFs, code snapshots) that agents reference can be included in exports:

**Opt-in Behavior:**
- Disabled by default to minimize export size
- Enable with `--include-attachments` or wizard checkbox
- Size limits: 10MB per file, 100MB total (configurable)

**Storage:**
- Attachments stored as separate encrypted blobs in `blobs/` directory
- Each blob named by content hash: `blobs/<sha256>.bin`
- Reference in messages table via `attachment_refs` JSON array

**Viewer Integration:**
- Lazy-load attachments on demand (not prefetched)
- Inline preview for images, syntax-highlighted code
- Download button for non-previewable types

### 5.2 Non-Functional Requirements

#### NFR-1: Security

- Zero plaintext content in public repository
- No metadata leakage (file names, sizes reveal nothing)
- Forward secrecy considerations (optional key rotation)

#### NFR-2: Performance (with explicit security tradeoff)

- Initial page load: < 3 seconds on 3G
- Search latency: < 100ms after decryption
- Database size: Efficient chunking for large exports
- Download size: Payload is compacted (SQLite VACUUM) + compressed (deflate) BEFORE encryption
- Streaming decrypt: Chunks decrypted and written to OPFS incrementally (bounded memory)
- OPFS persistence (OPT-IN): Store decrypted database in OPFS for instant subsequent loads. Default is memory-only session for maximum security.

#### NFR-3: Usability

- Wizard experience matching bv's polish
- Clear error messages for auth failures
- Progress indicators for long operations

#### NFR-4: Compatibility

- Modern browsers (Chrome 90+, Firefox 88+, Safari 14+)
- WASM support required
- JavaScript required (no graceful degradation)

---

## 6. Proposed Architecture

### High-Level Flow

```
┌─────────────────────────────────────────────────────────────┐
│                    cass CLI (Rust)                           │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ 1. User invokes: cass pages (interactive)               ││
│  │    or: cass export-pages --agents=... --password=...    ││
│  │                                                          ││
│  │ 2. Content Selection:                                    ││
│  │    - Query SQLite for matching conversations             ││
│  │    - Apply agent/time/workspace filters                  ││
│  │    - Build export manifest                               ││
│  │                                                          ││
│  │ 3. Export Database:                                      ││
│  │    - Create new SQLite with filtered content             ││
│  │    - Build FTS5 search index                             ││
│  │    - Compute statistics and metadata                     ││
│  │                                                          ││
│  │ 4. Encrypt:                                              ││
│  │    - Derive key via Argon2id(password, salt)             ││
│  │    - Encrypt compressed DB stream as AEAD chunks         ││
│  │    - Generate QR code (optional)                         ││
│  │                                                          ││
│  │ 5. Bundle:                                               ││
│  │    - Copy viewer assets                                  ││
│  │    - Inject configuration (salt, nonce, auth hints)      ││
│  │    - Generate README                                     ││
│  │                                                          ││
│  │ 6. Deploy (optional):                                    ││
│  │    - GitHub Pages via gh CLI                             ││
│  │    - Cloudflare Pages via wrangler                       ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                 Generated Static Site                        │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ User visits site:                                        ││
│  │   1. index.html loads (minimal, no sensitive data)       ││
│  │   2. Auth modal appears (password or QR scan)            ││
│  │   3. On success:                                         ││
│  │      - Derive key in browser (Argon2id via WASM)         ││
│  │      - Decrypt payload/chunk-*.bin → SQLite database     ││
│  │      - Initialize sqlite-wasm with decrypted data        ││
│  │      - Render full search UI                             ││
│  │   4. On failure:                                         ││
│  │      - Show error, remain on auth screen                 ││
│  │      - No data exposed                                   ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
```

### Component Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                      Rust CLI (cass)                         │
├─────────────────────────────────────────────────────────────┤
│ ┌───────────────┐ ┌───────────────┐ ┌─────────────────────┐ │
│ │ PagesWizard   │ │ ExportEngine  │ │ EncryptionModule    │ │
│ │               │ │               │ │                     │ │
│ │ - Interactive │ │ - Filter data │ │ - Argon2id KDF      │ │
│ │ - CLI args    │ │ - Build SQLite│ │ - AES-256-GCM       │ │
│ │ - Validation  │ │ - FTS5 index  │ │ - QR generation     │ │
│ └───────────────┘ └───────────────┘ └─────────────────────┘ │
│ ┌───────────────┐ ┌───────────────┐ ┌─────────────────────┐ │
│ │ BundleBuilder │ │ Deployer      │ │ AssetEmbed          │ │
│ │               │ │               │ │                     │ │
│ │ - Copy assets │ │ - GitHub      │ │ - HTML templates    │ │
│ │ - Inject conf │ │ - Cloudflare  │ │ - JS/CSS/WASM       │ │
│ │ - Generate QR │ │ - Local       │ │ - Vendor libs       │ │
│ └───────────────┘ └───────────────┘ └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                    Browser Runtime                           │
├─────────────────────────────────────────────────────────────┤
│ ┌───────────────┐ ┌───────────────┐ ┌─────────────────────┐ │
│ │ AuthModule    │ │ CryptoModule  │ │ DatabaseModule      │ │
│ │               │ │               │ │                     │ │
│ │ - Password UI │ │ - Argon2 WASM │ │ - sql.js WASM       │ │
│ │ - QR scanner  │ │ - AES-GCM     │ │ - FTS5 queries      │ │
│ │ - Session mgmt│ │ - Key storage │ │ - Result rendering  │ │
│ └───────────────┘ └───────────────┘ └─────────────────────┘ │
│ ┌───────────────┐ ┌───────────────┐ ┌─────────────────────┐ │
│ │ SearchUI      │ │ ConversationUI│ │ ExportUI            │ │
│ │               │ │               │ │                     │ │
│ │ - Query input │ │ - Message list│ │ - Copy/download     │ │
│ │ - Filters     │ │ - Syntax hl   │ │ - Share links       │ │
│ │ - Results     │ │ - Navigation  │ │ - Print view        │ │
│ └───────────────┘ └───────────────┘ └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

---

## 7. Security Model

### 7.1 Threat Model

#### Assets to Protect

1. **Conversation content**: User prompts, assistant responses
2. **Metadata**: Agent names, workspace paths, timestamps
3. **Search index**: Terms, frequencies, positions
4. **Statistics**: Counts, distributions, patterns

#### Adversaries

| Adversary | Capability | Mitigation |
|-----------|------------|------------|
| **Casual Observer** | Views public repo | All data encrypted |
| **GitHub Employee** | Access to repo storage | Encryption at rest |
| **Network Attacker** | MITM on HTTPS | HTTPS + SRI hashes |
| **Browser Extension** | DOM access post-auth | Content Security Policy |
| **Shoulder Surfer** | Sees password entry | QR code alternative |

#### Out of Scope

- Keyloggers on user's machine
- Malicious browser extensions with full DOM access
- Targeted attacks with physical access
- Quantum computing attacks (future consideration)

#### Additional Explicit Risk: Bundle Tampering / Repo Compromise

If an attacker can modify the deployed static assets (viewer.js/index.html), they can potentially steal passwords during unlock. Mitigations are limited on static hosting; we implement:
- **TOFU asset-hash warnings**: Store hash of critical assets after first successful unlock; warn loudly if assets change on subsequent visits before accepting a password
- **Commit-pinned URLs**: Guidance to share commit-pinned URLs/hashes out-of-band for high-trust sharing

**Integrity Fingerprint (recommended hardening):**
- Generate `site/integrity.json` containing SHA-256 for all public files (index.html, JS/CSS/WASM, config.json, payload chunks)
- Generate `private/integrity-fingerprint.txt` containing `SHA-256(integrity.json)`
- Viewer displays the fingerprint before password entry:
  "Verify fingerprint matches what the archive owner sent you before unlocking."
- This provides a practical out-of-band verification path (works even on first visit)

```javascript
// integrity.json structure
{
    "version": 1,
    "generated_at": "2025-01-06T12:34:56Z",
    "files": {
        "index.html": "sha256-abc123...",
        "viewer.js": "sha256-def456...",
        "config.json": "sha256-ghi789...",
        "payload/chunk-00000.bin": "sha256-jkl012...",
        // ...
    }
}

// In auth UI (before password entry)
function showIntegrityFingerprint() {
    const fp = document.getElementById('fingerprint');
    fp.textContent = `Fingerprint: ${shortFingerprint}`;
    fp.title = 'Verify this matches what the archive owner sent you';
}
```

### 7.2 Cryptographic Design (Envelope Encryption + AAD Binding)

We use **envelope encryption** to separate the data key from the user's password, with **AAD binding** to cryptographically tie all components together:

```
┌─────────────────────────────────────────────────────────────┐
│            Envelope Encryption Model + AAD Binding           │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  export_id (16 bytes random):                                │
│    - Unique per export                                       │
│    - Used as AAD for all AEAD operations                    │
│    - Binds config.json ↔ payload chunks ↔ key slots         │
│                                                              │
│  DEK (Data Encryption Key):                                  │
│    - Random 256-bit key generated per export                 │
│    - Encrypts the compressed archive payload chunks          │
│    - Never stored in plaintext                               │
│                                                              │
│  KEK (Key Encryption Key):                                   │
│    - Derived from password/recovery-secret via Argon2id      │
│    - Wraps (encrypts) the DEK                               │
│    - Multiple KEKs = multiple "key slots"                   │
│                                                              │
│  Benefits:                                                   │
│    ✓ Password rotation without re-encrypting payload         │
│    ✓ Multiple passwords (key slots, like LUKS)              │
│    ✓ Separate recovery secret (QR) from user password       │
│    ✓ AAD prevents chunk swapping/replay attacks             │
│                                                              │
╰─────────────────────────────────────────────────────────────╯
```

#### Key Derivation (KEK)

**Password slots (human-memorable secrets):**
```
Password → Argon2id → 256-bit KEK
           ├─ Memory: 64 MB (65536 KB)
           ├─ Iterations: 3
           ├─ Parallelism: 4
           └─ Salt: 16 bytes (random, per key slot)
```

**Recovery slots (high-entropy secrets):**
```
RecoverySecret → HKDF-SHA256 → 256-bit KEK
                 └─ Salt: 16 bytes (random, per key slot)
```

**Why two KDFs?**
- Argon2id is memory-hard (resists GPU/ASIC attacks on weak passwords)
- Recovery secrets are high-entropy (128+ bits); memory-hard KDF is wasted latency
- HKDF-SHA256 is fast and sufficient for uniformly random inputs
- This improves mobile unlock UX when using recovery secrets

**Why Argon2id for passwords?**
- Memory-hard (resists GPU/ASIC attacks)
- Hybrid design (resists side-channel + time-memory tradeoffs)
- Winner of Password Hashing Competition (2015)
- OWASP recommended

#### Chunk Encryption (DEK → Payload Chunks)

```
DEK + Nonce + CompressedChunk + AAD(export_id, chunk_index, schema_version)
    → AES-256-GCM → Ciphertext + AuthTag
                    ├─ DEK: 256 bits (random per export)
                    ├─ Nonce: 96 bits (counter-based; no XOR)
                    │   base_nonce = 8B random prefix || 4B random counter_start
                    │   nonce(i)   = prefix || (counter_start + i) mod 2^32
                    │   ENFORCE: chunk_count < 2^32
                    ├─ AuthTag: 128 bits (integrity)
                    └─ AAD: prevents chunk reorder/swap attacks
```

**Why counter-based nonce (not XOR)?**
- XOR-based derivation is error-prone (endianness, XOR width differ across Rust/JS)
- Counter-based is simpler to specify and test across implementations
- AES-GCM is unforgiving: a nonce reuse is catastrophic (key recovery possible)

#### Key Wrapping (KEK → DEK)

```
KEK + Nonce + DEK + AAD(export_id, slot_id) → AES-256-GCM → WrappedDEK + AuthTag
                    ├─ KEK: 256 bits (from Argon2id)
                    ├─ Nonce: 96 bits (random, per slot)
                    ├─ AuthTag: 128 bits (integrity)
                    └─ AAD: binds slot to this specific export
```

**Why AES-256-GCM for both?**
- Authenticated encryption (integrity + confidentiality)
- Hardware acceleration (AES-NI)
- Widely audited and deployed
- NIST approved

#### QR Code Authentication (Local-Only Artifact)

**CRITICAL RULE:** The QR image MUST NOT be included in the deployed GitHub Pages bundle. It is a convenience unlock factor that must remain out-of-band (e.g., printed, stored in a password manager, or shown on a second device).

```
┌─────────────────────────────────────────────────────────────┐
│  Recovery Secret (QR) creates an additional key slot:        │
│                                                              │
│  QR encodes → High-entropy recovery secret (base64)          │
│            → Argon2id → KEK (recovery slot)                  │
│            → Unwraps DEK → Decrypts payload                  │
│                                                              │
│  The recovery secret is NOT the user's password.             │
│  This allows separate rotation/revocation.                   │
│                                                              │
│  Export output is split into:                                │
│    - site/    → safe to deploy publicly                     │
│    - private/ → never deployed (QR image + recovery text)   │
╰─────────────────────────────────────────────────────────────╯
```

### 7.3 What Remains Visible

Even with encryption, some information is observable:

| Observable | Mitigation |
|------------|------------|
| Bundle exists | Unavoidable (GitHub repo is public) |
| Approximate size | Pad to fixed sizes (optional) |
| Last update time | Unavoidable (git history) |
| That cass was used | Consider generic filenames |

### 7.4 Session Management

```javascript
// After successful decryption:
const SESSION_DURATION = 4 * 60 * 60 * 1000; // 4 hours

// Option 1: Keep key in memory only (most secure)
window.sessionKey = derivedKey; // Lost on refresh

// Option 2: SessionStorage (survives refresh, not tabs)
sessionStorage.setItem('cass_session', encryptedKeyBlob);

// Option 3: "Remember me" with localStorage (least secure)
// NOT RECOMMENDED for sensitive data
```

### 7.5 Content Security Policy (Learned from bv)

bv implements strict CSP headers to prevent XSS and code injection. We adopt and strengthen this:

#### CSP Meta Tag (index.html)

```html
<meta http-equiv="Content-Security-Policy" content="
    default-src 'self';
    script-src 'self' 'wasm-unsafe-eval';
    style-src 'self';
    img-src 'self' data: blob:;
    font-src 'self';
    connect-src 'self';
    worker-src 'self' blob:;
    object-src 'none';
    frame-ancestors 'none';
    form-action 'none';
    base-uri 'none';
    upgrade-insecure-requests;
">

<!-- Additional hardening (works as meta in static hosting environments) -->
<meta name="referrer" content="no-referrer">
<meta http-equiv="Permissions-Policy" content="camera=(self), microphone=()">
<meta name="robots" content="noindex,nofollow">
```

#### CSP Directives Explained

| Directive | Value | Purpose |
|-----------|-------|---------|
| `default-src` | `'self'` | Only load resources from same origin |
| `script-src` | `'self' 'wasm-unsafe-eval'` | Allow same-origin JS + WASM compilation |
| `style-src` | `'self'` | Only external stylesheets (no unsafe-inline) |
| `img-src` | `'self' data: blob:` | Allow inline images + QR camera preview |
| `connect-src` | `'self'` | Only fetch from same origin |
| `worker-src` | `'self' blob:` | Allow service workers |
| `object-src` | `'none'` | Block plugins (Flash, Java, etc.) |
| `frame-ancestors` | `'none'` | Prevent embedding in iframes (clickjacking) |
| `form-action` | `'none'` | Prevent form submissions (no forms in viewer) |
| `base-uri` | `'none'` | Prevent base tag injection attacks |

#### Why `wasm-unsafe-eval` is Required

- sqlite-wasm and Argon2 WASM require `WebAssembly.compile()` or `WebAssembly.instantiate()`
- These functions trigger CSP's `eval` restrictions
- `wasm-unsafe-eval` is a targeted exception for WASM only (not general JS eval)
- Available in Chrome 97+, Firefox 102+, Safari 16+

#### Why No `unsafe-inline` in style-src

Alpine.js (removed) required `unsafe-inline` for its reactive expressions. With a CSP-safe UI layer:
- Styles are in external CSS files (Tailwind JIT purged)
- No inline style attributes generated by JS
- This makes XSS substantially harder to exploit

#### Input Sanitization

Despite CSP, we still sanitize all user content before rendering:

```javascript
import DOMPurify from 'dompurify';

// Optional but recommended: Trusted Types enforcement (if supported).
// - Prevents accidental assignment of unsanitized HTML into innerHTML sinks.
// - DOMPurify can be configured to return TrustedHTML.
// - If enabled, add: require-trusted-types-for 'script' (supported browsers only)

// Configuration matching bv's settings
const SANITIZE_CONFIG = {
    ALLOWED_TAGS: ['p', 'br', 'strong', 'em', 'code', 'pre', 'ul', 'ol', 'li', 'a', 'h1', 'h2', 'h3', 'h4', 'blockquote'],
    ALLOWED_ATTR: ['href', 'title', 'class'],
    ALLOW_DATA_ATTR: false,
    ADD_ATTR: ['target', 'rel'], // For links
    FORBID_TAGS: ['script', 'style', 'iframe', 'object', 'embed', 'form'],
    FORBID_ATTR: ['onerror', 'onclick', 'onload', 'onmouseover'],
};

function renderMessage(content) {
    // Parse Markdown first
    const html = marked.parse(content);
    // Then sanitize
    return DOMPurify.sanitize(html, SANITIZE_CONFIG);
}
```

### 7.6 Service Worker for Cross-Origin Isolation + Offline Caching (Encrypted-Only Payload Cache)

GitHub Pages does not allow configuring arbitrary response headers directly, but **COOP/COEP can be applied via a Service Worker** on subsequent loads, enabling cross-origin isolation (SharedArrayBuffer / WASM threads) even on static hosting.

We adopt the **coi-serviceworker** approach with an **encrypted payload cache** for offline capability:

```javascript
// sw.js - Cross-Origin Isolation + Offline Caching Service Worker
const CACHE_NAME = 'cass-archive-v1';
const IMMUTABLE_ASSETS = [
    './vendor/sqlite3.wasm',
    './vendor/argon2-wasm.wasm',
    './vendor/fflate.min.js',
    './styles.css'
];

// Encrypted payload can be cached safely (still requires password)
// Prefetched only when the user opts in ("Make available offline")
const ENCRYPTED_ASSETS = ['./config.json', './integrity.json'];

self.addEventListener('install', (event) => {
    event.waitUntil(
        caches.open(CACHE_NAME).then(cache => cache.addAll(IMMUTABLE_ASSETS))
    );
    self.skipWaiting();
});

self.addEventListener('activate', (event) => {
    event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);

    // Only handle same-origin requests
    if (url.origin !== location.origin) {
        return; // Let browser handle cross-origin
    }

    // For navigation requests, inject COOP/COEP headers
    if (event.request.mode === 'navigate') {
        event.respondWith(
            fetch(event.request).then(response => {
                const headers = new Headers(response.headers);
                headers.set('Cross-Origin-Opener-Policy', 'same-origin');
                headers.set('Cross-Origin-Embedder-Policy', 'require-corp');
                return new Response(response.body, {
                    status: response.status,
                    statusText: response.statusText,
                    headers
                });
            })
        );
        return;
    }

    // Cache-first for immutable assets
    event.respondWith(
        caches.match(event.request).then(cached => {
            return cached || fetch(event.request);
        })
    );
});

// Client → SW message to prefetch encrypted payload for offline use
self.addEventListener('message', (event) => {
    if (event.data?.type === 'PREFETCH_ENCRYPTED') {
        event.waitUntil((async () => {
            const cache = await caches.open(CACHE_NAME);
            await cache.addAll(ENCRYPTED_ASSETS);
            // payload chunk list is discovered from config.json
            const cfg = await (await fetch('./config.json')).json();
            await cache.addAll(cfg.payload.files);
        })());
    }
    if (event.data?.type === 'CLEAR_OFFLINE') {
        event.waitUntil(caches.delete(CACHE_NAME));
    }
});
```

#### Offline Capability via Encrypted Payload Cache

Caching encrypted payload is safe and useful:
- **Safe**: Encrypted chunks are opaque without the password
- **Useful**: Enables unlock without network after first download
- **User-initiated**: Only cached when user clicks "Make available offline"

```javascript
// In viewer.js - UI action to enable offline mode
async function enableOfflineMode() {
    const registration = await navigator.serviceWorker.ready;
    registration.active.postMessage({ type: 'PREFETCH_ENCRYPTED' });
    showToast('Downloading for offline access...');
}

async function clearOfflineData() {
    const registration = await navigator.serviceWorker.ready;
    registration.active.postMessage({ type: 'CLEAR_OFFLINE' });
    showToast('Offline data cleared');
}
```

#### Registration (in viewer.js)

```javascript
// Register service worker early (relative path for GitHub project pages)
if ('serviceWorker' in navigator) {
    navigator.serviceWorker.register('./sw.js', { scope: './' })
        .then(reg => console.log('SW registered:', reg.scope))
        .catch(err => console.warn('SW registration failed:', err));
}
```

#### Important UX Note: Two-Load Pattern

Cross-origin isolation via Service Worker requires a page reload:
- **First visit**: Installs Service Worker (no COI yet)
- **Second load** (automatic or prompted refresh): Cross-origin isolated, SharedArrayBuffer available

The viewer should detect this and prompt for a one-time refresh on first visit.

#### Benefits of COI Service Worker

| Feature | Without COI | With COI |
|---------|-------------|----------|
| Argon2 parallelism | Single-threaded (~3-9s) | Multi-threaded (~1-3s) |
| SharedArrayBuffer | Not available | Available |
| sqlite-wasm OPFS | Limited | Full support |
| Offline unlock | Not available | Cached assets work offline |

---

## 8. User Experience Flow

### 8.1 Export Wizard (Interactive Mode)

```
$ cass pages

╭─────────────────────────────────────────────────────────────╮
│           🔐 cass Pages Export Wizard                        │
│                                                              │
│   Create an encrypted, searchable web archive of your       │
│   AI coding agent conversations.                            │
╰─────────────────────────────────────────────────────────────╯

Step 1 of 7: Content Selection

? Which agents would you like to include?
  ◉ Claude Code (1,234 conversations)
  ◉ Codex (567 conversations)
  ◎ Cursor (89 conversations)
  ◉ Gemini (234 conversations)
  ◎ Aider (45 conversations)
  [Select all] [Select none]

? Time range:
  ◉ All time (2,169 conversations)
  ◎ Last 30 days (342 conversations)
  ◎ Last 90 days (891 conversations)
  ◎ Custom range...

? Which workspaces/projects?
  ◉ All workspaces (47 projects)
  ◎ Select specific...

──────────────────────────────────────────────────────────────

Step 2 of 7: Security Configuration

? Set a password for encryption:
  > ••••••••••••••••

  ℹ Password strength: Strong ████████░░
  ℹ This password will be required to view the exported site

? Generate recovery QR code?
  ◉ Yes (saved locally to private/ - NOT deployed to site)
  ◎ No

──────────────────────────────────────────────────────────────

Step 3 of 7: Site Configuration

? Site title (shown AFTER unlock): My Agent Archive
? Site description (shown AFTER unlock): Searchable archive of my AI coding sessions

? Show title/description on public auth page?
  ◎ No (recommended: avoids metadata leakage)
  ◉ Yes (WARNING: becomes visible to anyone)

  ℹ Default: Public page shows "Encrypted cass Archive"
  ℹ Real title/description stored in encrypted metadata, shown after unlock

──────────────────────────────────────────────────────────────

Step 4 of 7: Deployment Target

? Where would you like to deploy?
  ◉ GitHub Pages (requires gh CLI)
  ◎ Cloudflare Pages (requires wrangler)
  ◎ Local export only

? Repository name: my-agent-archive
? Repository visibility:
  ◉ Public (required for free GitHub Pages)
  ◎ Private (requires GitHub Pro/Team/Enterprise)

──────────────────────────────────────────────────────────────

Step 5 of 7: Pre-Publish Summary

╭─────────────────────────────────────────────────────────────╮
│                    ⚠️  REVIEW CAREFULLY                      │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Agents included:                                            │
│    • Claude Code (1,234 conversations, 45,678 messages)      │
│    • Codex (567 conversations, 12,345 messages)              │
│    • Gemini (234 conversations, 5,678 messages)              │
│                                                              │
│  Time range: 2023-06-15 to 2025-01-06                       │
│                                                              │
│  Workspaces included:                                        │
│    • /home/user/projects/webapp (423 conversations)          │
│    • /home/user/projects/api (312 conversations)             │
│    • /home/user/projects/ml-pipeline (156 conversations)     │
│    • ... and 44 more                                         │
│                                                              │
│  Total: 2,035 conversations, 63,701 messages                 │
│  Estimated bundle size: 24.5 MB (encrypted)                  │
│                                                              │
│  Encryption: AES-256-GCM with Argon2id key derivation        │
│  Password: Set ✓                                             │
│  QR Code: Will be generated                                  │
│                                                              │
│  Deployment: GitHub Pages (public repository)                │
│  URL: https://username.github.io/my-agent-archive            │
│                                                              │
╰─────────────────────────────────────────────────────────────╯

? Proceed with export and deployment? (y/N)

──────────────────────────────────────────────────────────────

Step 6 of 7: Export Progress

  Filtering conversations... ████████████████████ 100%
  Building search index... ████████████████████ 100%
  Encrypting database... ████████████████████ 100%
  Generating QR code... ████████████████████ 100%
  Bundling assets... ████████████████████ 100%

  ✓ Export complete: ./cass-pages-export/

──────────────────────────────────────────────────────────────

Step 7 of 7: Deployment

  Creating repository... ✓
  Pushing files... ████████████████████ 100%
  Enabling GitHub Pages... ✓

╭─────────────────────────────────────────────────────────────╮
│                        🎉 Success!                           │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Your encrypted archive is now live at:                      │
│  https://username.github.io/my-agent-archive                 │
│                                                              │
│  Output directories:                                         │
│    • site/    → deployed (safe to publish)                  │
│    • private/ → NOT deployed (QR code, recovery secrets)    │
│                                                              │
│  ⚠️  Keep your password AND private/ folder safe!            │
│     Without them, the archive cannot be decrypted.          │
│                                                              │
╰─────────────────────────────────────────────────────────────╯
```

### 8.1.1 Wizard Implementation Details (Learned from bv)

bv uses the `charmbracelet/huh` Go library for its wizard. For Rust, we use `dialoguer` + `indicatif` + `console` to achieve similar UX:

#### Wizard State Machine

```rust
use dialoguer::{Confirm, Input, MultiSelect, Password, Select, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use console::{style, Term};

#[derive(Debug, Clone)]
pub struct WizardState {
    // Step 1: Content Selection
    pub agents: Vec<String>,
    pub time_range: TimeRange,
    pub workspaces: Vec<PathBuf>,

    // Step 2: Security
    pub password: Option<String>,
    pub generate_qr: bool,

    // Step 3: Site Config
    pub title: String,
    pub description: String,

    // Step 4: Deployment
    pub target: DeployTarget,
    pub repo_name: Option<String>,

    // Internal
    pub current_step: usize,
    pub total_steps: usize,
}

impl WizardState {
    pub fn run_interactive(&mut self) -> Result<(), WizardError> {
        let term = Term::stdout();
        let theme = ColorfulTheme::default();

        // Print header
        self.print_header(&term)?;

        // Step 1: Content Selection
        self.step_content_selection(&term, &theme)?;

        // Step 2: Security Configuration
        self.step_security(&term, &theme)?;

        // Step 3: Site Configuration
        self.step_site_config(&term, &theme)?;

        // Step 4: Deployment Target
        self.step_deployment(&term, &theme)?;

        // Step 5: Pre-Publish Summary (with confirmation)
        if !self.step_summary(&term, &theme)? {
            return Err(WizardError::Cancelled);
        }

        // Step 6: Export Progress
        self.step_export(&term)?;

        // Step 7: Deploy
        self.step_deploy(&term)?;

        Ok(())
    }
}
```

#### Dynamic Content Stats (like bv)

```rust
/// Fetch live statistics for wizard display
pub struct ContentStats {
    pub agents: Vec<AgentStats>,
    pub total_conversations: usize,
    pub total_messages: usize,
    pub time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl ContentStats {
    /// Query the database for current statistics
    pub fn from_database(db: &Database) -> Result<Self, Error> {
        // Fast aggregate queries
        let agents = db.query_all("
            SELECT agent, COUNT(*) as conv_count,
                   SUM(message_count) as msg_count
            FROM conversations
            GROUP BY agent
            ORDER BY conv_count DESC
        ")?;

        // ... build stats
    }

    /// Format for multi-select display
    pub fn agent_choices(&self) -> Vec<String> {
        self.agents.iter().map(|a| {
            format!("{} ({} conversations)", a.name, a.count)
        }).collect()
    }
}
```

#### Progress Display (matching bv's style)

```rust
/// Multi-step progress display
pub fn create_export_progress() -> MultiProgress {
    let mp = MultiProgress::new();

    let style = ProgressStyle::default_bar()
        .template("{prefix:.bold.dim} {bar:40.cyan/blue} {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("█▓░");

    // Create progress bars for each phase
    let pb_filter = mp.add(ProgressBar::new(100));
    pb_filter.set_prefix("Filtering");
    pb_filter.set_style(style.clone());

    let pb_index = mp.add(ProgressBar::new(100));
    pb_index.set_prefix("Indexing");
    pb_index.set_style(style.clone());

    // ... more progress bars

    mp
}
```

#### Prerequisite Checking (from bv)

bv performs prerequisite checks before proceeding. We adopt this pattern:

```rust
#[derive(Debug)]
pub struct Prerequisites {
    pub gh_cli: Option<String>,       // Version if installed
    pub gh_authenticated: bool,
    pub wrangler_cli: Option<String>,
    pub wrangler_authenticated: bool,
    pub disk_space_mb: u64,
    pub estimated_size_mb: u64,
}

impl Prerequisites {
    pub fn check() -> Self {
        Self {
            gh_cli: Self::check_gh_version(),
            gh_authenticated: Self::check_gh_auth(),
            wrangler_cli: Self::check_wrangler_version(),
            wrangler_authenticated: Self::check_wrangler_auth(),
            disk_space_mb: Self::available_disk_space(),
            estimated_size_mb: 0, // Calculated after content selection
        }
    }

    fn check_gh_version() -> Option<String> {
        std::process::Command::new("gh")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.lines().next().unwrap_or("").to_string())
    }

    fn check_gh_auth() -> bool {
        std::process::Command::new("gh")
            .args(["auth", "status"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn display_status(&self, term: &Term) -> std::io::Result<()> {
        writeln!(term, "\n{}", style("Prerequisites Check:").bold())?;

        // GitHub CLI
        match &self.gh_cli {
            Some(v) => writeln!(term, "  {} gh CLI: {}", style("✓").green(), v)?,
            None => writeln!(term, "  {} gh CLI: not installed", style("✗").red())?,
        }

        if self.gh_cli.is_some() {
            if self.gh_authenticated {
                writeln!(term, "  {} gh authenticated", style("✓").green())?;
            } else {
                writeln!(term, "  {} gh not authenticated (run: gh auth login)", style("✗").red())?;
            }
        }

        // Disk space
        if self.disk_space_mb > self.estimated_size_mb * 2 {
            writeln!(term, "  {} Disk space: {} MB available",
                style("✓").green(), self.disk_space_mb)?;
        } else {
            writeln!(term, "  {} Low disk space: {} MB (need ~{} MB)",
                style("⚠").yellow(), self.disk_space_mb, self.estimated_size_mb * 2)?;
        }

        Ok(())
    }
}
```

### 8.2 Unencrypted Export (Requires Explicit Acknowledgment)

```
$ cass pages --no-encryption

╭─────────────────────────────────────────────────────────────╮
│                    ⚠️  SECURITY WARNING                      │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  You are about to export your AI coding agent conversations  │
│  WITHOUT ENCRYPTION to a PUBLIC GitHub repository.           │
│                                                              │
│  This means ANYONE ON THE INTERNET can view:                 │
│    • All your prompts and AI responses                       │
│    • File paths and project names                            │
│    • Any secrets accidentally included in conversations      │
│    • Your coding patterns and debugging approaches           │
│                                                              │
│  This data CANNOT be made private after publishing.          │
│                                                              │
╰─────────────────────────────────────────────────────────────╯

? To proceed, type exactly: I UNDERSTAND AND ACCEPT THE RISKS
  > _
```

### 8.3 Web UI Authentication Flow

```
┌─────────────────────────────────────────────────────────────┐
│                                                              │
│                    🔐 cass Archive                           │
│                                                              │
│         This archive is encrypted for your privacy.          │
│                                                              │
│  ┌─────────────────────────────────────────────────────────┐│
│  │                                                          ││
│  │  Password: [••••••••••••••]                              ││
│  │                                                          ││
│  │            [ Unlock Archive ]                            ││
│  │                                                          ││
│  │  ─────────────── or ───────────────                      ││
│  │                                                          ││
│  │            [ 📷 Scan QR Code ]                           ││
│  │                                                          ││
│  └─────────────────────────────────────────────────────────┘│
│                                                              │
│  ℹ️ Don't have the password? Contact the archive owner.      │
│                                                              │
└─────────────────────────────────────────────────────────────┘

           ↓ (after successful authentication)

┌─────────────────────────────────────────────────────────────┐
│  🔍 Search: [authentication bug fix____________] [🔎]       │
│                                                              │
│  Filters: [Claude Code ▼] [All Time ▼] [All Projects ▼]    │
│                                                              │
├──────────────────────────┬──────────────────────────────────┤
│ Results (47 matches)     │ Conversation Detail              │
│                          │                                  │
│ ┌──────────────────────┐ │ 📅 2024-12-15 14:32              │
│ │ Fix JWT validation   │ │ 🤖 Claude Code                   │
│ │ Claude • 2024-12-15  │ │ 📁 /projects/auth-service        │
│ │ Score: 9.2           │ │                                  │
│ └──────────────────────┘ │ ─────────────────────────────────│
│ ┌──────────────────────┐ │                                  │
│ │ OAuth flow debugging │ │ 👤 User:                         │
│ │ Codex • 2024-12-10   │ │ I'm getting an authentication    │
│ │ Score: 8.7           │ │ error when...                    │
│ └──────────────────────┘ │                                  │
│ ┌──────────────────────┐ │ 🤖 Assistant:                    │
│ │ Session management   │ │ Let me help debug this. First,   │
│ │ Gemini • 2024-12-08  │ │ let's check the JWT token...     │
│ │ Score: 8.1           │ │                                  │
│ └──────────────────────┘ │ ```javascript                    │
│                          │ const decoded = jwt.verify(...   │
│ [Load more...]           │ ```                              │
└──────────────────────────┴──────────────────────────────────┘
```

---

## 9. Technical Implementation Plan

### 9.1 Rust CLI Components

#### New Modules

```
src/
├── pages/
│   ├── mod.rs              # Module exports
│   ├── wizard.rs           # Interactive wizard (TUI-based)
│   ├── export.rs           # Database export with filters
│   ├── encrypt.rs          # Argon2id + AES-256-GCM
│   ├── bundle.rs           # Asset bundling
│   ├── deploy_github.rs    # GitHub Pages deployment
│   ├── deploy_cloudflare.rs # Cloudflare deployment
│   └── qr.rs               # QR code generation
├── pages_assets/           # Embedded web assets
│   ├── index.html
│   ├── viewer.js
│   ├── auth.js
│   ├── styles.css
│   └── vendor/
│       ├── sql-wasm.js
│       ├── sql-wasm.wasm
│       ├── argon2-wasm.js
│       ├── argon2-wasm.wasm
│       └── alpine.min.js
```

#### New Dependencies

```toml
# Cargo.toml additions
[dependencies]
argon2 = "0.5"              # Key derivation
aes-gcm = "0.10"            # Authenticated encryption
qrcode = "0.14"             # QR code generation
image = "0.25"              # Image processing for QR
dialoguer = "0.11"          # Interactive prompts
indicatif = "0.17"          # Progress bars
include_dir = "0.7"         # Asset embedding
```

### 9.2 Database Export Schema

**Learned from bv:** Use FTS5 with Porter stemmer for natural language, plus a separate FTS for code/path search.

```sql
-- Filtered export database schema
CREATE TABLE conversations (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    workspace TEXT,
    title TEXT,
    source_path TEXT NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    message_count INTEGER,
    metadata_json TEXT
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER,
    attachment_refs TEXT,  -- JSON array of blob hashes: ["sha256-abc...", "sha256-def..."]
    FOREIGN KEY (conversation_id) REFERENCES conversations(id)
);

-- Optional: Attachment metadata (only present if --include-attachments)
CREATE TABLE IF NOT EXISTS attachments (
    hash TEXT PRIMARY KEY,      -- sha256 of plaintext content
    filename TEXT NOT NULL,     -- original filename
    mime_type TEXT NOT NULL,    -- e.g., "image/png", "text/plain"
    size_bytes INTEGER NOT NULL,
    message_id INTEGER,         -- which message referenced this
    created_at INTEGER,
    FOREIGN KEY (message_id) REFERENCES messages(id)
);

-- ═══════════════════════════════════════════════════════════════════════════
-- DUAL FTS STRATEGY: Natural Language vs Code/Path Search
-- ═══════════════════════════════════════════════════════════════════════════

-- FTS5 Index #1: Natural Language Search (porter stemmer)
-- - "running" matches "run", "runs", "runner"
-- - Good for: English prose, documentation, explanations
-- NOTE: Use ONE tokenizer per FTS table (not both porter AND unicode61)
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id',
    tokenize='porter'
);

-- FTS5 Index #2: Code/Path Search (unicode61 tokenchars)
-- - Preserves snake_case, camelCase, file.extensions as searchable tokens
-- - "my_function" is a single token (not split on underscore)
-- - "AuthController.ts" matches exact filename
-- - Good for: function names, paths, identifiers, error messages
CREATE VIRTUAL TABLE messages_code_fts USING fts5(
    content,
    content='messages',
    content_rowid='id',
    tokenize="unicode61 tokenchars '_./\\'"
);

-- OPTIONAL: FTS5 Trigram Index for LIKE-style substring matching
-- Uncomment if users need arbitrary substring search (e.g., "foo" matches "foobar")
-- Note: Significantly increases index size (~3x content size)
-- CREATE VIRTUAL TABLE messages_trigram USING fts5(
--     content,
--     content='messages',
--     content_rowid='id',
--     tokenize='trigram'
-- );

-- NOTE: Triggers are NOT needed for static export databases.
-- FTS5 content tables are populated via INSERT during export.
-- The exported database is read-only in the browser.
-- These triggers would only be needed if the database were modified client-side.

-- ═══════════════════════════════════════════════════════════════════════════

-- Indexes for common query patterns
CREATE INDEX idx_messages_conversation ON messages(conversation_id);
CREATE INDEX idx_messages_role ON messages(role);
CREATE INDEX idx_conversations_agent ON conversations(agent);
CREATE INDEX idx_conversations_workspace ON conversations(workspace);
CREATE INDEX idx_conversations_started ON conversations(started_at);

-- Metadata
CREATE TABLE export_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);

INSERT INTO export_meta (key, value) VALUES
    ('schema_version', '1'),
    ('exported_at', datetime('now')),
    ('cass_version', '0.1.48'),
    ('agents', '["claude-code","codex","gemini"]'),
    ('time_range', '{"from":null,"to":null}'),
    ('encryption', 'aes-256-gcm'),
    ('kdf', 'argon2id');
```

#### FTS5 Query Escaping

FTS5 has special characters that must be escaped to prevent syntax errors or injection:

```javascript
// Escape special FTS5 characters for safe queries
function escapeFts5Query(query) {
    // FTS5 special chars: " * ^ - : ( ) AND OR NOT NEAR
    // For simple search: wrap each term in double-quotes
    return query
        .split(/\s+/)
        .filter(term => term.length > 0)
        .map(term => {
            // Escape embedded double-quotes by doubling them
            const escaped = term.replace(/"/g, '""');
            return `"${escaped}"`;
        })
        .join(' ');
}

// For prefix search (e.g., autocomplete), append *
function escapeFts5Prefix(query) {
    const terms = query.split(/\s+/).filter(t => t.length > 0);
    if (terms.length === 0) return '';
    const lastTerm = terms.pop();
    const escaped = terms.map(t => `"${t.replace(/"/g, '""')}"`);
    escaped.push(`"${lastTerm.replace(/"/g, '""')}"*`);
    return escaped.join(' ');
}
```

#### Choosing Which FTS to Query

```javascript
// In viewer.js - route queries to appropriate FTS
function searchMessages(rawQuery, searchMode = 'auto') {
    // Auto-detect: if query looks like code (has underscores, dots, camelCase)
    const isCodeQuery = /[_.]|[a-z][A-Z]/.test(rawQuery);

    // CRITICAL: Escape the query to prevent FTS5 syntax errors
    const query = escapeFts5Query(rawQuery);

    // Use snippet() for query-aware context extraction (FTS5 built-in)
    // snippet(table, column_idx, open_tag, close_tag, ellipsis, max_tokens)
    if (searchMode === 'code' || (searchMode === 'auto' && isCodeQuery)) {
        return db.exec(`
            SELECT m.*,
                   bm25(messages_code_fts) AS score,
                   snippet(messages_code_fts, 0, '<mark>', '</mark>', '…', 64) AS snippet
            FROM messages_code_fts
            JOIN messages m ON messages_code_fts.rowid = m.id
            WHERE messages_code_fts MATCH ?
            ORDER BY score
            LIMIT 100
        `, [query]);
    } else {
        return db.exec(`
            SELECT m.*,
                   bm25(messages_fts) AS score,
                   snippet(messages_fts, 0, '<mark>', '</mark>', '…', 64) AS snippet
            FROM messages_fts
            JOIN messages m ON messages_fts.rowid = m.id
            WHERE messages_fts MATCH ?
            ORDER BY score
            LIMIT 100
        `, [query]);
    }
}
```

### 9.2.1 Pre-Computed Data Files (Learned from bv)

bv pre-computes expensive analytics server-side to avoid client-side computation. We adopt this pattern:

```
data/
├── statistics.json        # Pre-computed dashboard metrics
├── agent_summary.json     # Per-agent statistics
├── workspace_summary.json # Per-workspace breakdown
├── timeline.json          # Message counts by day/week/month
└── top_terms.json         # Most frequent search terms/topics
```

#### statistics.json

```json
{
    "total_conversations": 2035,
    "total_messages": 63701,
    "agents": {
        "claude-code": { "conversations": 1234, "messages": 45678 },
        "codex": { "conversations": 567, "messages": 12345 },
        "gemini": { "conversations": 234, "messages": 5678 }
    },
    "time_range": {
        "earliest": "2023-06-15T00:00:00Z",
        "latest": "2025-01-06T23:59:59Z"
    },
    "message_roles": {
        "user": 31234,
        "assistant": 32467
    },
    "computed_at": "2025-01-06T12:34:56Z"
}
```

#### timeline.json (for sparkline charts)

```json
{
    "daily": [
        { "date": "2025-01-01", "messages": 156, "conversations": 12 },
        { "date": "2025-01-02", "messages": 203, "conversations": 18 }
    ],
    "weekly": [...],
    "monthly": [...]
}
```

**Why pre-compute?**
- Instant dashboard rendering (no SQL aggregation on load)
- Reduces sql.js memory pressure
- Enables rich visualizations without client computation
- Pre-computed data is encrypted alongside the database

### 9.2.2 Materialized Views for Search Performance

For large archives, create materialized views that accelerate common queries:

```sql
-- Materialized view: Recent conversations per agent
-- NOTE: Window function results can't be used in WHERE of the same SELECT,
-- so we use a subquery pattern.
CREATE TABLE mv_recent_by_agent AS
SELECT agent, conversation_id, title, started_at, message_count, rank
FROM (
    SELECT
        agent,
        id AS conversation_id,
        title,
        started_at,
        message_count,
        ROW_NUMBER() OVER (PARTITION BY agent ORDER BY started_at DESC) as rank
    FROM conversations
)
WHERE rank <= 50;

CREATE INDEX idx_mv_recent_agent ON mv_recent_by_agent(agent, rank);

-- Materialized view: Search result snippets
-- Pre-extract the first 200 chars of each message for fast preview
CREATE TABLE mv_message_snippets AS
SELECT
    id,
    conversation_id,
    role,
    SUBSTR(content, 1, 200) AS snippet,
    LENGTH(content) AS full_length
FROM messages;

CREATE INDEX idx_mv_snippets_conv ON mv_message_snippets(conversation_id);
```

**Trade-off**: Increases database size by ~10-15% but dramatically improves search result rendering speed.

### 9.3 Encryption Implementation (Envelope Encryption, Key Slots, Chunked AEAD)

#### CLI Encryption Pipeline MUST be Streaming (Production Requirement)

To support archives approaching GitHub Pages limits without excessive memory usage, the CLI MUST
stream: SQLite → compress → chunk → encrypt → write.

Key properties:
- O(1) memory with respect to archive size (bounded by a few buffers)
- Chunks are boundaries in the *compressed stream* (decompress remains streaming in browser)
- `config.json` is written last (after chunk_count is known)

```rust
// src/pages/encrypt.rs — streaming envelope encryption + chunked AEAD
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use argon2::{Argon2, Params, Version};
use rand::RngCore;
use zeroize::Zeroize;
use std::io::{Read, Write};

/// A single key slot (password or recovery secret)
pub struct KeySlot {
    pub id: u32,
    pub slot_type: String,    // "password" or "recovery"
    pub kdf: String,          // "argon2id" or "hkdf-sha256"
    pub kdf_params: Option<KdfParams>, // Only for argon2id
    pub salt: [u8; 16],       // per-slot
    pub nonce: [u8; 12],      // per-slot (for DEK wrapping)
    pub wrapped_dek: Vec<u8>, // 32B DEK + 16B tag (AES-GCM output)
}

/// Envelope encryption configuration (written to config.json)
pub struct EnvelopeConfig {
    pub export_id: [u8; 16],     // random per-export; used as AAD binding
    pub base_nonce: [u8; 12],    // base nonce for chunk encryption (counter-based)
    pub kdf_policy: KdfPolicy,
    pub compression: String,     // "deflate" | "zstd" | "none"
    pub key_slots: Vec<KeySlot>,
    pub chunk_count: u32,
    pub chunk_size: u32,
}

/// Encrypt a plaintext SQLite file by streaming compression → chunked AEAD.
/// Writes chunks directly to `site/payload/` and returns an EnvelopeConfig to be serialized.
pub fn encrypt_export_sqlite_streaming<R: Read>(
    mut sqlite_plaintext: R,
    chunk_size: usize,
    out_payload_dir: &std::path::Path,
    kek_inputs: Vec<(String /*slot_type*/, SecretBytes /*secret*/)>,
    kdf_policy: &KdfPolicy,
) -> Result<EnvelopeConfig, EncryptError> {
    // 1) Generate random DEK, export_id, and base_nonce
    let mut export_id = [0u8; 16];
    let mut dek = [0u8; 32];
    let mut base_nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut export_id);
    rand::thread_rng().fill_bytes(&mut dek);
    rand::thread_rng().fill_bytes(&mut base_nonce);

    // 2) Stream: plaintext → compressor → chunk buffer → AEAD encrypt → write chunk files
    let payload_cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
    std::fs::create_dir_all(out_payload_dir)?;

    // Streaming compressor (deflate shown; zstd would be similar)
    let mut compressor = flate2::read::DeflateEncoder::new(
        sqlite_plaintext,
        flate2::Compression::default(),
    );

    let mut chunk_index: u32 = 0;
    let mut buf = vec![0u8; 128 * 1024];
    let mut chunk = Vec::with_capacity(chunk_size);

    loop {
        let n = compressor.read(&mut buf)?;
        if n == 0 { break; }
        chunk.extend_from_slice(&buf[..n]);

        while chunk.len() >= chunk_size {
            let plaintext_part: Vec<u8> = chunk.drain(..chunk_size).collect();
            let nonce = derive_chunk_nonce(&base_nonce, chunk_index);
            let aad = build_chunk_aad(&export_id, chunk_index, 2);
            let ciphertext = payload_cipher.encrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: &plaintext_part, aad: &aad },
            )?;
            write_chunk_file(out_payload_dir, chunk_index, &ciphertext)?;
            chunk_index += 1;
        }
    }

    // Final partial chunk (if any)
    if !chunk.is_empty() {
        let nonce = derive_chunk_nonce(&base_nonce, chunk_index);
        let aad = build_chunk_aad(&export_id, chunk_index, 2);
        let ciphertext = payload_cipher.encrypt(
            Nonce::from_slice(&nonce),
            Payload { msg: &chunk, aad: &aad },
        )?;
        write_chunk_file(out_payload_dir, chunk_index, &ciphertext)?;
        chunk_index += 1;
    }

    // 3) For each key slot: derive KEK and wrap DEK
    let mut key_slots = Vec::new();
    for (i, (slot_type, secret)) in kek_inputs.into_iter().enumerate() {
        let mut salt = [0u8; 16];
        let mut wrap_nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut wrap_nonce);

        // Choose KDF based on slot type
        let (kdf_name, kdf_params, mut kek) = if slot_type == "recovery" {
            // Recovery secrets are high-entropy; use fast HKDF-SHA256
            let kek = derive_kek_hkdf(&secret.0, &salt)?;
            ("hkdf-sha256".to_string(), None, kek)
        } else {
            // Passwords need memory-hard KDF
            let kek = derive_kek_argon2id(&secret.0, &salt, kdf_policy)?;
            ("argon2id".to_string(), Some(kdf_policy.argon2id_params.clone()), kek)
        };

        let wrap_cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&kek));

        // AAD for wrapping = export_id || slot_id
        let wrap_aad = build_slot_aad(&export_id, i as u32);
        let wrapped_dek = wrap_cipher.encrypt(
            Nonce::from_slice(&wrap_nonce),
            Payload { msg: &dek, aad: &wrap_aad },
        )?;

        kek.zeroize(); // Clear KEK from memory

        key_slots.push(KeySlot {
            id: i as u32,
            slot_type,
            kdf: kdf_name,
            kdf_params,
            salt,
            nonce: wrap_nonce,
            wrapped_dek,
        });
    }

    // 4) Zeroize DEK in memory
    dek.zeroize();

    Ok(EnvelopeConfig {
        export_id,
        base_nonce,
        kdf_policy: kdf_policy.clone(),
        compression: "deflate".to_string(),
        key_slots,
        chunk_count: chunk_index,
        chunk_size: chunk_size as u32,
    })
}
```

**Cargo.toml additions:**
```toml
[dependencies]
argon2 = "0.5"
aes-gcm = "0.10"
zeroize = "1.7"              # Secure memory clearing
flate2 = "1.0"               # Deflate compression
```

### 9.4 Browser Decryption (Worker-based, Unwrap DEK + Stream Decrypt)

All expensive operations run in a dedicated Web Worker for UI responsiveness:

```
┌─────────────────────────────────────────────────────────────┐
│                    Worker Architecture                       │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  main thread:                                                │
│    - Auth UI (password/QR input)                            │
│    - Progress display                                        │
│    - Rendering (search, conversations)                      │
│                                                              │
│  crypto_worker.js:                                          │
│    - Argon2id key derivation                                │
│    - DEK unwrapping (try each key slot)                     │
│    - Chunk download + AEAD decrypt                          │
│    - Streaming decompression (fflate)                       │
│    - OPFS write (if opted-in)                               │
│    - sqlite-wasm initialization                             │
│                                                              │
╰─────────────────────────────────────────────────────────────╯
```

#### Step 1: Unwrap DEK from Key Slots

```javascript
// crypto_worker.js — runs in Web Worker
async function unlockDEK(secret, config) {
  const argon2 = await loadArgon2();
  const exportIdBytes = base64ToBytes(config.export_id);

  for (const slot of config.key_slots) {
    // Derive KEK using Argon2id
    const kek = await argon2.hash({
      pass: secret,
      salt: base64ToBytes(slot.salt),
      time: config.kdf_params.iterations,
      mem:  config.kdf_params.memory_kb,
      parallelism: config.kdf_params.parallelism,
      hashLen: 32,
      type: argon2.ArgonType.Argon2id,
    });

    try {
      // Build AAD for unwrapping: export_id || slot_id
      const unwrapAad = buildSlotAad(exportIdBytes, slot.id);
      const kekKey = await crypto.subtle.importKey(
        'raw', kek.hash, { name: 'AES-GCM' }, false, ['decrypt']
      );
      const dekBuf = await crypto.subtle.decrypt(
        { name: 'AES-GCM', iv: base64ToBytes(slot.nonce), additionalData: unwrapAad },
        kekKey,
        base64ToBytes(slot.wrapped_dek)
      );
      return new Uint8Array(dekBuf); // 32 bytes DEK
    } catch (_) {
      // Auth tag mismatch → try next slot
      continue;
    }
  }
  throw new Error('Invalid password / recovery secret');
}
```

#### Step 2: Stream Decrypt Chunks → Decompress → Write OPFS

```javascript
// crypto_worker.js — streaming decrypt + decompress + OPFS write
async function downloadDecryptToOPFS(config, dekBytes, onProgress, abortSignal) {
  const chunkFiles = config.payload.files;
  const total = chunkFiles.length;
  const exportIdBytes = base64ToBytes(config.export_id);

  // Open OPFS file for writing
  const writer = await openOpfsWritable('decrypted.sqlite3');

  // Initialize streaming decompressor (fflate)
  const { Inflate } = await import('./vendor/fflate.min.js');
  const inflater = new Inflate((chunk, final) => {
    writer.write(chunk);
    if (final) writer.close();
  });

  // Import DEK for chunk decryption
  const dekKey = await crypto.subtle.importKey(
    'raw', dekBytes, { name: 'AES-GCM' }, false, ['decrypt']
  );

  for (let i = 0; i < total; i++) {
    if (abortSignal?.aborted) throw new Error('Cancelled');

    // Fetch encrypted chunk
    const response = await fetch(chunkFiles[i], { signal: abortSignal });
    const encryptedChunk = new Uint8Array(await response.arrayBuffer());

    // Derive per-chunk nonce and AAD
    const chunkNonce = deriveChunkNonce(config.base_nonce, i);
    const chunkAad = buildChunkAad(exportIdBytes, i, config.version);

    // Decrypt chunk (AEAD verifies integrity)
    const compressedChunk = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: chunkNonce, additionalData: chunkAad },
      dekKey,
      encryptedChunk
    );

    // Feed to streaming decompressor
    inflater.push(new Uint8Array(compressedChunk), i === total - 1);

    onProgress((i + 1) / total);
  }
}
```

#### Step 3: Initialize SQLite from OPFS

```javascript
// crypto_worker.js — open database from OPFS
async function initializeDatabaseFromOPFS() {
  // Load sqlite-wasm (official SQLite build with OPFS VFS)
  const sqlite3 = await loadSqliteWasm();

  // Open DB stored in OPFS (written during decrypt pipeline)
  const db = await sqlite3.oo1.OpfsDb('decrypted.sqlite3');

  // Verify schema version
  const version = db.selectValue("SELECT value FROM export_meta WHERE key='schema_version'");
  if (version !== '2') {
    throw new Error('Incompatible archive version');
  }

  return db;
}
```

### 9.5 Multi-Tier Database Loading (Streamable Chunked AEAD)

We use a streaming architecture that combines encryption, decompression, and persistence in a single pass:

```
┌─────────────────────────────────────────────────────────────┐
│           Database Loading Strategy (Chunked AEAD)           │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Tier 1: OPFS Cache Check (OPT-IN only)                     │
│  ───────────────────────────────────────                    │
│  • Only checked if user enabled "Remember on this device"   │
│  • Verify fingerprint matches config.json export_id         │
│  • If valid: open sqlite-wasm directly from OPFS (<50ms)    │
│  • Includes "Clear cached data" button in UI                │
│  • Default: SKIP (memory-only for max security)             │
│                                                              │
│                          ↓ (no cache or user chose fresh)   │
│                                                              │
│  Tier 2: Stream Decrypt → Decompress → Write (ALWAYS)       │
│  ─────────────────────────────────────────────────          │
│  • All exports use chunked AEAD (8 MiB chunks default)      │
│  • Fetch chunk → AEAD decrypt (auth via export_id AAD)      │
│  • Stream into fflate decompressor                          │
│  • Write plaintext to OPFS (if opted-in) or memory          │
│  • Bounded memory: only 1-2 chunks in flight                │
│  • Progress: (chunks_done / total_chunks) × 100             │
│                                                              │
│                          ↓ (all chunks processed)           │
│                                                              │
│  Tier 3: Initialize sqlite-wasm                             │
│  ─────────────────────────────                              │
│  • Open database from OPFS or memory buffer                 │
│  • Verify schema_version matches expected                   │
│  • Ready for queries                                        │
│                                                              │
╰─────────────────────────────────────────────────────────────╯
```

**Key differences from bv's approach:**
- Chunks are authenticated via AEAD (auth tag), not separate SHA256 hashes
- Decompression is streaming (fflate), not post-hoc
- OPFS persistence is OPT-IN (security-first default)
- AAD binding (export_id) prevents chunk substitution attacks

#### OPFS Implementation (Opt-In with Clear Cache)

```javascript
// OPFS persistence is OPT-IN for security
// User must explicitly check "Remember on this device" to enable
const OPFS_DIR = 'cass-cache';
const DB_FILENAME = 'decrypted.sqlite3';
const META_FILENAME = 'cache-meta.json';

// Check if user has opted into OPFS persistence
function isOpfsPersistenceEnabled() {
    return localStorage.getItem('cass-opfs-enabled') === 'true';
}

// UI: checkbox "Remember on this device (stores decrypted data locally)"
function setOpfsPersistence(enabled) {
    if (enabled) {
        localStorage.setItem('cass-opfs-enabled', 'true');
    } else {
        localStorage.removeItem('cass-opfs-enabled');
        clearOpfsCache(); // Clear immediately when disabled
    }
}

async function getOpfsRoot() {
    if (!navigator.storage?.getDirectory) {
        return null; // OPFS not supported
    }
    try {
        const root = await navigator.storage.getDirectory();
        return await root.getDirectoryHandle(OPFS_DIR, { create: true });
    } catch (e) {
        console.warn('OPFS unavailable:', e);
        return null;
    }
}

async function loadFromOpfsCache(expectedExportId) {
    // Only check cache if user opted in
    if (!isOpfsPersistenceEnabled()) return null;

    const dir = await getOpfsRoot();
    if (!dir) return null;

    try {
        // Read cache metadata to verify export_id matches
        const metaHandle = await dir.getFileHandle(META_FILENAME);
        const metaFile = await metaHandle.getFile();
        const meta = JSON.parse(await metaFile.text());

        if (meta.export_id !== expectedExportId) {
            console.log('OPFS cache export_id mismatch, will decrypt fresh');
            return null;
        }

        // Cache is valid - database can be opened directly from OPFS
        return { cached: true, exportId: meta.export_id };
    } catch (e) {
        return null; // Cache miss
    }
}

async function saveToOpfsCache(exportId) {
    // Only save if user opted in
    if (!isOpfsPersistenceEnabled()) return;

    const dir = await getOpfsRoot();
    if (!dir) return;

    try {
        // Database was already written during streaming decrypt
        // Just save metadata for future cache validation
        const metaHandle = await dir.getFileHandle(META_FILENAME, { create: true });
        const writable = await metaHandle.createWritable();
        await writable.write(JSON.stringify({
            export_id: exportId,
            cached_at: new Date().toISOString()
        }));
        await writable.close();
    } catch (e) {
        console.warn('Failed to save OPFS metadata:', e);
    }
}

// UI: "Clear cached data" button handler
async function clearOpfsCache() {
    const dir = await getOpfsRoot();
    if (!dir) return;

    try {
        await dir.removeEntry(DB_FILENAME);
        await dir.removeEntry(META_FILENAME);
        console.log('OPFS cache cleared');
    } catch (e) {
        // Files may not exist, that's OK
    }
}
```

#### Streaming Decrypt Pipeline (replaces old chunked download)

The chunked download is now integrated into the streaming decrypt pipeline (see Section 9.4). The config.json chunk manifest format:

```javascript
// config.json payload section (NEW format - AEAD authenticated chunks)
// {
//   "export_id": "base64-16-bytes",
//   "base_nonce": "base64-12-bytes",
//   "compression": "deflate",
//   "payload": {
//     "chunk_size": 8388608,  // 8 MiB default
//     "chunk_count": 4,
//     "files": ["payload.0.bin", "payload.1.bin", "payload.2.bin", "payload.3.bin"]
//   }
// }
// NOTE: No chunk_hashes array - each chunk is authenticated via AEAD tag

// NOTE: Chunk download and verification is now integrated into the
// streaming decrypt pipeline (downloadDecryptToOPFS in Section 9.4).
// Each chunk is verified via AEAD auth tag, not separate SHA256 hashes.
// Concurrency is still limited (1-2 chunks in flight) for bounded memory.
```

#### Browser Compatibility for OPFS

| Browser | OPFS Support | Notes |
|---------|--------------|-------|
| Chrome 102+ | ✅ Full | Recommended |
| Edge 102+ | ✅ Full | Chromium-based |
| Firefox 111+ | ✅ Full | Since March 2023 |
| Safari 15.2+ | ⚠️ Partial | No `createWritable()` |
| Mobile Chrome | ✅ Full | Android 102+ |
| Mobile Safari | ⚠️ Limited | iOS 15.2+, limited quota |

**Fallback**: When OPFS is unavailable, the decrypted database is held in memory only. Users will need to re-enter their password on page refresh.

### 9.6 WASM Memory Management (Learned from bv)

bv uses a careful WASM memory management pattern to prevent memory leaks when working with sql.js. We adopt this:

#### The Problem

sql.js allocates memory in the WASM heap that JavaScript's garbage collector cannot see. Prepared statements, result sets, and intermediate data must be explicitly freed.

#### The Solution: Scoped Resource Pattern

```javascript
/**
 * Execute a database operation with automatic resource cleanup.
 * Inspired by bv's withSubgraph() pattern.
 *
 * @param {SQL.Database} db - The sql.js database instance
 * @param {Function} operation - Function receiving (db) => result
 * @returns {any} - Result of the operation
 */
function withDatabaseScope(db, operation) {
    const statements = [];

    // Proxy to track prepared statements
    const trackedDb = {
        prepare: (sql) => {
            const stmt = db.prepare(sql);
            statements.push(stmt);
            return stmt;
        },
        exec: (sql) => db.exec(sql),
        run: (sql, params) => db.run(sql, params),
        // ... other methods pass through
    };

    try {
        return operation(trackedDb);
    } finally {
        // Free all tracked statements
        for (const stmt of statements) {
            try { stmt.free(); } catch (e) { /* ignore */ }
        }
    }
}

// Usage example
function searchMessages(db, query, limit = 50) {
    return withDatabaseScope(db, (scopedDb) => {
        const stmt = scopedDb.prepare(`
            SELECT m.id, m.content, m.role, c.title, c.agent
            FROM messages_fts
            JOIN messages m ON messages_fts.rowid = m.id
            JOIN conversations c ON m.conversation_id = c.id
            WHERE messages_fts MATCH ?
            ORDER BY rank
            LIMIT ?
        `);

        stmt.bind([query, limit]);

        const results = [];
        while (stmt.step()) {
            results.push(stmt.getAsObject());
        }

        return results;
        // stmt.free() called automatically when scope exits
    });
}
```

#### Hybrid WASM Scorer Pattern (from bv)

bv implements a hybrid approach where complex scoring runs in Rust/WASM for large datasets but falls back to JS for smaller ones:

```javascript
// Threshold for when WASM scoring provides benefit
const WASM_SCORER_THRESHOLD = 5000;

async function scoreResults(results, scorerWasm) {
    if (results.length < WASM_SCORER_THRESHOLD) {
        // JS scoring is faster for small datasets (no WASM call overhead)
        return results.map(r => ({
            ...r,
            score: computeScoreJS(r)
        }));
    }

    // For large datasets, WASM scoring is significantly faster
    // Pack data into typed array for efficient WASM transfer
    const packedData = packResultsForWasm(results);

    // Call WASM scorer (compiled from Rust)
    const scores = scorerWasm.score_batch(packedData);

    // Unpack and merge
    return results.map((r, i) => ({
        ...r,
        score: scores[i]
    }));
}

function computeScoreJS(result) {
    // Simple BM25-ish scoring in JS
    const tf = result.matches / result.content_length;
    const idf = Math.log(1 + result.total_docs / result.doc_freq);
    return tf * idf;
}
```

#### Memory Budget Monitoring

```javascript
// Monitor WASM memory usage
function getWasmMemoryUsage() {
    // sql.js exposes the underlying WASM module
    if (window.SQL?.Module?.HEAPU8) {
        const heap = window.SQL.Module.HEAPU8;
        return {
            used: heap.length,
            limit: 256 * 1024 * 1024, // Typical browser limit
            percentage: (heap.length / (256 * 1024 * 1024)) * 100
        };
    }
    return null;
}

// Warn if approaching memory limit
function checkMemoryPressure() {
    const usage = getWasmMemoryUsage();
    if (usage && usage.percentage > 80) {
        console.warn(`WASM memory at ${usage.percentage.toFixed(1)}% - consider reducing result limits`);
        return true;
    }
    return false;
}
```

### 9.7 Viewer Scaling: Virtualization & Deep Links

For archives with 100K+ messages, the viewer must efficiently render large result sets and support direct linking to specific content.

#### Virtual Scrolling for Large Result Sets

```javascript
// Use a virtual list for search results (only render visible items)
import { VirtualList } from './virtual-list.js';

const ITEM_HEIGHT = 80; // px per search result row
const BUFFER_ITEMS = 5; // extra items above/below viewport

class VirtualSearchResults {
    constructor(container) {
        this.container = container;
        this.allResults = [];
        this.virtualList = new VirtualList({
            container,
            itemHeight: ITEM_HEIGHT,
            buffer: BUFFER_ITEMS,
            renderItem: (item, index) => this.renderResultRow(item, index)
        });
    }

    setResults(results) {
        this.allResults = results;
        this.virtualList.setItems(results);
    }

    renderResultRow(result, index) {
        // Only called for visible items
        return `
            <div class="result-row" data-id="${result.id}">
                <div class="result-title">${escapeHtml(result.title)}</div>
                <div class="result-meta">${result.agent} • ${formatDate(result.created_at)}</div>
                <div class="result-snippet">${highlightMatches(result.snippet)}</div>
            </div>
        `;
    }
}
```

#### Deep Links with Hash-Based Routing

Support direct links to specific conversations and messages:

```
https://user.github.io/archive/#/c/12345          → conversation 12345
https://user.github.io/archive/#/c/12345/m/67    → message 67 in conversation 12345
https://user.github.io/archive/#/search/auth+bug → search for "auth bug"
```

```javascript
// Hash-based router (works without server-side config)
class ArchiveRouter {
    constructor(app) {
        this.app = app;
        window.addEventListener('hashchange', () => this.route());
        this.route(); // Handle initial load
    }

    route() {
        const hash = window.location.hash.slice(1); // Remove leading #
        const parts = hash.split('/').filter(Boolean);

        if (parts[0] === 'c' && parts[1]) {
            const convId = parseInt(parts[1], 10);
            const msgId = parts[2] === 'm' ? parseInt(parts[3], 10) : null;
            this.app.openConversation(convId, msgId);
        } else if (parts[0] === 'search' && parts[1]) {
            const query = decodeURIComponent(parts[1]);
            this.app.search(query);
        } else {
            this.app.showHome();
        }
    }

    navigate(path) {
        window.location.hash = path;
    }
}

// Generate shareable links
function getShareLink(conversationId, messageId = null) {
    const base = window.location.href.split('#')[0];
    const path = messageId
        ? `/c/${conversationId}/m/${messageId}`
        : `/c/${conversationId}`;
    return `${base}#${path}`;
}
```

#### Lazy Conversation Loading

Don't load full conversation content until needed:

```javascript
// Conversation list shows only metadata (fast)
async function loadConversationList() {
    return db.exec(`
        SELECT id, title, agent, started_at, message_count
        FROM conversations
        ORDER BY started_at DESC
        LIMIT 1000
    `);
}

// Full messages loaded only when viewing (on-demand)
async function loadConversationMessages(convId) {
    return db.exec(`
        SELECT id, role, content, created_at
        FROM messages
        WHERE conversation_id = ?
        ORDER BY idx ASC
    `, [convId]);
}
```

---

## 10. File Structure & Bundle Contents

### Generated Bundle (Split Output)

**CRITICAL:** Export produces two directories to prevent accidental secret exposure:

```
cass-pages-export/
├── site/                   # ← DEPLOY THIS (safe for public hosting)
│   ├── index.html          # Entry point (auth UI + app shell)
│   ├── .nojekyll           # Disable Jekyll processing on GitHub Pages
│   ├── robots.txt          # Disallow crawling (auth page is still public)
│   ├── config.json         # Salt, nonce, key slots (NOT secrets!)
│   ├── integrity.json      # Hash manifest for all public files (anti-tamper aid)
│   ├── payload/            # Chunked AEAD ciphertext (ALWAYS used)
│   │   ├── chunk-00000.bin
│   │   ├── chunk-00001.bin
│   │   └── ...
│   ├── blobs/              # Optional: encrypted attachment blobs (--include-attachments)
│   │   ├── sha256-abc123.bin
│   │   └── ...
│   ├── sw.js               # COI service worker
│   ├── viewer.js           # Main application logic
│   ├── auth.js             # Authentication module
│   ├── search.js           # Search UI components
│   ├── conversation.js     # Conversation renderer
│   ├── styles.css          # Tailwind-based styles
│   ├── vendor/
│   │   ├── sqlite3.js      # Official sqlite-wasm loader
│   │   ├── sqlite3.wasm    # SQLite WASM binary
│   │   ├── sqlite3-opfs.js # OPFS worker helper
│   │   ├── argon2-wasm.js  # Argon2 WASM loader
│   │   ├── argon2-wasm.wasm # Argon2 WASM binary
│   │   ├── fflate.min.js   # Streaming decompression
│   │   ├── marked.min.js   # Markdown rendering
│   │   └── prism.min.js    # Syntax highlighting
│   ├── assets/
│   │   ├── logo.svg        # cass logo
│   │   └── icons.svg       # UI icons
│   └── README.md           # Archive description (no secrets)
│
└── private/                # ← NEVER DEPLOY (keep offline/secure)
    ├── recovery-secret.txt # High-entropy recovery passphrase
    ├── qr-code.png         # QR-encoded recovery secret
    ├── integrity-fingerprint.txt # Verify site integrity out-of-band
    └── master-key.json     # Optional: encrypted DEK backup
```

### Why Two Directories?

| Directory | Contents | Who Sees It |
|-----------|----------|-------------|
| `site/` | Encrypted archive + viewer code | Public (anyone with URL) |
| `private/` | Recovery secrets, QR code, key backup | Only you (offline storage) |

**Deployment copies ONLY `site/`** to GitHub Pages. The `private/` directory should be stored securely (password manager, encrypted USB, safe deposit box for critical archives).

### config.json (Public) — Envelope Encryption + Payload Manifest (Single Source of Truth)

```json
{
    "version": 2,
    "export_id": "base64-16-bytes",
    "algorithm": "aes-256-gcm",
    "base_nonce": "base64-12-bytes",
    "compression": "deflate",
    "kdf_defaults": {
        "argon2id": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
    },
    "payload": {
        "chunk_size": 8388608,
        "chunk_count": 4,
        "files": [
            "payload/chunk-00000.bin",
            "payload/chunk-00001.bin",
            "payload/chunk-00002.bin",
            "payload/chunk-00003.bin"
        ]
    },
    "key_slots": [
        {
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "kdf_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
            "salt": "base64-encoded-16-bytes",
            "nonce": "base64-encoded-12-bytes",
            "wrapped_dek": "base64-encoded-48-bytes"
        },
        {
            "id": 1,
            "slot_type": "recovery",
            "kdf": "hkdf-sha256",
            "salt": "base64-encoded-16-bytes",
            "nonce": "base64-encoded-12-bytes",
            "wrapped_dek": "base64-encoded-48-bytes"
        }
    ],
    "exported_at": "2025-01-06T12:34:56Z",
    "cass_version": "0.2.0"
}
```

**Security notes**:
- This file is intentionally public. It contains only public parameters, not secrets.
- `export_id` and `base_nonce` are unique per export; used as AAD binding.
- `wrapped_dek` is the encrypted DEK—cannot be decrypted without the correct password/recovery secret.
- Key slots are self-describing: each has its own `slot_type`, `kdf`, and optional `kdf_params`.
- **No human labels in public config**: slot labels (if any) live in encrypted metadata to avoid PII leakage.
- Password slots use Argon2id (memory-hard); recovery slots use HKDF-SHA256 (fast for high-entropy secrets).

The actual DEK is only recoverable by deriving the KEK from password/secret + salt, then unwrapping the DEK.

---

## 11. Frontend Technology Stack

### Required Libraries (Updated for Chunked AEAD Architecture)

| Library | Version | Uncompressed | Gzipped | Purpose |
|---------|---------|--------------|---------|---------|
| **sqlite-wasm** | 3.46+ | 850KB | 340KB | SQLite in browser (OPFS VFS, FTS5) — **ONLY RUNTIME** |
| **fflate** | 0.8+ | 29KB | 9KB | Streaming deflate decompression |
| **argon2-browser** | 1.18+ | 200KB | 78KB | Password hashing (WASM) |
| **UI layer** | (custom) | ~10–30KB | ~4–12KB | CSP-safe UI (no eval; ES modules; no inline handlers) |
| **Tailwind CSS** | 3.4+ | 398KB (full) | 50KB (JIT purged) | Utility-first CSS |
| **Marked.js** | 14.0+ | 48KB | 18KB | Markdown rendering |
| **Prism.js** | 1.29+ | 30KB | 11KB | Syntax highlighting |
| **DOMPurify** | 3.1+ | 20KB | 8KB | XSS sanitization |
| **html5-qrcode** | 2.3+ | 156KB | 52KB | QR code scanning |

**SQLite Runtime Selection:**
- **sqlite-wasm** is the only runtime (sql.js removed to eliminate duplicate APIs and bundle weight).
- When OPFS is unavailable, use sqlite-wasm in-memory mode (deserialize DB bytes into the WASM heap).
- This keeps the query API and memory model consistent across browsers.

**Note:** Removing sql.js saves bundle size and eliminates a second DB API surface.

### Optional Libraries (Feature-Dependent)

| Library | Version | Size | When Needed |
|---------|---------|------|-------------|
| **D3.js** | 7.9+ | 273KB (87KB gz) | For timeline/chart visualizations |
| **Force-Graph** | 1.43+ | 194KB (58KB gz) | For conversation relationship graphs |
| **Mermaid** | 10.9+ | 3.2MB (800KB gz) | For rendering diagrams in messages |

**Recommendation**: Start with core libraries only. Add D3/Force-Graph/Mermaid as opt-in features.

### Total Bundle Size Analysis

| Component | Uncompressed | Gzipped | Brotli |
|-----------|--------------|---------|--------|
| **Core JavaScript** | ~400KB | ~120KB | ~95KB |
| **sqlite-wasm** | 850KB | 340KB | 280KB |
| **Argon2 WASM** | 200KB | 78KB | 62KB |
| **fflate** | 29KB | 9KB | 7KB |
| **Alpine.js** | 44KB | 16KB | 13KB |
| **Tailwind CSS** | 50KB (purged) | 12KB | 10KB |
| **Vendor libs** | ~150KB | ~55KB | ~45KB |
| **Total (code only)** | **~1.7MB** | **~630KB** | **~512KB** |

**Note:** sql.js (640KB/290KB) is bundled as fallback but only loaded when sqlite-wasm OPFS is unavailable.

#### Size by User Journey

| Moment | What Loads | Gzipped Size |
|--------|------------|--------------|
| **Initial page** | index.html, auth.js, styles.css, Alpine | ~40KB |
| **Password entry** | Argon2 WASM (async) | +78KB |
| **After unlock** | sqlite-wasm, fflate, viewer.js, Marked, Prism | +460KB |
| **Encrypted data** | payload.*.bin chunks (varies) | Variable |

### Bundle Optimization Strategies (from bv)

#### 1. Code Splitting

```javascript
// Load heavy dependencies only when needed
async function loadSearchUI() {
    const { SearchModule } = await import('./search.js');
    const { marked } = await import('./vendor/marked.min.js');
    const { Prism } = await import('./vendor/prism.min.js');
    return new SearchModule(marked, Prism);
}
```

#### 2. WASM Loading Strategy

```javascript
// Parallel WASM initialization
const [argon2Ready, sqlReady] = await Promise.all([
    initArgon2(),  // Only needed for decryption
    initSqlJs(),   // Only needed after decryption
]);
```

#### 3. Critical CSS Inlining

```html
<!-- index.html - inline critical CSS for instant render -->
<style>
    /* Only auth page styles - 2KB */
    .auth-container { /* ... */ }
    .password-input { /* ... */ }
    .unlock-button { /* ... */ }
</style>
<!-- Load full styles async -->
<link rel="preload" href="styles.css" as="style" onload="this.rel='stylesheet'">
```

#### 4. Asset Preloading

```html
<!-- Preload critical resources -->
<link rel="preload" href="vendor/argon2-wasm.wasm" as="fetch" crossorigin>
<link rel="preload" href="vendor/sql-wasm.wasm" as="fetch" crossorigin>
<link rel="preload" href="encrypted.bin" as="fetch" crossorigin>
```

### Browser Compatibility

| Browser | Min Version | WASM | OPFS | Service Worker | Notes |
|---------|-------------|------|------|----------------|-------|
| Chrome | 102+ | ✅ | ✅ | ✅ | Full support |
| Firefox | 111+ | ✅ | ✅ | ✅ | Full support |
| Safari | 15.2+ | ✅ | ⚠️ | ✅ | OPFS limited |
| Edge | 102+ | ✅ | ✅ | ✅ | Chromium-based |
| Mobile Chrome | 102+ | ✅ | ✅ | ✅ | Android |
| Mobile Safari | 15.2+ | ✅ | ⚠️ | ✅ | iOS, OPFS limited |

**Hard Requirements**:
- WebAssembly with `wasm-unsafe-eval` CSP support
- Web Crypto API (SubtleCrypto)
- ES2020+ JavaScript (async/await, optional chaining)
- Fetch API with streaming support
- CSS Grid/Flexbox

**Soft Requirements** (graceful degradation):
- OPFS (fallback: memory-only)
- Service Workers (fallback: no offline)
- SharedArrayBuffer (fallback: single-threaded Argon2)

---

## 12. CLI Interface Design

### New Subcommand: `cass pages`

```
USAGE:
    cass pages [OPTIONS]
    cass pages --export-only <DIR>
    cass pages --preview <DIR>
    cass pages --verify <DIR>

DESCRIPTION:
    Export and deploy an encrypted, searchable web archive of your
    AI coding agent conversations.

OPTIONS:
    Content Selection:
        --agents <LIST>         Comma-separated agent slugs to include
                                [default: all]
        --workspaces <LIST>     Comma-separated workspace paths to include
                                [default: all]
        --since <DATE>          Only include conversations after this date
                                [format: YYYY-MM-DD or "30 days ago"]
        --until <DATE>          Only include conversations before this date
                                [format: YYYY-MM-DD or "today"]

    Privacy Controls:
        --path-mode <MODE>      How to store file paths in export:
                                  relative  - paths relative to workspace (default)
                                  basename  - filename only, no directory info
                                  full      - absolute paths (with warning)
                                  hash      - SHA256 of path (for stealth mode)
        --stealth               Alias for --path-mode hash; also strips
                                hostnames, usernames from all metadata

    Security:
        --password <PASS>       Encryption password (prompted if not provided)
        --password-file <FILE>  Read password from file
        --recovery-secret       Generate additional recovery key slot
        --no-encryption         Export without encryption (DANGEROUS)
        --generate-qr           Generate QR code for recovery secret
                                (saved to private/ - NEVER deployed)

    Site Configuration:
        --title <TEXT>          Site title [default: "cass Archive"]
        --description <TEXT>    Site description

    Deployment:
        --target <TARGET>       Deployment target: github, cloudflare, local
                                [default: github]
        --repo <NAME>           Repository name (GitHub/Cloudflare)
        --branch <BRANCH>       Git branch [default: gh-pages for GitHub]
        --private               Create private repository (requires paid plan)
        --base-path <PATH>      Base path for project pages (auto-detected)
                                e.g., /my-archive for user.github.io/my-archive

    Other:
        --export-only <DIR>     Export bundle without deploying
        --preview <DIR>         Start local preview server
        --verify <DIR>          Verify existing export (for CI pipelines)
        --dry-run               Show what would be exported, don't export
        --json                  Output progress as JSON (for automation)
        --yes                   Skip confirmation prompts (except safety)

EXAMPLES:
    # Interactive wizard (recommended)
    cass pages

    # Export Claude Code conversations from last 30 days
    cass pages --agents claude-code --since "30 days ago" \
               --title "Recent Claude Sessions"

    # Privacy-conscious export (no paths or usernames)
    cass pages --stealth --export-only ./my-export

    # Export specific project with recovery QR
    cass pages --workspaces /home/user/myproject \
               --recovery-secret --generate-qr --export-only ./my-export

    # Preview existing export locally
    cass pages --preview ./my-export

    # CI/CD verification (exits 0 if valid, non-zero otherwise)
    cass pages --verify ./my-export --json

    # Robot mode for CI/CD deployment
    cass pages --json --password-file /secrets/pw.txt \
               --target github --repo my-archive --branch gh-pages --yes

EXIT CODES:
    0   Success (or --verify passed)
    1   General error
    2   Invalid arguments
    3   Authentication required (--no-encryption without confirmation)
    4   Deployment failed
    5   User cancelled
    6   Verification failed (--verify mode)
```

### Verify Command Details

The `--verify` command checks an existing export for:
- All required files present (`index.html`, `encrypted.bin`, `config.json`, `sw.js`)
- config.json schema validity
- Encrypted blob has valid header magic (`CASS`)
- File sizes within GitHub Pages limits (100 MB per file)
- No secrets in site/ directory

```bash
# CI pipeline usage
cass pages --verify ./dist/site --json || exit 1
```

Output:
```json
{
    "status": "valid",
    "checks": {
        "required_files": true,
        "config_schema": true,
        "encrypted_header": true,
        "size_limits": true,
        "no_secrets_in_site": true
    },
    "warnings": [],
    "site_size_bytes": 25678901
}
```

### Key Management Commands

Envelope encryption enables key management without re-encrypting the payload:

```
USAGE:
    cass pages key <SUBCOMMAND>

SUBCOMMANDS:
    list        List key slots in an exported archive
    add         Add a new password/recovery key slot
    revoke      Remove a key slot (requires another valid password)
    rotate      Replace all key slots (regenerates DEK, re-encrypts payload)

OPTIONS (common):
    --archive <DIR>     Path to exported archive (site/ directory)
    --password <PASS>   Current password to authenticate
    --json              Output in JSON format

EXAMPLES:
    # List existing key slots (shows labels, not secrets)
    cass pages key list --archive ./site

    # Add a new password for a teammate
    cass pages key add --archive ./site \
        --password "current-pass" \
        --new-password "teammate-pass" \
        --label "alice"

    # Add a recovery secret (generates high-entropy secret)
    cass pages key add --archive ./site \
        --password "current-pass" \
        --recovery --label "backup-2025"

    # Revoke a compromised key slot
    cass pages key revoke --archive ./site \
        --password "good-pass" \
        --slot-id 2

    # Full key rotation (re-encrypts payload - use if DEK may be compromised)
    cass pages key rotate --archive ./site \
        --old-password "compromised-pass" \
        --new-password "fresh-pass"

OUTPUT (key list --json):
{
    "key_slots": [
        { "id": 0, "label": "password", "created_at": "2025-01-06T12:00:00Z" },
        { "id": 1, "label": "recovery", "created_at": "2025-01-06T12:00:00Z" },
        { "id": 2, "label": "alice", "created_at": "2025-01-07T09:00:00Z" }
    ],
    "active_slots": 3,
    "dek_created_at": "2025-01-06T12:00:00Z"
}

EXIT CODES:
    0   Success
    1   Authentication failed (wrong password)
    2   Invalid arguments
    3   Archive not found or corrupted
    4   Cannot revoke last remaining slot
```

**Security notes:**
- `add` and `revoke` only modify `config.json` (key slots); the encrypted payload is unchanged
- `rotate` re-encrypts the entire payload with a new DEK; use when the DEK itself may be compromised
- After any key change, re-deploy the updated `site/` directory

### Robot Mode Output

```json
{
    "status": "success",
    "export": {
        "agents": ["claude-code", "codex"],
        "workspaces": ["/home/user/project1", "/home/user/project2"],
        "time_range": {
            "from": "2024-01-01T00:00:00Z",
            "to": "2025-01-06T23:59:59Z"
        },
        "conversations": 1234,
        "messages": 56789,
        "bundle_size_bytes": 25678901,
        "encrypted": true
    },
    "deployment": {
        "target": "github",
        "repository": "username/my-archive",
        "url": "https://username.github.io/my-archive",
        "deployed_at": "2025-01-06T12:34:56Z"
    },
    "qr_code": "./qr-code.png"
}
```

---

## 13. Encryption Implementation Details

### Key Derivation Parameters

```
Algorithm: Argon2id v1.3
Memory:    64 MB (65536 KB)
Time:      3 iterations
Threads:   4 parallel lanes
Salt:      16 bytes (cryptographically random)
Output:    32 bytes (256 bits)
```

**Rationale**:
- 64 MB memory makes GPU attacks expensive (~100x slower than CPU)
- 3 iterations balance security vs. UX (2-3 second derivation)
- 4 threads utilize modern multi-core CPUs
- Matches OWASP recommendations for password storage

### Encryption Parameters

```
Algorithm:  AES-256-GCM
Key:        256 bits (from Argon2id)
Nonce:      96 bits (cryptographically random, unique per export)
Auth Tag:   128 bits (integrity verification)
```

### Binary Format (Envelope Encryption with Key Slots)

```
encrypted.bin structure:
┌────────────────────────────────────────────────────────────┐
│ Magic: "CASS" (4 bytes)                                    │
│ Version: 2 (2 bytes, little-endian) ← v2 = envelope enc    │
│ Flags: 0 (2 bytes, reserved for future compression etc.)   │
│ Payload nonce: 12 bytes (for DEK → payload encryption)     │
│ Ciphertext length: N (8 bytes, little-endian)              │
│ Ciphertext: (N bytes, compressed + encrypted payload)      │
│ Auth tag: 16 bytes (GCM tag, already included in above)    │
└────────────────────────────────────────────────────────────┘

config.json (separate file, plaintext) — now includes key slots:
{
    "version": 2,
    "algorithm": "aes-256-gcm",
    "kdf": "argon2id",
    "kdf_params": {
        "memory_kb": 65536,
        "iterations": 3,
        "parallelism": 4
    },
    "compression": "deflate",
    "key_slots": [
        {
            "id": 0,
            "label": "password",
            "salt": "base64...",       // 16 bytes, unique per slot
            "nonce": "base64...",      // 12 bytes, for KEK → DEK wrap
            "wrapped_dek": "base64..." // 48 bytes (32-byte DEK + 16-byte tag)
        },
        {
            "id": 1,
            "label": "recovery",
            "salt": "base64...",
            "nonce": "base64...",
            "wrapped_dek": "base64..."
        }
    ],
    "exported_at": "2025-01-06T12:34:56Z",
    "cass_version": "0.2.0"
}
```

### Key Slot Unlock Flow

```
User provides password or recovery secret
         │
         ▼
┌─────────────────────────────────────────────────────────┐
│  For each key_slot in config.key_slots:                  │
│    1. Derive KEK = Argon2id(input, slot.salt)           │
│    2. Try unwrap: DEK = AES-GCM-Decrypt(                │
│         KEK, slot.nonce, slot.wrapped_dek)              │
│    3. If auth tag valid → DEK found, break              │
│    4. If auth tag invalid → try next slot               │
└─────────────────────────────────────────────────────────┘
         │
         ▼ (DEK successfully unwrapped)
         │
┌─────────────────────────────────────────────────────────┐
│  Decompress + decrypt payload:                           │
│    plaintext = deflate_decompress(                      │
│      AES-GCM-Decrypt(DEK, payload_nonce, ciphertext)    │
│    )                                                     │
└─────────────────────────────────────────────────────────┘
```

**Benefits of key slots:**
- Add new passwords without re-encrypting the payload
- Revoke a compromised password by regenerating DEK + all slots
- Recovery secret is independent from user password
- Future: hardware key support (YubiKey HMAC-SHA1)

### Password Strength Validation

```rust
fn validate_password(password: &str) -> PasswordStrength {
    let length = password.len();
    let has_upper = password.chars().any(|c| c.is_uppercase());
    let has_lower = password.chars().any(|c| c.is_lowercase());
    let has_digit = password.chars().any(|c| c.is_numeric());
    let has_special = password.chars().any(|c| !c.is_alphanumeric());

    let score = match length {
        0..=7 => 0,
        8..=11 => 1,
        12..=15 => 2,
        _ => 3,
    } + has_upper as u8 + has_lower as u8 + has_digit as u8 + has_special as u8;

    match score {
        0..=2 => PasswordStrength::Weak,
        3..=4 => PasswordStrength::Fair,
        5..=6 => PasswordStrength::Good,
        _ => PasswordStrength::Strong,
    }
}
```

---

## 14. Safety Guardrails

### Guardrail 1: Encryption Required by Default

```rust
// Encryption is mandatory unless explicitly disabled
if !config.encryption_enabled {
    eprintln!("⚠️  SECURITY WARNING");
    eprintln!("You are about to export WITHOUT ENCRYPTION.");
    eprintln!();
    eprintln!("Type exactly: I UNDERSTAND AND ACCEPT THE RISKS");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if input.trim() != "I UNDERSTAND AND ACCEPT THE RISKS" {
        return Err(ExportError::UnencryptedNotConfirmed);
    }
}
```

### Guardrail 2: Pre-Publish Summary

Always shown before any deployment:

```
╭─────────────────────────────────────────────────────────────╮
│                    📋 EXPORT SUMMARY                         │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Agents:                                                     │
│    ✓ Claude Code    1,234 conversations   45,678 messages   │
│    ✓ Codex            567 conversations   12,345 messages   │
│    ✓ Gemini           234 conversations    5,678 messages   │
│    ✗ Cursor            89 conversations    (excluded)       │
│    ✗ Aider             45 conversations    (excluded)       │
│                                                              │
│  Time Range:                                                 │
│    From: 2024-01-01 00:00:00 UTC                            │
│    To:   2025-01-06 23:59:59 UTC                            │
│    Duration: 371 days                                        │
│                                                              │
│  Workspaces:                                                 │
│    • /home/user/projects/webapp         423 conversations   │
│    • /home/user/projects/api            312 conversations   │
│    • /home/user/projects/ml-pipeline    156 conversations   │
│    • ... and 12 more workspaces                             │
│                                                              │
│  Totals:                                                     │
│    Conversations: 2,035                                      │
│    Messages:      63,701                                     │
│    Est. Size:     24.5 MB (encrypted)                       │
│                                                              │
│  Security:                                                   │
│    Encryption: AES-256-GCM ✓                                │
│    Password:   Set ✓                                        │
│    QR Code:    Will be generated                            │
│                                                              │
│  Deployment:                                                 │
│    Target:     GitHub Pages                                  │
│    Repository: username/my-agent-archive (PUBLIC)            │
│    URL:        https://username.github.io/my-agent-archive   │
│                                                              │
╰─────────────────────────────────────────────────────────────╯
```

### Guardrail 3: Secret Detection

Before export, scan for potential secrets:

```rust
const SECRET_PATTERNS: &[(&str, &str)] = &[
    (r"(?i)api[_-]?key\s*[:=]\s*['\"]?[\w-]{20,}", "API Key"),
    (r"(?i)secret\s*[:=]\s*['\"]?[\w-]{20,}", "Secret"),
    (r"(?i)password\s*[:=]\s*['\"]?[^\s'\"]{8,}", "Password"),
    (r"ghp_[a-zA-Z0-9]{36}", "GitHub PAT"),
    (r"sk-[a-zA-Z0-9]{48}", "OpenAI API Key"),
    (r"-----BEGIN (RSA |EC |)PRIVATE KEY-----", "Private Key"),
];

fn scan_for_secrets(content: &str) -> Vec<SecretMatch> {
    // Returns list of potential secrets with line numbers
    // User can review before proceeding
}
```

If secrets detected:

```
⚠️  POTENTIAL SECRETS DETECTED

The following conversations may contain sensitive data:

  1. /projects/api/.claude/messages.jsonl:1234
     Possible: OpenAI API Key
     Context: "...set OPENAI_API_KEY=sk-abc123..."

  2. /projects/webapp/.claude/messages.jsonl:5678
     Possible: Password
     Context: "...password=SuperSecret123..."

Options:
  [1] Exclude these conversations and continue
  [2] Review each match individually
  [3] Continue anyway (secrets will be encrypted)
  [4] Cancel export
```

### Guardrail 4: Confirmation for Destructive Operations

```rust
// Before overwriting existing export
if output_dir.exists() && !output_dir.read_dir()?.next().is_none() {
    eprintln!("Directory {} already exists and is not empty.", output_dir.display());
    eprintln!("Contents will be DELETED and replaced.");

    if !confirm("Proceed?")? {
        return Err(ExportError::Cancelled);
    }
}

// Before deploying to existing repository
if repo_exists {
    eprintln!("Repository {} already exists.", repo_name);
    eprintln!("This will REPLACE all existing content.");

    if !confirm("Proceed?")? {
        return Err(ExportError::Cancelled);
    }
}
```

---

## 15. Migration Path & Compatibility

### cass Version Compatibility

| cass Version | Export Format | Notes |
|--------------|---------------|-------|
| 0.2.0+ | v1 | Initial release |
| Future | v2+ | Backward compatible |

### Export Format Versioning

```json
// config.json
{
    "version": 1,
    "min_viewer_version": "1.0.0",
    "cass_version": "0.2.0"
}
```

### Upgrade Path

1. **Viewer updates**: Deploy new viewer.js to existing archive
2. **Re-export**: Generate new archive with same password
3. **No data migration**: Encrypted blobs are immutable

---

## 16. Risk Analysis

### Technical Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| WASM not supported | Low | High | Fallback error message |
| Large databases slow | Medium | Medium | Chunking, lazy loading |
| Browser memory limits | Low | Medium | Streaming decryption |
| Argon2 too slow on mobile | Medium | Low | Reduced parameters option |

### Security Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Weak password chosen | Medium | High | Strength meter, warnings |
| Password shared insecurely | Medium | High | QR code alternative |
| Key logged by extension | Low | High | CSP headers |
| Side-channel attack | Very Low | Medium | Standard crypto libs |

### Usability Risks

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Password forgotten | Medium | High | QR backup, clear warnings |
| Wizard too complex | Low | Medium | Sensible defaults |
| Export takes too long | Low | Low | Progress indicators |

---

## 17. Implementation Phases

### Phase 1: Core Export (2-3 weeks)

- [ ] Database export with filters (agents, time, workspaces)
- [ ] SQLite schema for web consumption
- [ ] FTS5 index generation
- [ ] Basic CLI interface (`cass pages --export-only`)

### Phase 2: Encryption (1-2 weeks)

- [ ] Argon2id key derivation
- [ ] AES-256-GCM encryption
- [ ] QR code generation
- [ ] Password strength validation

### Phase 3: Web Viewer (2-3 weeks)

- [ ] Authentication UI (password + QR)
- [ ] Decryption module (Argon2 WASM + Web Crypto)
- [ ] sql.js integration
- [ ] Search UI
- [ ] Conversation viewer

### Phase 4: Wizard & Deployment (1-2 weeks)

- [ ] Interactive wizard (TUI-based)
- [ ] GitHub Pages deployment
- [ ] Cloudflare Pages deployment
- [ ] Local preview server

### Phase 5: Polish & Safety (1 week)

- [ ] Secret detection
- [ ] Pre-publish summary
- [ ] Safety confirmations
- [ ] Documentation

### Phase 6: Testing & Hardening (1-2 weeks)

- [ ] Cross-browser testing (Chrome, Firefox, Safari, Edge, mobile)
- [ ] Performance optimization (large archive profiling)
- [ ] Security audit (focus on crypto, CSP, input validation)
- [ ] Edge case handling

#### Crypto Test Vectors & Fuzzing

**Test Vectors (known-answer tests):**
- [ ] Argon2id: Verify against RFC 9106 test vectors
- [ ] AES-256-GCM: Verify against NIST SP 800-38D test vectors
- [ ] Key slot unwrapping: Round-trip encrypt/decrypt with multiple slots
- [ ] Chunked AEAD: Verify chunk boundary handling, nonce derivation
- [ ] AAD binding: Verify rejection when export_id or chunk_index tampered

**Fuzzing targets:**
- [ ] FTS5 query parser (malformed inputs, injection attempts)
- [ ] Password input (Unicode normalization, empty, very long)
- [ ] config.json parser (malformed JSON, missing fields, extra fields)
- [ ] Chunk file fetch (partial responses, corrupted auth tags)
- [ ] fflate decompressor (truncated streams, invalid deflate)

**Integration tests:**
- [ ] Full export → deploy → unlock cycle with test fixtures
- [ ] Key add/revoke/rotate operations with verification
- [ ] OPFS opt-in/clear-cache flow
- [ ] Graceful degradation when sqlite-wasm unavailable (sql.js fallback)

**Estimated Total: 9-14 weeks**

---

## 18. Open Questions

### Design Decisions Needed

1. **Session persistence**: Should decryption key be kept in sessionStorage (survives refresh) or memory only (maximum security)?

2. **Multiple passwords**: Should we support multiple passwords with different access levels (e.g., "viewer" vs "admin")?

3. **Expiring links**: Should we support time-limited access (e.g., "this link expires in 30 days")?

4. **Offline mode**: After initial decryption, should the viewer work offline? (Service Worker caching)

5. **Search index encryption**: Should we pre-build an encrypted search index, or build it client-side after decryption?

6. **Mobile optimization**: Should we have a separate mobile-optimized viewer, or responsive design only?

7. **Partial decryption**: Should we support decrypting individual conversations (granular encryption)?

8. **Key rotation**: Should we support changing the password without re-exporting?

### Technical Decisions Needed

1. **Argon2 WASM library**: Use `argon2-browser` (established) or `argon2-wasm` (lighter)?

2. **Chunking strategy**: Fixed-size chunks or semantic chunking (per-conversation)?

3. **Compression**: Compress before encryption (saves space) or not (simpler)?

4. **Asset embedding**: Embed all assets in Rust binary or keep separate for easier updates?

---

## 19. Appendix: Original Requirements

The following is the original prompt that initiated this proposal:

> Carefully study the /data/projects/beads_viewer (also known as "bv") repo as it pertains to the web export feature that lets you make a version of the system that can go on gh pages as a static website. We would like to do something like that for cass, but with some major changes:
>
> * It needs to be very quick and easy to interactively (or via the command line using a robot mode input) to select which of the available indexed agents to include (default is ALL agents); the time range (default is ALL logs); which project folders you want to include (default is ALL).
>
> * Because these logs can easily include secret information you wouldn't want to release publicly on a public gh pages site (and because gh pages ONLY works with public repos), we need to have a rock solid encryption system that uses a password or qr code via webcam to unlock to allow the user in the web browser to view and search and see ANYTHING about the exported indexes. The public link to the static site on gh pages should go to an authentication page first, and the user must enter the right password or use the qr code; if they do, it would decrypt the contents and show the static web app; otherwise it wouldn't work at all and would reveal nothing.
>
> * Aside from that, we'd want to use a very similar stack, with sqlite.js (wasm) and other similar libraries and techniques that allow us to compile a modular, complex web app into a few files that "just work" in a secure, performant, way on gh pages with a stunning UI/UX. And also a very similar workflow in terms of the `bv -pages` wizard, with the same conveniences and details/polish, but with the difference that it has more emphasis of security and making it hard to accidentally publish to gh pages something without a password set (this should be possible but it should require the user to literally type: "I UNDERSTAND AND ACCEPT THE RISKS" to proceed with the final publishing step; we want to help users avoid shooting themselves in the foot. We should also, just prior to publishing, show the user the full list of coding agents, project folders, and time period included in the exported sqlite db file so there are no surprises!)

---

## Document History

| Version | Date | Author | Changes |
|---------|------|--------|---------|
| 1.0 | 2026-01-06 | Claude (Opus 4.5) | Initial proposal |
| 1.1 | 2026-01-06 | Claude (Opus 4.5) | Enhanced with bv deep dive insights (see below) |
| 1.2 | 2026-01-06 | Claude (Opus 4.5) | Added envelope encryption, key slots, AAD binding |
| 1.3 | 2026-01-06 | Claude (Opus 4.5) | Added chunked AEAD, worker architecture, redaction |
| 1.4 | 2026-01-06 | Claude (Opus 4.5) | Production hardening (see below) |

### Version 1.4 Changes (Production Hardening)

This version applies 12 revisions focused on internal consistency and production-grade implementation:

1. **Crypto code consistency**: Sections 9.3, 9.4 now fully implement envelope encryption design from §7.2
2. **Streamable chunked AEAD**: Section 9.5 rewritten for streaming decrypt + decompress pipeline
3. **AAD binding**: export_id used as Additional Authenticated Data throughout to prevent chunk swapping
4. **Streaming decompression**: Added fflate library (~9KB gzipped) to Section 11
5. **SQLite runtime**: Clarified sqlite-wasm as primary (OPFS support), sql.js as fallback
6. **Worker architecture**: All crypto/decompress/DB operations in dedicated Web Worker
7. **OPFS opt-in**: Default is memory-only; "Remember on this device" checkbox enables persistence
8. **Redaction pipeline**: Added FR-6 with secret detection, user-defined rules, share profiles
9. **Key management CLI**: Added `cass pages key {list,add,revoke,rotate}` commands
10. **SQL bug fixes**: Fixed invalid materialized view (window function in WHERE), added FTS5 query escaping
11. **GitHub Pages limits**: Clarified sites are ALWAYS public, added real size limits (1GB site, 100MiB/file)
12. **Test hardening**: Phase 6 now includes crypto test vectors, fuzzing targets, integration tests

### Version 1.1 Changes (bv Deep Dive Enhancements)

Based on a comprehensive analysis of bv's (beads_viewer) web export implementation, the following enhancements were added:

#### New Sections Added:
- **Section 7.5**: Content Security Policy (CSP) with strict headers and `wasm-unsafe-eval`
- **Section 7.6**: Service Worker for CORS isolation
- **Section 9.2.1**: Pre-computed data files pattern (statistics.json, timeline.json)
- **Section 9.2.2**: Materialized views for search performance
- **Section 9.5**: Multi-tier database loading (OPFS caching, chunked downloads, SHA256 verification)
- **Section 9.6**: WASM memory management (scoped resource pattern, hybrid scorer)
- **Section 8.1.1**: Wizard implementation details (state machine, prerequisites, progress display)

#### Enhanced Existing Sections:
- **Section 9.2**: FTS5 now uses `porter unicode61` tokenizer for better search
- **Section 9.2**: Added indexes for common query patterns
- **Section 11**: Updated library versions with accurate sizes (gzipped and Brotli)
- **Section 11**: Added optional libraries (D3, Force-Graph, Mermaid)
- **Section 11**: Added bundle optimization strategies (code splitting, WASM loading, preloading)
- **Section 11**: Enhanced browser compatibility table with OPFS/SW columns

#### Key Technical Insights Incorporated:
1. **OPFS caching** survives page refreshes, providing instant database loading on return visits
2. **Database chunking** for files >5MB with 1MB chunks and SHA256 verification per chunk
3. **Multi-tier loading strategy** (OPFS cache → chunked → single file)
4. **FTS5 with Porter stemmer** matches word variants ("running" → "run")
5. **withDatabaseScope()** pattern for WASM memory management
6. **Hybrid WASM scorer** falls back to JS for small datasets (<5000 items)
7. **Pre-computed analytics** (statistics.json, timeline.json) for instant dashboard rendering
8. **CSP with wasm-unsafe-eval** required for sql.js and Argon2 WASM
9. **Service Worker** for additional CORS isolation and offline capabilities
10. **Prerequisite checking** before deployment (gh CLI, authentication, disk space)

---

*End of Proposal Document*
