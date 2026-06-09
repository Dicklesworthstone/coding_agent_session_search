//! Cross-platform (macOS/Linux) path and tooling compatibility for fleet probes.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.5
//! ("Handle macOS/Linux path and tooling differences in fleet diagnostics").
//!
//! Fleet probes run shell snippets on heterogeneous hosts and parse the results
//! back on a (typically Linux) controller. macOS hosts diverge in ways that
//! silently break naive probes: `uname -s` is `Darwin`, the data dir lives under
//! `~/Library/Application Support`, home is `/Users/<name>` not `/home/<name>`,
//! and BSD userland rejects GNU-isms like `date -Is`. This module is the pure,
//! testable kernel for those differences: OS detection, per-OS data-dir
//! resolution, path-origin classification, username redaction, and portable
//! command snippets that avoid GNU-only assumptions. It composes with the
//! fleet-doctor schema (`6.1`) by producing its [`HostOs`] and the `tool_notes`
//! consumed by `Platform`.
//!
//! The motivating evidence: a `mac-mini-max` probe surfaced a BSD `date -Is`
//! incompatibility and a macOS-specific data dir, and its workspace mismatch was
//! high precisely because the controller assumed Linux paths.

use crate::fleet_doctor_schema::HostOs;
use serde::{Deserialize, Serialize};

/// Detect [`HostOs`] from the output of `uname -s` (case-insensitive,
/// whitespace-tolerant). Unknown kernels map to [`HostOs::Other`] rather than a
/// guess.
pub fn detect_host_os(uname_s: &str) -> HostOs {
    let s = uname_s.trim().to_ascii_lowercase();
    if s == "darwin" {
        HostOs::MacOs
    } else if s == "linux" {
        HostOs::Linux
    } else if s.starts_with("mingw") || s.starts_with("msys") || s.starts_with("cygwin") || s.contains("windows") {
        HostOs::Windows
    } else {
        HostOs::Other
    }
}

/// Resolve the default CASS data directory for `os` given the host's `$HOME`,
/// honoring `$XDG_DATA_HOME` on Linux when provided.
///
/// - macOS: `<home>/Library/Application Support/cass`
/// - Linux: `$XDG_DATA_HOME/cass` if set, else `<home>/.local/share/cass`
/// - Windows: `<home>\AppData\Roaming\cass`
/// - Other: `<home>/.cass`
pub fn default_data_dir(os: HostOs, home: &str, xdg_data_home: Option<&str>) -> String {
    let home = home.trim_end_matches('/');
    match os {
        HostOs::MacOs => format!("{home}/Library/Application Support/cass"),
        HostOs::Linux => match xdg_data_home.map(str::trim).filter(|s| !s.is_empty()) {
            Some(xdg) => format!("{}/cass", xdg.trim_end_matches('/')),
            None => format!("{home}/.local/share/cass"),
        },
        HostOs::Windows => format!("{}\\AppData\\Roaming\\cass", home.trim_end_matches('\\')),
        HostOs::Other => format!("{home}/.cass"),
    }
}

/// Where a path appears to originate, so the controller can reason about
/// workspace provenance without assuming a single layout.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PathOrigin {
    /// `/Users/<name>/…` — a macOS home directory.
    MacUsersHome,
    /// `/home/<name>/…` — a Linux home directory.
    LinuxHome,
    /// `/dp/…` or `/data/projects/…` — a sibling-checkout / workspace area.
    DpCheckout,
    /// Anything else.
    Other,
}

/// Classify a path's origin from its leading components.
pub fn classify_path_origin(path: &str) -> PathOrigin {
    let p = path.trim();
    if p.starts_with("/Users/") {
        PathOrigin::MacUsersHome
    } else if p.starts_with("/home/") {
        PathOrigin::LinuxHome
    } else if p.starts_with("/dp/") || p.starts_with("/data/projects/") {
        PathOrigin::DpCheckout
    } else {
        PathOrigin::Other
    }
}

