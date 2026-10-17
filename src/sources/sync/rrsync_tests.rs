//! Regression coverage for restricted, read-only rsync sources (GH #531).

use super::*;

#[test]
fn rrsync_rejection_admits_only_the_argument_protection_fallback() {
    // Captured from a real rsync 3.4.1 transfer through stock rrsync -ro.
    // rrsync reports argv[0], including the executable's absolute path.
    let captured = concat!(
        "/usr/bin/rrsync error: option -s has been disabled on this server.\n",
        "rsync: connection unexpectedly closed (0 bytes received so far) [Receiver]\n",
        "rsync error: error in rsync protocol data stream (code 12) at io.c(232) [Receiver=3.4.1]\n",
    );
    assert!(rsync_arg_protection_remote_rejected(captured));
    for program in ["rrsync", "/usr/bin/rrsync", "/usr/local/bin/rrsync"] {
        assert!(rsync_arg_protection_remote_rejected(&format!(
            "{program} error: option -s has been disabled on this server."
        )));
    }
    assert!(rsync_arg_protection_remote_rejected(
        "  /USR/BIN/RRSYNC ERROR: OPTION -S HAS BEEN DISABLED ON THIS SERVER.\r\n"
    ));
}

#[test]
fn rrsync_rejection_does_not_downgrade_unrelated_policy_or_ssh_failures() {
    for diagnostic in [
        "",
        "rrsync error: option -L has been disabled on this server.",
        "rrsync error: option --delete has been disabled on this server.",
        "rrsync error: sending to this server is not allowed.",
        "rrsync error: SSH_ORIGINAL_COMMAND does not run rsync",
        "rrsync error: path outside the restricted directory",
        "Permission denied (publickey).",
        "Host key verification failed.",
        "Connection refused",
        "rsync: connection unexpectedly closed (0 bytes received so far)",
        "rsync: change_dir \"option -s has been disabled on this server.\" failed",
        "rrsync error: option -s has been disabled on this server. Permission denied",
        "/usr/bin/rrsync error: option -s has been disabled on this server. Permission denied",
        "prefix rrsync error: option -s has been disabled on this server.",
        "prefix /usr/bin/rrsync error: option -s has been disabled on this server.",
        "/usr/bin/not-rrsync error: option -s has been disabled on this server.",
        "/usr/bin/rrsync-other error: option -s has been disabled on this server.",
        "/bad path/rrsync error: option -s has been disabled on this server.",
    ] {
        assert!(
            !rsync_arg_protection_remote_rejected(diagnostic),
            "unrelated failure must not cause argument-protection fallback: {diagnostic}"
        );
    }
}

#[test]
fn restricted_source_paths_keep_rsync_escaping_and_root_relative_identity() {
    for path in [
        "/.claude/projects/",
        "/.codex/sessions/",
        "/Library/Application Support/Cursor/",
        "/session's directory/",
    ] {
        assert_eq!(
            remote_spec_for_rsync("restricted", path),
            format!("restricted:{path}")
        );
    }
    assert_eq!(RsyncArgProtection::None.flag(), None);
}

/// Exercise the real client/server protocol with a stock rrsync forced command.
/// The local pipe replaces SSH transport only; neither rsync nor rrsync is mocked.
/// Run explicitly on a Unix host with rsync and rrsync on PATH:
/// cargo test --lib rrsync_tests::restricted_readonly_transfer -- --ignored
#[cfg(unix)]
#[test]
#[ignore = "requires real rsync and rrsync executables on PATH"]
fn restricted_readonly_transfer() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::TempDir::new().expect("restricted transfer tempdir");
    let restricted = temp.path().join("restricted");
    let source = restricted.join(".claude/projects");
    std::fs::create_dir_all(&source).expect("create restricted session directory");
    std::fs::write(source.join("session.jsonl"), b"{\"message\":\"initial\"}\n")
        .expect("write session");
    std::fs::write(source.join("a quoted name.jsonl"), b"spaces\n").expect("write spaced filename");
    let adapter = temp.path().join("rrsync-transport.sh");
    std::fs::write(
        &adapter,
        concat!(
            "#!/bin/sh\nset -eu\n",
            "root=$1\nshift\nshift\n",
            "export SSH_ORIGINAL_COMMAND=\"$*\"\n",
            "exec rrsync -ro \"$root\"\n",
        ),
    )
    .expect("write local pipe transport");
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700))
        .expect("make transport executable");
    let transport = format!(
        "sh {} {}",
        quote_remote_shell_path(adapter.to_str().expect("UTF-8 adapter path")),
        quote_remote_shell_path(restricted.to_str().expect("UTF-8 restricted path")),
    );
    let remote = remote_spec_for_rsync("fixture", "/.claude/projects/");

    for protection in [
        RsyncArgProtection::SecludedArgs,
        RsyncArgProtection::ProtectArgs,
    ] {
        let destination = temp.path().join(protection.flag().expect("protected flag"));
        std::fs::create_dir_all(&destination).expect("create destination");
        let destination_str = destination.to_str().expect("UTF-8 destination");
        let transfer = |protection| {
            run_rsync_command("5", &transport, &remote, destination_str, protection)
                .expect("execute real rsync")
        };
        let rejected = transfer(protection);
        assert!(
            !rejected.status.success(),
            "rrsync must reject hidden server arguments"
        );
        assert!(rsync_arg_protection_remote_rejected(
            &String::from_utf8_lossy(&rejected.stderr)
        ));
        assert!(!destination.join("session.jsonl").exists());

        let accepted = transfer(RsyncArgProtection::None);
        assert!(
            accepted.status.success(),
            "restricted fallback failed: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        for name in ["session.jsonl", "a quoted name.jsonl"] {
            assert_eq!(
                std::fs::read(destination.join(name)).unwrap(),
                std::fs::read(source.join(name)).unwrap()
            );
        }
        std::fs::write(destination.join("local-only.jsonl"), b"retain me")
            .expect("write local-only file");
        let unchanged = transfer(RsyncArgProtection::None);
        assert!(unchanged.status.success());
        assert_eq!(
            std::fs::read(destination.join("local-only.jsonl")).unwrap(),
            b"retain me",
            "fallback must retain additive sync semantics"
        );
    }
}
