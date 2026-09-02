//! `cass pages key …` — the operator CLI over [`super::key_management`].
//!
//! The key-slot engine (LUKS-style: several independently wrapped copies of
//! one data-encryption key, stable slot ids, atomic publish of the mutated
//! config) has been complete for months, and `docs/RECOVERY.md` has documented
//! this command the whole time; until the reality check of 2026-09-01 nothing
//! called it. This module is the wiring: argument parsing, password
//! acquisition, and truthful output. It contains no key material logic of its
//! own — every mutation goes through the engine's guarded functions.
//!
//! Password rules follow the rest of cass: a password is never accepted on the
//! command line (argv is visible to every process on the host). It is read
//! from stdin with `--password-stdin` (one password per line: the current
//! password first, then the new one where a verb needs one) or prompted for
//! interactively when stdin is a terminal.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;
use zeroize::Zeroizing;

use super::key_management::{
    KeyListResult, RevokeResult, RotateResult, key_add_password, key_add_recovery, key_list,
    key_revoke, key_rotate,
};
use crate::model::cli_error_kind::ErrorKind as CliErrorKind;
use crate::{CliError, CliResult};

/// Exit code for a key-management failure the operator can act on (wrong
/// password, refusing to revoke the last slot, unsupported bundle format).
pub const PAGES_KEY_EXIT_FAILED: i32 = 1;
/// Exit code for a usage error (bad slot id, missing archive path).
pub const PAGES_KEY_EXIT_USAGE: i32 = 2;
/// Exit code when the archive path does not resolve to a pages bundle.
pub const PAGES_KEY_EXIT_NOT_FOUND: i32 = 3;
/// Exit code when a password was required but none could be obtained.
pub const PAGES_KEY_EXIT_PASSWORD: i32 = 6;

#[derive(Debug, Clone, Args)]
pub struct PagesKeyArgs {
    #[command(subcommand)]
    pub command: PagesKeyCommand,
}

/// Shared arguments for every key verb.
#[derive(Debug, Clone, Args)]
pub struct PagesKeyCommon {
    /// Exported pages bundle: the bundle root (containing `site/`) or the
    /// `site/` directory itself.
    #[arg(long)]
    pub archive: PathBuf,

    /// Read passwords from stdin, one per line, instead of prompting: the
    /// current password first, then the new password for verbs that take one.
    #[arg(long)]
    pub password_stdin: bool,

    /// Machine-readable output (one JSON document on stdout).
    #[arg(long, visible_alias = "robot")]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PagesKeyCommand {
    /// List the key slots of an encrypted archive (never needs a password).
    List(PagesKeyCommon),
    /// Add a second password slot (unlocks with the current password).
    AddPassword(PagesKeyCommon),
    /// Add a recovery-secret slot; the secret is printed exactly once.
    AddRecovery(PagesKeyCommon),
    /// Revoke one key slot by id (the last remaining slot cannot be revoked).
    Revoke {
        #[command(flatten)]
        common: PagesKeyCommon,
        /// Slot id to revoke, as shown by `key list`.
        #[arg(long)]
        slot: u8,
    },
    /// Re-encrypt the archive under a fresh data-encryption key with a new
    /// password; every existing slot is discarded.
    Rotate {
        #[command(flatten)]
        common: PagesKeyCommon,
        /// Also mint a new recovery secret for the rotated key (printed once).
        #[arg(long)]
        keep_recovery: bool,
    },
}

impl PagesKeyCommand {
    pub fn common(&self) -> &PagesKeyCommon {
        match self {
            Self::List(common) | Self::AddPassword(common) | Self::AddRecovery(common) => common,
            Self::Revoke { common, .. } | Self::Rotate { common, .. } => common,
        }
    }

    pub fn json(&self) -> bool {
        self.common().json
    }

    fn verb(&self) -> &'static str {
        match self {
            Self::List(_) => "list",
            Self::AddPassword(_) => "add-password",
            Self::AddRecovery(_) => "add-recovery",
            Self::Revoke { .. } => "revoke",
            Self::Rotate { .. } => "rotate",
        }
    }

    fn needs_new_password(&self) -> bool {
        matches!(self, Self::AddPassword(_) | Self::Rotate { .. })
    }
}