/// Redact the username component from a home-rooted path, preserving structure
/// for correlation while removing PII. `/Users/alice/x` -> `/Users/<user>/x`,
/// `/home/bob/y` -> `/home/<user>/y`. Non-home paths are returned unchanged.
pub fn redact_user_path(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            // rest = "<name>" or "<name>/sub/...". Replace the first segment.
            let tail = rest.split_once('/').map(|(_, t)| t);
            return match tail {
                Some(t) => format!("{prefix}<user>/{t}"),
                None => format!("{prefix}<user>"),
            };
        }
    }
    path.to_string()
}

/// A portable shell command whose output the controller can parse identically on
/// GNU and BSD userland. Each carries the command plus a note on why the naive
/// GNU form was avoided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableCommand {
    /// The portable command to run.
    pub command: &'static str,
    /// The GNU-only form it replaces (for documentation/auditing).
    pub avoids: &'static str,
}

/// An ISO-8601-ish UTC timestamp command that works on both GNU coreutils and
/// BSD `date`. GNU's `date -Is` / `date --iso-8601=seconds` are unavailable on
/// BSD; the explicit `+FORMAT` works everywhere.
pub const PORTABLE_ISO_TIMESTAMP: PortableCommand = PortableCommand {
    command: "date -u +%Y-%m-%dT%H:%M:%SZ",
    avoids: "date -Is / date --iso-8601=seconds (GNU-only)",
};

/// A portable epoch-seconds command (both GNU and BSD support `+%s`).
pub const PORTABLE_EPOCH_SECONDS: PortableCommand = PortableCommand {
    command: "date -u +%s",
    avoids: "date --utc=@... arithmetic (GNU-only)",
};

/// Structured `tool_notes` (key=value tokens consumed by
/// [`crate::fleet_doctor_schema::Platform`]) describing how a host's userland
/// diverges from a GNU/Linux controller. Empty for a plain Linux host.
pub fn tool_notes_for(os: HostOs) -> Vec<String> {
    match os {
        HostOs::MacOs => vec![
            "date=bsd".to_string(),
            "coreutils=bsd".to_string(),
            "data_dir=library-application-support".to_string(),
            "home=/Users".to_string(),
        ],
        HostOs::Windows => vec!["shell=non-posix".to_string(), "data_dir=appdata".to_string()],
        HostOs::Linux | HostOs::Other => Vec::new(),
    }
}

/// `true` if a probe snippet contains a GNU-only construct that would fail on BSD
/// userland without a guard. Conservative best-effort lint for probe authoring.
pub fn has_unguarded_gnuism(snippet: &str) -> bool {
    const GNUISMS: &[&str] = &[
        "date -Is",
        "date --iso-8601",
        "readlink -f", // BSD readlink lacks -f; use realpath or a guard
        "sed -r",      // BSD sed uses -E
        "grep -P",     // BSD grep lacks PCRE
        "stat -c",     // BSD stat uses -f
    ];
    GNUISMS.iter().any(|g| snippet.contains(g))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_darwin_and_linux_uname() {
        assert_eq!(detect_host_os("Darwin"), HostOs::MacOs);
        assert_eq!(detect_host_os("darwin\n"), HostOs::MacOs);
        assert_eq!(detect_host_os("  Linux "), HostOs::Linux);
        assert_eq!(detect_host_os("MINGW64_NT-10.0"), HostOs::Windows);
        assert_eq!(detect_host_os("FreeBSD"), HostOs::Other);
    }

    #[test]
    fn macos_data_dir_uses_application_support() {
        let dir = default_data_dir(HostOs::MacOs, "/Users/alice", None);
        assert_eq!(dir, "/Users/alice/Library/Application Support/cass");
    }

    #[test]
    fn linux_data_dir_uses_xdg_then_local_share() {
        assert_eq!(
            default_data_dir(HostOs::Linux, "/home/bob", Some("/home/bob/.xdg")),
            "/home/bob/.xdg/cass"
        );
        assert_eq!(
            default_data_dir(HostOs::Linux, "/home/bob", None),
            "/home/bob/.local/share/cass"
        );
        // Empty XDG falls back to ~/.local/share.
        assert_eq!(
            default_data_dir(HostOs::Linux, "/home/bob", Some("  ")),
            "/home/bob/.local/share/cass"
        );
    }

    #[test]
    fn data_dir_trims_trailing_home_slash() {
        assert_eq!(
            default_data_dir(HostOs::MacOs, "/Users/alice/", None),
            "/Users/alice/Library/Application Support/cass"
        );
    }

    #[test]
    fn classifies_users_home_dp_and_linux_paths() {
        assert_eq!(classify_path_origin("/Users/alice/.claude"), PathOrigin::MacUsersHome);
        assert_eq!(classify_path_origin("/home/bob/.codex"), PathOrigin::LinuxHome);
        assert_eq!(classify_path_origin("/dp/frankensqlite"), PathOrigin::DpCheckout);
        assert_eq!(
            classify_path_origin("/data/projects/coding_agent_session_search"),
            PathOrigin::DpCheckout
        );
        assert_eq!(classify_path_origin("/opt/whatever"), PathOrigin::Other);
    }

    #[test]
    fn redacts_username_from_home_paths() {
        assert_eq!(redact_user_path("/Users/alice/.claude/x"), "/Users/<user>/.claude/x");
        assert_eq!(redact_user_path("/home/bob/.codex"), "/home/<user>/.codex");
        assert_eq!(redact_user_path("/Users/alice"), "/Users/<user>");
        // Non-home paths are unchanged.
        assert_eq!(redact_user_path("/data/projects/x"), "/data/projects/x");
        assert_eq!(redact_user_path("/opt/cass"), "/opt/cass");
    }

    #[test]
    fn portable_timestamp_avoids_gnu_only_flags() {
        assert_eq!(PORTABLE_ISO_TIMESTAMP.command, "date -u +%Y-%m-%dT%H:%M:%SZ");
        assert!(!PORTABLE_ISO_TIMESTAMP.command.contains("-Is"));
        assert!(!PORTABLE_ISO_TIMESTAMP.command.contains("--iso-8601"));
        assert!(PORTABLE_ISO_TIMESTAMP.avoids.contains("GNU-only"));
        assert!(PORTABLE_EPOCH_SECONDS.command.contains("+%s"));
    }

    #[test]
    fn tool_notes_flag_macos_bsd_divergences_and_are_empty_on_linux() {
        let mac = tool_notes_for(HostOs::MacOs);
        assert!(mac.iter().any(|n| n == "date=bsd"));
        assert!(mac.iter().any(|n| n.starts_with("data_dir=")));
        assert!(tool_notes_for(HostOs::Linux).is_empty());
    }

    #[test]
    fn detects_unguarded_gnuisms() {
        assert!(has_unguarded_gnuism("ts=$(date -Is)"));
        assert!(has_unguarded_gnuism("p=$(readlink -f \"$x\")"));
        assert!(has_unguarded_gnuism("sed -r 's/a/b/'"));
        assert!(has_unguarded_gnuism("grep -P '\\d+'"));
        assert!(has_unguarded_gnuism("stat -c %s file"));
        // The portable form is clean.
        assert!(!has_unguarded_gnuism(PORTABLE_ISO_TIMESTAMP.command));
        assert!(!has_unguarded_gnuism("realpath \"$x\""));
    }

    #[test]
    fn path_origin_wire_values_are_kebab_case() {
        for (origin, wire) in [
            (PathOrigin::MacUsersHome, "mac-users-home"),
            (PathOrigin::LinuxHome, "linux-home"),
            (PathOrigin::DpCheckout, "dp-checkout"),
            (PathOrigin::Other, "other"),
        ] {
            assert_eq!(serde_json::to_string(&origin).unwrap(), format!("\"{wire}\""));
        }
    }
}