/// Passwords resolved for one invocation. Kept separate from acquisition so
/// the verbs can be tested without a terminal or a piped stdin.
#[derive(Debug)]
pub struct KeyPasswords {
    pub current: Zeroizing<String>,
    pub new: Option<Zeroizing<String>>,
}

/// What one verb did, in the shape both the human and JSON renderers use.
#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum PagesKeyOutcome {
    List {
        archive: PathBuf,
        #[serde(flatten)]
        result: KeyListResult,
    },
    AddPassword {
        archive: PathBuf,
        slot_id: u8,
        active_slots: usize,
    },
    AddRecovery {
        archive: PathBuf,
        slot_id: u8,
        active_slots: usize,
        /// The recovery secret, shown exactly once. It is not stored anywhere
        /// in the archive; losing it means this slot can never be used.
        recovery_secret: String,
    },
    Revoke {
        archive: PathBuf,
        #[serde(flatten)]
        result: RevokeResult,
    },
    Rotate {
        archive: PathBuf,
        #[serde(flatten)]
        result: RotateResult,
    },
}

/// Entry point used by the `cass pages key …` dispatch arm.
pub fn run_pages_key_command(args: &PagesKeyArgs) -> CliResult<()> {
    let command = &args.command;
    let passwords = if matches!(command, PagesKeyCommand::List(_)) {
        None
    } else {
        Some(acquire_passwords(command)?)
    };
    let outcome = execute_pages_key_command(command, passwords.as_ref())?;
    render_outcome(&outcome, command.json());
    Ok(())
}

/// Run one verb against the archive. `passwords` is required for every verb
/// except `list`.
pub fn execute_pages_key_command(
    command: &PagesKeyCommand,
    passwords: Option<&KeyPasswords>,
) -> CliResult<PagesKeyOutcome> {
    let common = command.common();
    let archive = resolve_archive(&common.archive)?;
    let verb = command.verb();
    let current = || -> CliResult<&str> {
        passwords
            .map(|p| p.current.as_str())
            .ok_or_else(|| password_required(verb))
    };
    let new_password = || -> CliResult<&str> {
        passwords
            .and_then(|p| p.new.as_deref())
            .map(String::as_str)
            .ok_or_else(|| password_required(verb))
    };

    match command {
        PagesKeyCommand::List(_) => {
            let result = key_list(&archive).map_err(|err| key_failure(verb, &archive, &err))?;
            Ok(PagesKeyOutcome::List { archive, result })
        }
        PagesKeyCommand::AddPassword(_) => {
            let slot_id = key_add_password(&archive, current()?, new_password()?)
                .map_err(|err| key_failure(verb, &archive, &err))?;
            let active_slots = active_slot_count(&archive, verb)?;
            Ok(PagesKeyOutcome::AddPassword {
                archive,
                slot_id,
                active_slots,
            })
        }
        PagesKeyCommand::AddRecovery(_) => {
            let (slot_id, secret) = key_add_recovery(&archive, current()?)
                .map_err(|err| key_failure(verb, &archive, &err))?;
            let active_slots = active_slot_count(&archive, verb)?;
            Ok(PagesKeyOutcome::AddRecovery {
                archive,
                slot_id,
                active_slots,
                recovery_secret: secret.encoded().to_string(),
            })
        }
        PagesKeyCommand::Revoke { slot, .. } => {
            let result = key_revoke(&archive, current()?, *slot)
                .map_err(|err| key_failure(verb, &archive, &err))?;
            Ok(PagesKeyOutcome::Revoke { archive, result })
        }
        PagesKeyCommand::Rotate { keep_recovery, .. } => {
            let result = key_rotate(
                &archive,
                current()?,
                new_password()?,
                *keep_recovery,
                |_progress| {},
            )
            .map_err(|err| key_failure(verb, &archive, &err))?;
            Ok(PagesKeyOutcome::Rotate { archive, result })
        }
    }
}

fn active_slot_count(archive: &Path, verb: &'static str) -> CliResult<usize> {
    key_list(archive)
        .map(|listing| listing.active_slots)
        .map_err(|err| key_failure(verb, archive, &err))
}

fn resolve_archive(path: &Path) -> CliResult<PathBuf> {
    super::resolve_site_dir(path).map_err(|err| CliError {
        code: PAGES_KEY_EXIT_NOT_FOUND,
        kind: CliErrorKind::Pages.kind_str(),
        message: format!("{} is not a pages bundle: {err:#}", path.display()),
        hint: Some(
            "Pass the exported bundle root (the directory containing site/) or its site/ directory"
                .to_string(),
        ),
        retryable: false,
    })
}

fn key_failure(verb: &'static str, archive: &Path, err: &anyhow::Error) -> CliError {
    let message = format!("{err:#}");
    let hint = if message.contains("last remaining key slot") {
        Some("Add another slot with `key add-password` or `key add-recovery` first".to_string())
    } else if message.to_ascii_lowercase().contains("password") {
        Some("The current password did not unlock any slot; check it and retry".to_string())
    } else {
        Some(format!(
            "Run `cass pages --verify {}` to check the bundle before retrying",
            archive.display()
        ))
    };
    CliError {
        code: PAGES_KEY_EXIT_FAILED,
        kind: CliErrorKind::Pages.kind_str(),
        message: format!("pages key {verb} failed: {message}"),
        hint,
        retryable: false,
    }
}

fn password_required(verb: &'static str) -> CliError {
    CliError {
        code: PAGES_KEY_EXIT_PASSWORD,
        kind: CliErrorKind::PasswordRequired.kind_str(),
        message: format!("pages key {verb} needs the archive password"),
        hint: Some(
            "Pipe it with --password-stdin (current password on line 1, new password on line 2 \
             for add-password/rotate) or run interactively"
                .to_string(),
        ),
        retryable: false,
    }
}

/// Obtain the passwords a verb needs: from stdin lines with
/// `--password-stdin`, otherwise from interactive prompts when stdin is a
/// terminal. Never from argv.
fn acquire_passwords(command: &PagesKeyCommand) -> CliResult<KeyPasswords> {
    let common = command.common();
    let verb = command.verb();
    if common.password_stdin {
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        let current = read_password_line(&mut lines, verb, "current password")?;
        let new = if command.needs_new_password() {
            Some(read_password_line(&mut lines, verb, "new password")?)
        } else {
            None
        };
        return Ok(KeyPasswords { current, new });
    }
    if !std::io::stdin().is_terminal() {
        return Err(password_required(verb));
    }
    let current = prompt_password("Current archive password", verb)?;
    let new = if command.needs_new_password() {
        let first = prompt_password("New password", verb)?;
        let confirm = prompt_password("Confirm new password", verb)?;
        if *first != *confirm {
            return Err(CliError {
                code: PAGES_KEY_EXIT_USAGE,
                kind: CliErrorKind::Usage.kind_str(),
                message: "the new password and its confirmation differ".to_string(),
                hint: None,
                retryable: false,
            });
        }
        Some(first)
    } else {
        None
    };
    Ok(KeyPasswords { current, new })
}

fn read_password_line(
    lines: &mut std::io::Lines<std::io::StdinLock<'_>>,
    verb: &'static str,
    what: &str,
) -> CliResult<Zeroizing<String>> {
    let line = lines.next().transpose().map_err(|err| CliError {
        code: PAGES_KEY_EXIT_PASSWORD,
        kind: CliErrorKind::PasswordReadError.kind_str(),
        message: format!("pages key {verb}: failed to read the {what} from stdin: {err}"),
        hint: None,
        retryable: false,
    })?;
    let value = line
        .map(|line| line.trim_end_matches(['\r', '\n']).to_string())
        .unwrap_or_default();
    if value.is_empty() {
        return Err(CliError {
            code: PAGES_KEY_EXIT_PASSWORD,
            kind: CliErrorKind::PasswordRequired.kind_str(),
            message: format!("pages key {verb}: the {what} read from stdin was empty"),
            hint: Some("--password-stdin expects one non-empty password per line".to_string()),
            retryable: false,
        });
    }
    Ok(Zeroizing::new(value))
}

fn prompt_password(prompt: &str, verb: &'static str) -> CliResult<Zeroizing<String>> {
    dialoguer::Password::new()
        .with_prompt(prompt)
        .allow_empty_password(false)
        .interact()
        .map(Zeroizing::new)
        .map_err(|err| CliError {
            code: PAGES_KEY_EXIT_PASSWORD,
            kind: CliErrorKind::PasswordReadError.kind_str(),
            message: format!("pages key {verb}: password prompt failed: {err}"),
            hint: Some("Use --password-stdin in non-interactive sessions".to_string()),
            retryable: false,
        })
}

fn render_outcome(outcome: &PagesKeyOutcome, json: bool) {
    if json {
        let mut value = serde_json::to_value(outcome).unwrap_or_else(
            |err| serde_json::json!({ "action": "unknown", "serialize_error": err.to_string() }),
        );
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert("success".to_string(), serde_json::Value::Bool(true));
        }
        println!("{value}");
        if matches!(outcome, PagesKeyOutcome::AddRecovery { .. })
            || matches!(
                outcome,
                PagesKeyOutcome::Rotate { result, .. } if result.recovery_secret.is_some()
            )
        {
            eprintln!(
                "note: the recovery secret above is shown once and is not stored in the archive"
            );
        }
        return;
    }
    match outcome {
        PagesKeyOutcome::List { archive, result } => {
            println!("Key slots for {}", archive.display());
            println!(
                "  export id: {}   active slots: {}   dek created: {}",
                result.export_id,
                result.active_slots,
                result.dek_created_at.as_deref().unwrap_or("unknown")
            );
            for slot in &result.slots {
                let label = slot
                    .label
                    .as_deref()
                    .map(|label| format!("  label: {label}"))
                    .unwrap_or_default();
                println!(
                    "  slot {:>3}  {:<9}  kdf: {}{}",
                    slot.id, slot.slot_type, slot.kdf, label
                );
            }
        }
        PagesKeyOutcome::AddPassword {
            archive,
            slot_id,
            active_slots,
        } => {
            println!(
                "Added password slot {slot_id} to {} ({active_slots} active slots)",
                archive.display()
            );
        }
        PagesKeyOutcome::AddRecovery {
            archive,
            slot_id,
            active_slots,
            recovery_secret,
        } => {
            println!(
                "Added recovery slot {slot_id} to {} ({active_slots} active slots)",
                archive.display()
            );
            println!();
            println!("  RECOVERY SECRET (shown once, not stored anywhere):");
            println!("  {recovery_secret}");
            println!();
            println!("  Store it offline. Without it this slot can never be used.");
        }
        PagesKeyOutcome::Revoke { archive, result } => {
            println!(
                "Revoked slot {} from {} ({} slots remain)",
                result.revoked_slot_id,
                archive.display(),
                result.remaining_slots
            );
        }
        PagesKeyOutcome::Rotate { archive, result } => {
            println!(
                "Rotated the data-encryption key for {} at {} ({} slot{})",
                archive.display(),
                result.new_dek_created_at.to_rfc3339(),
                result.slot_count,
                if result.slot_count == 1 { "" } else { "s" }
            );
            if let Some(secret) = &result.recovery_secret {
                println!();
                println!("  NEW RECOVERY SECRET (shown once, not stored anywhere):");
                println!("  {secret}");
                println!();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::bundle::BundleBuilder;
    use crate::pages::encrypt::EncryptionEngine;
    use tempfile::TempDir;

    const PASSWORD: &str = "correct horse battery staple";

    /// A real encrypted bundle (engine + bundle builder), exactly what
    /// `cass pages` exports; no mocks stand in for the key engine.
    fn encrypted_bundle() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let input = temp.path().join("input.txt");
        let encrypted = temp.path().join("encrypted");
        let bundle = temp.path().join("bundle");
        std::fs::write(&input, b"pages key cli fixture").unwrap();
        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot(PASSWORD).unwrap();
        engine.encrypt_file(&input, &encrypted, |_, _| {}).unwrap();
        BundleBuilder::new()
            .build(&encrypted, &bundle, |_, _| {})
            .unwrap();
        (temp, bundle)
    }

    fn common(archive: &Path) -> PagesKeyCommon {
        PagesKeyCommon {
            archive: archive.to_path_buf(),
            password_stdin: false,
            json: false,
        }
    }

    fn passwords(current: &str, new: Option<&str>) -> KeyPasswords {
        KeyPasswords {
            current: Zeroizing::new(current.to_string()),
            new: new.map(|value| Zeroizing::new(value.to_string())),
        }
    }

    fn list(archive: &Path) -> KeyListResult {
        match execute_pages_key_command(&PagesKeyCommand::List(common(archive)), None).unwrap() {
            PagesKeyOutcome::List { result, .. } => result,
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    /// Positive observable: every verb round-trips against a real bundle and
    /// `list` reflects each mutation. Planted negatives: a wrong password is a
    /// typed `pages` failure (exit 1) with the password hint, and revoking the
    /// last slot is refused with the add-first hint. No-claim: stdin/prompt
    /// acquisition is exercised by the CLI test, not here.
    #[test]
    fn every_verb_round_trips_on_a_real_bundle_and_refuses_the_unsafe_cases() {
        let (_temp, bundle) = encrypted_bundle();
        let initial = list(&bundle);
        assert_eq!(initial.active_slots, 1, "{initial:?}");
        assert_eq!(initial.slots[0].slot_type, "password");

        // add-password
        let outcome = execute_pages_key_command(
            &PagesKeyCommand::AddPassword(common(&bundle)),
            Some(&passwords(PASSWORD, Some("second password 42"))),
        )
        .unwrap();
        let PagesKeyOutcome::AddPassword {
            slot_id,
            active_slots,
            ..
        } = outcome
        else {
            panic!("expected add-password outcome");
        };
        assert_eq!(slot_id, 1);
        assert_eq!(active_slots, 2);

        // The new password unlocks the archive for the next mutation.
        let outcome = execute_pages_key_command(
            &PagesKeyCommand::AddRecovery(common(&bundle)),
            Some(&passwords("second password 42", None)),
        )
        .unwrap();
        let PagesKeyOutcome::AddRecovery {
            slot_id,
            active_slots,
            recovery_secret,
            ..
        } = outcome
        else {
            panic!("expected add-recovery outcome");
        };
        assert_eq!(slot_id, 2);
        assert_eq!(active_slots, 3);
        assert!(
            recovery_secret.len() >= 32,
            "recovery secret must be the encoded secret, got {recovery_secret:?}"
        );
        assert_eq!(list(&bundle).slots[2].slot_type, "recovery");

        // revoke the second password slot
        let outcome = execute_pages_key_command(
            &PagesKeyCommand::Revoke {
                common: common(&bundle),
                slot: 1,
            },
            Some(&passwords(PASSWORD, None)),
        )
        .unwrap();
        let PagesKeyOutcome::Revoke { result, .. } = outcome else {
            panic!("expected revoke outcome");
        };
        assert_eq!(result.revoked_slot_id, 1);
        assert_eq!(result.remaining_slots, 2);
        assert!(
            list(&bundle).slots.iter().all(|slot| slot.id != 1),
            "slot 1 must be gone"
        );

        // Planted negative: the wrong password is a typed failure, not a panic
        // and not a silent success.
        let err = execute_pages_key_command(
            &PagesKeyCommand::AddPassword(common(&bundle)),
            Some(&passwords("not the password", Some("whatever password"))),
        )
        .unwrap_err();
        assert_eq!(err.code, PAGES_KEY_EXIT_FAILED, "{err:?}");
        assert_eq!(err.kind, "pages");
        assert!(
            err.message.contains("add-password failed"),
            "{}",
            err.message
        );
        assert_eq!(
            list(&bundle).active_slots,
            2,
            "a refused mutation changes nothing"
        );

        // rotate with a fresh recovery secret: only the new slots survive.
        let outcome = execute_pages_key_command(
            &PagesKeyCommand::Rotate {
                common: common(&bundle),
                keep_recovery: true,
            },
            Some(&passwords(PASSWORD, Some("rotated password 99"))),
        )
        .unwrap();
        let PagesKeyOutcome::Rotate { result, .. } = outcome else {
            panic!("expected rotate outcome");
        };
        assert_eq!(result.slot_count, 2, "{result:?}");
        assert!(result.recovery_secret.is_some());
        let rotated = list(&bundle);
        assert_eq!(rotated.active_slots, 2);
        assert_ne!(
            rotated.export_id, initial.export_id,
            "rotation must mint a new export id"
        );

        // The old password is dead after rotation.
        let err = execute_pages_key_command(
            &PagesKeyCommand::AddRecovery(common(&bundle)),
            Some(&passwords(PASSWORD, None)),
        )
        .unwrap_err();
        assert_eq!(err.code, PAGES_KEY_EXIT_FAILED);

        // Planted negative: revoking down to the last slot is refused.
        let recovery_slot = rotated
            .slots
            .iter()
            .find(|slot| slot.slot_type == "recovery")
            .map(|slot| slot.id)
            .expect("rotate --keep-recovery leaves a recovery slot");
        execute_pages_key_command(
            &PagesKeyCommand::Revoke {
                common: common(&bundle),
                slot: recovery_slot,
            },
            Some(&passwords("rotated password 99", None)),
        )
        .unwrap();
        let err = execute_pages_key_command(
            &PagesKeyCommand::Revoke {
                common: common(&bundle),
                slot: list(&bundle).slots[0].id,
            },
            Some(&passwords("rotated password 99", None)),
        )
        .unwrap_err();
        assert_eq!(err.code, PAGES_KEY_EXIT_FAILED);
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|hint| hint.contains("add-password")),
            "{err:?}"
        );
        assert_eq!(list(&bundle).active_slots, 1);
    }

    /// A verb that needs a password refuses to run without one (exit 6,
    /// `password-required`) and `list` never asks for one.
    #[test]
    fn password_verbs_require_a_password_and_list_does_not() {
        let (_temp, bundle) = encrypted_bundle();
        let err = execute_pages_key_command(&PagesKeyCommand::AddRecovery(common(&bundle)), None)
            .unwrap_err();
        assert_eq!(err.code, PAGES_KEY_EXIT_PASSWORD);
        assert_eq!(err.kind, "password-required");
        let err = execute_pages_key_command(
            &PagesKeyCommand::AddPassword(common(&bundle)),
            Some(&passwords(PASSWORD, None)),
        )
        .unwrap_err();
        assert_eq!(
            err.code, PAGES_KEY_EXIT_PASSWORD,
            "add-password needs the new password too"
        );
        assert_eq!(list(&bundle).active_slots, 1);
    }

    /// A path that is not a bundle is exit 3 with the bundle hint.
    #[test]
    fn non_bundle_paths_are_not_found() {
        let temp = TempDir::new().unwrap();
        let err = execute_pages_key_command(
            &PagesKeyCommand::List(common(&temp.path().join("missing"))),
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, PAGES_KEY_EXIT_NOT_FOUND);
        assert_eq!(err.kind, "pages");
    }

    /// The JSON document carries `success`, the action tag, and the flattened
    /// engine result, so `jq .active_slots` works without a wrapper.
    #[test]
    fn json_outcome_is_flat_and_tagged() {
        let (_temp, bundle) = encrypted_bundle();
        let outcome =
            execute_pages_key_command(&PagesKeyCommand::List(common(&bundle)), None).unwrap();
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(value["action"], serde_json::json!("list"));
        assert_eq!(value["active_slots"], serde_json::json!(1));
        assert!(value["slots"].is_array());
        assert!(value["export_id"].is_string());
    }
}
