use super::*;

#[cfg(target_os = "linux")]
fn session_source(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(&format!("cass-writer-probe-{}-", std::process::id()))
            })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    // Retain tiny synthetic fixtures, including failure evidence. No source or
    // test directory is removed by these tests.
    struct Fixture {
        root: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Fixture {
        fn new() -> io::Result<Self> {
            // The production census is global. Keep synthetic writer owners
            // from other tests from entering this test's initial snapshot and
            // exiting during its refresh; each fixture still uses real procfs.
            static WRITERS: Mutex<()> = Mutex::new(());
            let guard = WRITERS.lock().unwrap_or_else(|error| error.into_inner());
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cass-writer-probe-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path)?;
            Ok(Self {
                root: path,
                _guard: guard,
            })
        }

        fn source(&self, name: &str) -> io::Result<PathBuf> {
            let path = self.root.join(format!("{name}.jsonl"));
            fs::write(&path, b"{\"complete\":\"durable session row\"}\n")?;
            Ok(path)
        }
    }

    fn writer(path: &Path) -> io::Result<File> {
        OpenOptions::new().append(true).open(path)
    }

    fn captured(probe: &SourceWriterProbe, identity: FileIdentity) -> bool {
        probe
            .state
            .lock()
            .unwrap()
            .census
            .writers
            .contains_key(&identity)
    }

    fn refresh_count(probe: &SourceWriterProbe) -> usize {
        probe.state.lock().unwrap().refresh_count
    }

    /// z6ycn: as a non-root user every process whose descriptors this user
    /// cannot list is a visibility gap, and one that appears during a census
    /// (an sshd session on a build worker, for example) blocks the refresh.
    /// Keeping the positive is then the probe's intended answer. True only when
    /// the census was otherwise complete, every owner stayed visible, and every
    /// new gap belongs to some other process.
    fn refresh_blocked_only_by_foreign_opacity(probe: &SourceWriterProbe) -> bool {
        let state = probe.state.lock().unwrap();
        state.last_refresh_block.as_ref().is_some_and(|block| {
            block.complete
                && block.owners_visible
                && !block.new_gap_pids.is_empty()
                && !block.new_gap_pids.contains(&std::process::id())
        })
    }

    /// Query a source whose answer depends on a refresh. `None` means that
    /// refresh was blocked only by foreign opacity, after asserting the
    /// conservative answer; the caller retries its whole scenario.
    fn query_after_refresh(probe: &SourceWriterProbe, identity: FileIdentity) -> Option<bool> {
        let deferred = probe.should_defer(identity);
        if refresh_blocked_only_by_foreign_opacity(probe) {
            assert!(deferred, "a blocked refresh must keep the cached positive");
            return None;
        }
        Some(deferred)
    }

    /// Run a live-procfs scenario with fresh fixtures until one attempt's
    /// refreshes all kept an unchanged visibility scope (`Ok(true)`). Every
    /// assertion still runs on that attempt; a host where no attempt succeeds
    /// fails the test rather than skipping it.
    fn with_foreign_opacity_retries(
        mut scenario: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<()> {
        const ATTEMPTS: usize = 20;
        for _ in 0..ATTEMPTS {
            if scenario()? {
                return Ok(());
            }
        }
        Err(io::Error::other(format!(
            "all {ATTEMPTS} attempts had a refresh blocked by newly opaque foreign processes"
        )))
    }

    #[test]
    fn cached_writer_is_rechecked_after_the_descriptor_closes() -> io::Result<()> {
        with_foreign_opacity_retries(|| {
            let fixture = Fixture::new()?;
            let path = fixture.source("closed")?;
            let identity = file_identity(&path)?;
            let contents = fs::read(&path)?;
            let modified = fs::metadata(&path)?.modified()?;
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(
                captured(&probe, identity),
                "the old immutable set would defer this source"
            );
            drop(held);
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(
                !deferred,
                "closed writer must not remain active for the scan"
            );
            assert_eq!(refresh_count(&probe), 1);
            assert_eq!(fs::read(&path)?, contents);
            assert_eq!(fs::metadata(&path)?.modified()?, modified);
            Ok(true)
        })
    }

    #[test]
    fn a_still_held_writer_remains_deferred_without_another_census() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("held")?;
        let identity = file_identity(&path)?;
        let _held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        for _ in 0..5 {
            assert!(probe.should_defer(identity));
        }
        assert_eq!(refresh_count(&probe), 0);
        Ok(())
    }

    #[test]
    fn read_only_descriptors_never_supply_writer_evidence() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("read-only")?;
        let identity = file_identity(&path)?;
        let _reader = File::open(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(!captured(&probe, identity));
        assert!(!probe.should_defer(identity));
        assert_eq!(refresh_count(&probe), 0);

        with_foreign_opacity_retries(|| {
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            drop(held);
            let _successor_reader = File::open(&path)?;
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(!deferred, "a read-only successor is not writer evidence");
            Ok(true)
        })
    }

    #[test]
    fn a_successor_descriptor_is_found_after_the_original_closes() -> io::Result<()> {
        with_foreign_opacity_retries(|| {
            let fixture = Fixture::new()?;
            let path = fixture.source("successor")?;
            let identity = file_identity(&path)?;
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            // This distinct descriptor did not exist during the original census.
            let successor = writer(&path)?;
            drop(held);
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(deferred);
            assert_eq!(refresh_count(&probe), 1);
            drop(successor);
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(!deferred);
            Ok(true)
        })
    }

    #[test]
    fn a_known_writer_is_found_through_a_non_session_hardlink() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("hardlink-successor")?;
        let alias = fixture.root.join("writer-handle.data");
        fs::hard_link(&path, &alias)?;
        let identity = file_identity(&path)?;

        // Initial capture retains its existing source-path predicate scope.
        let held_alias = writer(&alias)?;
        let initial = SourceWriterProbe::capture(session_source);
        assert!(!captured(&initial, identity));
        drop(held_alias);

        with_foreign_opacity_retries(|| {
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            let successor = writer(&alias)?;
            drop(held);
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(
                deferred,
                "a known positive inode remains writable through another suffix"
            );
            assert_eq!(refresh_count(&probe), 1);
            drop(successor);
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(!deferred);
            Ok(true)
        })
    }

    #[test]
    fn initial_capture_never_follows_a_non_session_descriptor() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.source("followed")?;
        let feed = fixture.root.join("feed.tsv.gz");
        fs::write(&feed, b"not a session source")?;
        let _held_source = writer(&source)?;
        let _held_feed = writer(&feed)?;
        let probe = SourceWriterProbe::capture(session_source);
        let state = probe.state.lock().unwrap();
        assert!(state.census.writers.contains_key(&file_identity(&source)?));
        assert!(
            state.census.followed.contains(&fs::canonicalize(&source)?),
            "a session-named writer is followed for its identity"
        );
        // Following this one stats it on its own filesystem. On a hung sshfs
        // mount that stat blocked for minutes, and index runs on the host sat
        // in the census until their stall watchdog fired.
        assert!(!state.census.followed.contains(&fs::canonicalize(&feed)?));
        Ok(())
    }

    #[test]
    fn mount_table_marks_network_and_fuse_mounts_remote() {
        let mounts = parse_mount_identities(
            &[
                "22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw",
                "36 35 98:0 /mnt1 /mnt/parent rw,noatime master:1 - ext3 /dev/root rw",
                "40 1 0:45 / /data/ssd rw,relatime shared:30 - zfs pool/ssd rw,xattr",
                "41 1 0:46 / /tmp rw,nosuid,nodev shared:31 - tmpfs tmpfs rw,size=8g",
                "42 24 0:47 / /sys/fs/fuse/connections rw,relatime shared:32 - fusectl fusectl rw",
                "412 30 0:61 / /home/u/bulk rw,relatime shared:220 - fuse.sshfs u@host:/bulk rw",
                "413 30 0:62 / /mnt/nas rw,relatime shared:221 - fuseblk /dev/sdb1 rw",
                "414 30 0:63 / /net/home rw,relatime shared:222 - nfs4 server:/home rw,vers=4.2",
                // An escaped space in a mount path never forms the separator.
                "415 30 0:64 / /mnt/a\\040-\\040b rw,relatime - cifs //server/share rw",
                "not a mount line",
            ]
            .join("\n"),
        );
        for local in [22, 36, 40, 41, 42] {
            assert_eq!(
                mounts.get(&local).map(|mount| mount.remote),
                Some(false),
                "mount {local}"
            );
        }
        for remote in [412, 413, 414, 415] {
            assert_eq!(
                mounts.get(&remote).map(|mount| mount.remote),
                Some(true),
                "mount {remote}"
            );
        }
        assert_eq!(mounts.len(), 9);
    }

    #[test]
    fn a_descriptor_not_proven_local_counts_as_remote() -> io::Result<()> {
        let fixture = Fixture::new()?;
        fs::write(
            fixture.root.join("mountinfo"),
            [
                "36 35 98:0 / /data rw,noatime - ext4 /dev/sda1 rw",
                "412 30 0:61 / /bulk rw,relatime - fuse.sshfs u@host:/bulk rw",
            ]
            .join("\n"),
        )?;
        let fdinfo = |name: &str, contents: &str| -> io::Result<PathBuf> {
            let path = fixture.root.join(name);
            fs::write(&path, contents)?;
            Ok(path)
        };
        let local = fdinfo("local", "pos:\t0\nflags:\t02100001\nmnt_id:\t36\nino:\t7\n")?;
        let sshfs = fdinfo("sshfs", "pos:\t0\nflags:\t02100001\nmnt_id:\t412\n")?;
        let unlisted = fdinfo("unlisted", "pos:\t0\nflags:\t02100001\nmnt_id:\t999\n")?;
        let unnamed = fdinfo("no-mount-id", "pos:\t0\nflags:\t02100001\n")?;
        let mut mounts = None;
        assert!(
            descriptor_mount(&fixture.root, &read_descriptor_info(&local)?, &mut mounts)
                .is_some_and(|mount| !mount.remote)
        );
        for descriptor in [&sshfs, &unlisted, &unnamed] {
            assert!(
                descriptor_mount(
                    &fixture.root,
                    &read_descriptor_info(descriptor)?,
                    &mut mounts
                )
                .is_none_or(|mount| mount.remote),
                "{}",
                descriptor.display()
            );
        }
        // Without a readable mount table no descriptor is proven local.
        let mut unreadable = None;
        assert!(
            descriptor_mount(
                &fixture.root.join("no-such-process"),
                &read_descriptor_info(&local)?,
                &mut unreadable
            )
            .is_none()
        );
        Ok(())
    }

    /// A real directory tree with procfs field syntax and a dangling fd link.
    /// Following that link cannot supply an identity; only the procfs fields
    /// can. No live network mount or privileged mount operation is needed.
    fn remote_process(fixture: &Fixture, filesystem: &str) -> io::Result<PathBuf> {
        use std::os::unix::fs::symlink;

        let process = fixture.root.join("424242");
        fs::create_dir_all(process.join("fd"))?;
        fs::create_dir_all(process.join("fdinfo"))?;
        fs::write(
            process.join("stat"),
            format!("424242 (session writer) S {} 42\n", ["0"; 18].join(" ")),
        )?;
        fs::write(
            process.join("mountinfo"),
            format!("412 30 0:61 / /bulk rw,relatime - {filesystem} remote rw\n"),
        )?;
        fs::write(
            process.join("fdinfo/7"),
            "pos:\t0\nflags:\t02100001\nmnt_id:\t412\nino:\t987654321\n",
        )?;
        symlink(
            fixture.root.join("unavailable-session.jsonl"),
            process.join("fd/7"),
        )?;
        Ok(process)
    }

    #[test]
    fn remote_session_writers_are_captured_and_rechecked_without_following_targets()
    -> io::Result<()> {
        for filesystem in ["fuse.sshfs", "nfs4", "cifs"] {
            let fixture = Fixture::new()?;
            let process = remote_process(&fixture, filesystem)?;
            let census = collect_writers_in(&fixture.root, session_source, &HashMap::new());
            let identity = (61, 987_654_321);
            assert!(census.complete);
            assert!(census.visibility_gaps.is_empty());
            assert!(census.followed.is_empty());
            assert_eq!(census.writers.len(), 1);
            let witness = &census.writers[&identity][0];
            assert_eq!(witness.observe(identity), DescriptorState::Open);
            assert_eq!(witness.observe((61, 987_654_322)), DescriptorState::Closed);
            assert_eq!(
                fs::metadata(process.join("fd/7")).unwrap_err().kind(),
                io::ErrorKind::NotFound,
                "the unresolvable target cannot have supplied this identity"
            );

            // Reuse of the descriptor as a reader revokes writer evidence
            // without following the target, too.
            fs::write(
                process.join("fdinfo/7"),
                "pos:\t0\nflags:\t02100000\nmnt_id:\t412\nino:\t987654321\n",
            )?;
            assert_eq!(witness.observe(identity), DescriptorState::Closed);
        }
        Ok(())
    }

    #[test]
    fn incomplete_remote_identity_never_stats_or_revokes_a_prior_positive() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let process = remote_process(&fixture, "fuse.sshfs")?;
        let identity = (61, 987_654_321);
        let census = collect_writers_in(&fixture.root, session_source, &HashMap::new());
        let witness = &census.writers[&identity][0];
        for contents in [
            // Linux before 5.14 has no inode field. Never revive the hang
            // by falling back to target metadata on those kernels.
            "flags:\t02100001\nmnt_id:\t412\n",
            "flags:\t02100001\nmnt_id:\t412\nino:\tinvalid\n",
            "flags:\t02100001\nmnt_id:\t999\nino:\t987654321\n",
            "flags:\t02100001\nino:\t987654321\n",
        ] {
            fs::write(process.join("fdinfo/7"), contents)?;
            assert_eq!(witness.observe(identity), DescriptorState::Uncertain);
            let fresh = collect_writers_in(&fixture.root, session_source, &census.writers);
            assert!(fresh.followed.is_empty());
            assert_eq!(fresh.visibility_gaps.len(), 1);
            assert!(
                fresh
                    .visibility_gaps
                    .iter()
                    .all(|gap| gap.inspection == Inspection::Identity)
            );
        }
        let mut state = ProbeState {
            census,
            ..ProbeState::default()
        };
        let fresh = collect_writers_in(&fixture.root, session_source, &state.census.writers);
        state.apply_refresh(fresh);
        assert!(state.refresh_blocked);
        assert!(state.census.writers.contains_key(&identity));
        Ok(())
    }

    #[test]
    fn malformed_mount_devices_do_not_authorize_target_metadata() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let process = remote_process(&fixture, "fuse.sshfs")?;
        let info = read_descriptor_info(&process.join("fdinfo/7"))?;
        for device in ["no-device", "0:invalid", "4294967296:61", "0:4294967296"] {
            fs::write(
                process.join("mountinfo"),
                format!("412 30 {device} / /bulk rw - ext4 source rw\n"),
            )?;
            let mount = descriptor_mount(&process, &info, &mut None);
            assert!(mount.is_none());
            assert_eq!(
                descriptor_identity(&process.join("fd/7"), &info, mount)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData,
                "an unknown mount cannot be treated as local"
            );
        }
        Ok(())
    }

    #[test]
    fn local_mounts_retain_metadata_identity_without_procfs_inode() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.source("legacy-kernel")?;
        let info = DescriptorInfo {
            writable: true,
            mount_id: Some(36),
            inode: None,
        };
        assert_eq!(
            descriptor_identity(
                &source,
                &info,
                Some(MountIdentity {
                    device: 0,
                    remote: false,
                })
            )?,
            file_identity(&source)?
        );
        Ok(())
    }

    #[test]
    fn census_rejects_access_or_identity_changes_during_inspection() -> io::Result<()> {
        fn change_fdinfo_after_admission(path: &Path) -> bool {
            if !session_source(path) {
                return false;
            }
            let root = path.parent().unwrap();
            fs::write(
                root.join("424242/fdinfo/7"),
                fs::read(root.join("next-fdinfo")).unwrap(),
            )
            .unwrap();
            true
        }

        for replacement in [
            "flags:\t02100000\nmnt_id:\t412\nino:\t987654321\n",
            "flags:\t02100001\nmnt_id:\t412\nino:\t987654322\n",
        ] {
            let fixture = Fixture::new()?;
            remote_process(&fixture, "nfs4")?;
            let original = collect_writers_in(&fixture.root, session_source, &HashMap::new());
            let identity = (61, 987_654_321);
            assert!(original.writers.contains_key(&identity));
            fs::write(fixture.root.join("next-fdinfo"), replacement)?;
            // The admission predicate is reached after the first real fdinfo
            // read. Mutating the fixture here deterministically recreates a
            // close/reopen race between access and identity observations.
            let fresh = collect_writers_in(
                &fixture.root,
                change_fdinfo_after_admission,
                &original.writers,
            );
            assert!(fresh.writers.is_empty());
            assert!(fresh.followed.is_empty());
            assert!(fresh.visibility_gaps.iter().any(|gap| {
                gap.inspection == Inspection::Identity
                    && gap.error_kind == io::ErrorKind::WouldBlock
            }));
            let mut state = ProbeState {
                census: original,
                ..ProbeState::default()
            };
            state.apply_refresh(fresh);
            assert!(state.refresh_blocked);
            assert!(state.census.writers.contains_key(&identity));
        }
        Ok(())
    }

    #[test]
    fn mount_device_encoding_agrees_with_live_procfs_file_identity() -> io::Result<()> {
        use std::os::fd::AsRawFd;

        // Procfs has a uniform device/inode namespace; unlike overlayfs it
        // does not substitute a lower filesystem's identity in stat results.
        let file = File::open("/proc/self/stat")?;
        let descriptor = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let info = read_descriptor_info(&PathBuf::from(format!(
            "/proc/self/fdinfo/{}",
            file.as_raw_fd()
        )))?;
        let mount = descriptor_mount(Path::new("/proc/self"), &info, &mut None)
            .expect("an open procfs file must have a visible mount");
        assert_eq!(mount.device, file_identity(&descriptor)?.0);
        // High minors need their upper bits moved past the major field.
        assert_eq!(linux_device_id(8, 257), 0x10_0801);
        assert_eq!(linux_device_id(0x1234, 0x56789abc), 0x1567_89a2_34bc);
        Ok(())
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "holder subprocess invoked by the cross-process successor regression"]
    fn writer_holder_child() -> io::Result<()> {
        let root = PathBuf::from(std::env::var_os("CASS_WRITER_PROBE_CHILD_ROOT").unwrap());
        // The parent's writable file is held as stdin while this test runs.
        fs::write(root.join("ready"), b"descriptor retained")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("release").try_exists()? {
            assert!(Instant::now() < deadline, "parent did not release holder");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    fn a_new_process_writer_cannot_be_missed_when_the_cached_owner_closes() -> io::Result<()> {
        with_foreign_opacity_retries(|| {
            let fixture = Fixture::new()?;
            let path = fixture.source("new-process")?;
            let identity = file_identity(&path)?;
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            let module = module_path!().split_once("::").unwrap().1;
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args([
                    "--ignored",
                    "--exact",
                    &format!("{module}::writer_holder_child"),
                ])
                .env("CASS_WRITER_PROBE_CHILD_ROOT", &fixture.root)
                .stdin(Stdio::from(held))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            let mut child = ChildGuard(command.spawn()?);
            drop(command);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !fixture.root.join("ready").try_exists()? {
                assert!(
                    child.0.try_wait()?.is_none(),
                    "holder exited before readiness"
                );
                assert!(Instant::now() < deadline, "holder did not become ready");
                std::thread::sleep(Duration::from_millis(5));
            }
            let Some(deferred) = query_after_refresh(&probe, identity) else {
                return Ok(false);
            };
            assert!(deferred, "successor lives in a new process");
            assert_eq!(refresh_count(&probe), 1);
            fs::write(fixture.root.join("release"), b"release")?;
            assert!(child.0.wait()?.success());
            // Disappearance cannot distinguish a departed owner from a PID hidden
            // by procfs policy. Preserve the positive until a new filter capture.
            assert!(probe.should_defer(identity));
            assert_eq!(refresh_count(&probe), 2);
            assert!(probe.should_defer(identity));
            assert_eq!(refresh_count(&probe), 2);
            assert!(!SourceWriterProbe::capture(session_source).should_defer(identity));
            Ok(true)
        })
    }

    #[test]
    fn one_refresh_clears_twenty_already_closed_positive_sources() -> io::Result<()> {
        with_foreign_opacity_retries(|| {
            let fixture = Fixture::new()?;
            let mut writers = Vec::new();
            let mut identities = Vec::new();
            for index in 0..20 {
                let path = fixture.source(&format!("source-{index}"))?;
                identities.push(file_identity(&path)?);
                writers.push(writer(&path)?);
            }
            let probe = SourceWriterProbe::capture(session_source);
            assert!(
                identities
                    .iter()
                    .all(|&identity| captured(&probe, identity))
            );
            drop(writers);
            for identity in identities {
                let Some(deferred) = query_after_refresh(&probe, identity) else {
                    return Ok(false);
                };
                assert!(!deferred);
            }
            assert_eq!(
                refresh_count(&probe),
                1,
                "one shared census covers all closed sources"
            );
            Ok(true)
        })
    }

    #[test]
    fn unreadable_descriptor_evidence_cannot_clear_a_cached_positive() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("uncertain")?;
        let identity = file_identity(&path)?;
        let _held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        {
            let mut state = probe.state.lock().unwrap();
            let witnesses = state.census.writers.get_mut(&identity).unwrap();
            for witness in witnesses {
                let DescriptorWitness::Proc { info, .. } = witness;
                // A real I/O error, independent of root's permission bypass:
                // read_to_string cannot read a directory as descriptor flags.
                info.clone_from(&fixture.root);
            }
        }
        assert!(probe.should_defer(identity));
        assert_eq!(refresh_count(&probe), 0);
        Ok(())
    }

    #[test]
    fn incomplete_refresh_retains_prior_positive_evidence() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("partial-census")?;
        let identity = file_identity(&path)?;
        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        probe
            .state
            .lock()
            .unwrap()
            .apply_refresh(WriterCensus::default());
        drop(held);
        assert!(
            probe.should_defer(identity),
            "partial inspection is not proof of absence"
        );
        assert_eq!(
            refresh_count(&probe),
            0,
            "failed census is not repeated for each source"
        );
        Ok(())
    }

    fn opaque_descriptor(pid: u32, start_time: u64, descriptor: &str) -> VisibilityGap {
        VisibilityGap {
            process: ProcessIdentity { pid, start_time },
            descriptor: Some(descriptor.into()),
            inspection: Inspection::AccessFlags,
            error_kind: io::ErrorKind::PermissionDenied,
        }
    }

    #[test]
    fn stable_preexisting_opacity_preserves_the_initial_inspection_scope() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("stable-opacity")?;
        let identity = file_identity(&path)?;
        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        drop(held);
        let mut state = probe.state.lock().unwrap();
        let gap = opaque_descriptor(999_991, 101, "3");
        state.initial_visibility_gaps.insert(gap.clone());
        let visible_processes = state.census.visible_processes.clone();
        state.apply_refresh(WriterCensus {
            complete: true,
            visibility_gaps: HashSet::from([gap]),
            visible_processes,
            ..WriterCensus::default()
        });
        assert!(!state.refresh_blocked);
        assert!(!state.census.writers.contains_key(&identity));
        Ok(())
    }

    #[test]
    fn new_opacity_and_pid_replacement_cannot_clear_prior_writer_evidence() -> io::Result<()> {
        for new_gap in [
            opaque_descriptor(999_991, 101, "4"), // New opaque descriptor.
            opaque_descriptor(999_992, 101, "3"), // New opaque process.
            opaque_descriptor(999_991, 102, "3"), // Same PID, different process lifetime.
        ] {
            let fixture = Fixture::new()?;
            let path = fixture.source("new-opacity")?;
            let identity = file_identity(&path)?;
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            drop(held);
            let mut state = probe.state.lock().unwrap();
            state
                .initial_visibility_gaps
                .insert(opaque_descriptor(999_991, 101, "3"));
            let visible_processes = state.census.visible_processes.clone();
            state.apply_refresh(WriterCensus {
                complete: true,
                visibility_gaps: HashSet::from([new_gap]),
                visible_processes,
                ..WriterCensus::default()
            });
            assert!(state.refresh_blocked);
            assert!(state.census.writers.contains_key(&identity));
        }
        Ok(())
    }

    #[test]
    fn opacity_without_a_process_start_time_is_never_baselined() {
        let mut census = WriterCensus {
            complete: true,
            ..WriterCensus::default()
        };
        census.observe_error(
            io::Error::from(io::ErrorKind::PermissionDenied),
            None,
            None,
            Inspection::DescriptorInventory,
        );
        assert!(!census.complete);
        assert!(census.visibility_gaps.is_empty());
    }

    #[test]
    fn a_missing_or_replaced_owner_cannot_clear_a_cached_positive() -> io::Result<()> {
        for replacement in [false, true] {
            let fixture = Fixture::new()?;
            let path = fixture.source("missing-owner")?;
            let identity = file_identity(&path)?;
            let held = writer(&path)?;
            let probe = SourceWriterProbe::capture(session_source);
            assert!(captured(&probe, identity));
            drop(held);
            let mut state = probe.state.lock().unwrap();
            let DescriptorWitness::Proc { process, .. } = state.census.writers[&identity][0];
            let owner = process.expect("the fixture owner has a visible process start time");
            let visible_processes = if replacement {
                HashSet::from([ProcessIdentity {
                    start_time: owner.start_time + 1,
                    ..owner
                }])
            } else {
                HashSet::new()
            };
            // Model a complete-looking procfs census that omits the old PID,
            // or has recycled its number. Neither is proof that the writer died.
            state.apply_refresh(WriterCensus {
                complete: true,
                visible_processes,
                ..WriterCensus::default()
            });
            assert!(state.refresh_blocked);
            assert!(state.census.writers.contains_key(&identity));
        }
        Ok(())
    }

    #[test]
    fn a_refresh_for_another_source_retains_every_unobserved_writer_owner() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let first = fixture.source("queried-owner")?;
        let second = fixture.source("unqueried-owner")?;
        let first_id = file_identity(&first)?;
        let second_id = file_identity(&second)?;
        let held_first = writer(&first)?;
        let held_second = writer(&second)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, first_id));
        assert!(captured(&probe, second_id));
        drop(held_first);
        drop(held_second);

        let mut state = probe.state.lock().unwrap();
        let hidden_owner = ProcessIdentity {
            pid: 999_993,
            start_time: 123,
        };
        // Deterministically model a second captured owner that subsequently
        // vanishes from the process census. The first source's owner stays visible.
        for witness in state.census.writers.get_mut(&second_id).unwrap() {
            let DescriptorWitness::Proc { process, .. } = witness;
            *process = Some(hidden_owner);
        }
        let visible_processes = state.census.visible_processes.clone();
        state.census.visible_processes.insert(hidden_owner);
        state.apply_refresh(WriterCensus {
            complete: true,
            visible_processes,
            ..WriterCensus::default()
        });
        assert!(state.refresh_blocked);
        assert!(state.census.writers.contains_key(&first_id));
        assert!(state.census.writers.contains_key(&second_id));
        Ok(())
    }

    #[test]
    fn a_positive_without_an_identifiable_owner_cannot_be_cleared() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("unknown-owner")?;
        let identity = file_identity(&path)?;
        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        drop(held);
        let mut state = probe.state.lock().unwrap();
        for witness in state.census.writers.get_mut(&identity).unwrap() {
            let DescriptorWitness::Proc { process, .. } = witness;
            *process = None;
        }
        let visible_processes = state.census.visible_processes.clone();
        state.apply_refresh(WriterCensus {
            complete: true,
            visible_processes,
            ..WriterCensus::default()
        });
        assert!(state.refresh_blocked);
        assert!(state.census.writers.contains_key(&identity));
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_captured_positives_remain_deferred_without_lsof_reinspection() {
    let identity = (101, 202);
    let probe = SourceWriterProbe {
        writers: HashMap::from([(identity, vec![(999_991, 3)])]),
    };
    // An inaccessible or nonexistent PID must not turn an lsof omission into
    // proof of closure. These lookups require no subprocess or OS inspection.
    for _ in 0..5 {
        assert!(probe.should_defer(identity));
    }
    assert!(!probe.should_defer((101, 203)));
}

#[test]
fn lsof_uses_access_fields_and_resets_at_descriptor_and_process_boundaries() {
    let parsed = parse_lsof(
        b"p101\0\nf3\0aw\0D0x1000002\0i300\0n/tmp/write.jsonl\0\nf4\0ar\0D0x1000002\0i400\0n/tmp/read.jsonl\0\nf5\0au\0D0x1000003\0i500\0n/tmp/update.jsonl\0\np202\0\nf6\0ar\0D0x1000004\0i600\0n/tmp/other.jsonl\0\n",
    );
    assert!(parsed.complete);
    assert_eq!(parsed.records.len(), 4);
    assert_eq!(
        parsed
            .records
            .iter()
            .map(|record| (record.process, record.descriptor, record.writable))
            .collect::<Vec<_>>(),
        vec![
            (101, 3, true),
            (101, 4, false),
            (101, 5, true),
            (202, 6, false)
        ]
    );
    assert_eq!(parsed.records[2].path, Path::new("/tmp/update.jsonl"));
    assert_eq!(parsed.records[2].identity, Some((0x1000003, 500)));
    assert_eq!(parsed.records[3].identity, Some((0x1000004, 600)));
}

#[test]
fn lsof_missing_access_mode_cannot_inherit_a_previous_writer_or_prove_absence() {
    for output in [
        b"p101\0f3\0aw\0n/tmp/first.jsonl\0f4\0n/tmp/no-mode.jsonl\0".as_slice(),
        b"p101\0f3\0aw\0n/tmp/first.jsonl\0p202\0f4\0n/tmp/no-mode.jsonl\0".as_slice(),
        b"p101\0f3\0a?\0n/tmp/unknown-mode.jsonl\0".as_slice(),
        b"p101\0fNOFD\0n/proc/101/fd permission denied\0".as_slice(),
    ] {
        assert!(
            !parse_lsof(output).complete,
            "uncertain output must not clear old positives"
        );
    }
}

#[test]
fn lsof_file_names_cannot_inject_descriptor_fields_through_newlines() {
    let parsed = parse_lsof(b"p101\0f3\0aw\0D0x10\0i20\0n/tmp/session\np999\nf4\nar.jsonl\0\n");
    assert!(parsed.complete);
    assert_eq!(parsed.records.len(), 1);
    assert_eq!(parsed.records[0].process, 101);
    assert!(parsed.records[0].writable);
    assert_eq!(parsed.records[0].identity, Some((16, 20)));
    assert_eq!(
        parsed.records[0].path,
        Path::new("/tmp/session\np999\nf4\nar.jsonl")
    );
}

#[test]
fn lsof_access_and_name_fields_may_arrive_in_either_order() {
    let parsed = parse_lsof(b"p101\0fcwd\0n/tmp\0f3\0i20\0n/tmp/write.jsonl\0D0x10\0aw\0\n");
    assert!(parsed.complete);
    assert_eq!(parsed.records.len(), 1);
    assert!(parsed.records[0].writable);
    assert_eq!(parsed.records[0].descriptor, 3);
    assert_eq!(parsed.records[0].identity, Some((16, 20)));
}

#[test]
fn malformed_or_truncated_lsof_output_cannot_prove_absence() {
    for output in [
        b"".as_slice(),
        b"garbage\0".as_slice(),
        b"p101\0f3\0aw\0n/tmp/session.jsonl".as_slice(),
        b"p101\0f3\0aw\0".as_slice(),
        b"p101\0f3w\0n/tmp/session.jsonl\0".as_slice(),
        b"f3\0aw\0n/tmp/session.jsonl\0".as_slice(),
    ] {
        assert!(
            !parse_lsof(output).complete,
            "malformed output was accepted: {output:?}"
        );
    }
}

fn lsof_session_source(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

#[test]
fn lsof_device_and_inode_fields_preserve_darwin_identity() {
    let parsed = parse_lsof(
        b"p101\0f3\0aw\0D0x80000002\0i18446744073709551615\0n/missing/session.jsonl\0\n",
    );
    assert!(parsed.complete);
    assert_eq!(
        parsed.records[0].identity,
        Some((0xffff_ffff_8000_0002, u64::MAX)),
        "Darwin's signed dev_t is sign-extended by MetadataExt::dev"
    );
    assert_eq!(parse_lsof_device("0x1aB"), Some(0x1ab));
    for malformed in ["", "10", "0x", "0x+1", "0x-1", "0x 1", "0x100000000"] {
        assert_eq!(parse_lsof_device(malformed), None, "{malformed}");
    }
}

#[test]
fn lsof_identity_never_inherits_across_records_or_processes() {
    let census = lsof_writer_census(
        b"p101\0f3\0aw\0D0x10\0i20\0n/missing/first.jsonl\0f4\0aw\0i21\0n/missing/no-device.jsonl\0p202\0f5\0aw\0D0x11\0n/missing/no-inode.jsonl\0",
        true,
        lsof_session_source,
    );
    assert!(census.visibility_gap);
    assert_eq!(census.writers, HashMap::from([((16, 20), vec![(101, 3)])]));
}

#[test]
fn lsof_invalid_identity_keeps_other_observed_positives() {
    let first = b"p101\0f3\0aw\0D0x10\0i20\0n/missing/first.jsonl\0";
    for fields in [
        b"D0x11\0D0x12\0i21\0".as_slice(),
        b"D0x11\0i21\0i22\0".as_slice(),
        b"D0x11\0i-1\0".as_slice(),
        b"D0x11\0i+1\0".as_slice(),
        b"D0x11\0i18446744073709551616\0".as_slice(),
        b"D0x11\0i0x15\0".as_slice(),
        b"D0x100000000\0i21\0".as_slice(),
        b"D0x11\0i21\0ar\0".as_slice(),
        b"D0x11\0i21\0a?\0aw\0".as_slice(),
    ] {
        let output = [
            first.as_slice(),
            b"f4\0aw\0n/missing/invalid.jsonl\0",
            fields,
        ]
        .concat();
        let census = lsof_writer_census(&output, true, lsof_session_source);
        assert!(census.visibility_gap, "{fields:?}");
        assert_eq!(
            census.writers,
            HashMap::from([((16, 20), vec![(101, 3)])]),
            "{fields:?}"
        );
    }
}

#[test]
fn malformed_lsof_boundaries_do_not_reuse_a_prior_process_or_descriptor() {
    let first = b"p101\0f3\0aw\0D0x10\0i20\0n/missing/first.jsonl\0";
    for boundary in [
        b"p\xff\0f4\0".as_slice(),
        b"p+202\0f4\0".as_slice(),
        b"p0\0f4\0".as_slice(),
        b"f\xff\0".as_slice(),
        b"f+4\0".as_slice(),
    ] {
        let output = [
            first.as_slice(),
            boundary,
            b"aw\0D0x11\0i21\0n/missing/other.jsonl\0",
        ]
        .concat();
        let census = lsof_writer_census(&output, true, lsof_session_source);
        assert!(census.visibility_gap, "{boundary:?}");
        assert_eq!(
            census.writers,
            HashMap::from([((16, 20), vec![(101, 3)])]),
            "{boundary:?}"
        );
    }
}

#[test]
fn lsof_census_uses_descriptor_identity_without_path_metadata() {
    // These nonexistent paths cannot supply metadata. The device/inode from
    // lsof still identifies the writable source; readonly and unrelated files
    // must not become writer evidence. A partial run retains that positive.
    let output = b"p101\0f3\0aw\0D0x10\0i20\0n/missing-cass-probe/session.jsonl\0f4\0ar\0D0x10\0i21\0n/missing-cass-probe/readonly.jsonl\0f5\0au\0D0x10\0i22\0n/missing-cass-probe/feed.csv\0";
    for complete in [true, false] {
        let census = lsof_writer_census(output, complete, lsof_session_source);
        assert_eq!(census.visibility_gap, !complete);
        assert_eq!(census.writers, HashMap::from([((16, 20), vec![(101, 3)])]));
    }
}

#[test]
fn truncated_lsof_field_cannot_supply_a_source_or_identity() {
    for output in [
        b"p101\0f3\0aw\0D0x10\0i20\0n/missing/session.jsonl".as_slice(),
        b"p101\0f3\0aw\0D0x10\0n/missing/session.jsonl\0i20".as_slice(),
        b"p101\0f3\0aw\0i20\0n/missing/session.jsonl\0D0x10".as_slice(),
    ] {
        let census = lsof_writer_census(output, true, lsof_session_source);
        assert!(census.visibility_gap);
        assert!(census.writers.is_empty(), "{output:?}");
    }
}

#[cfg(unix)]
mod lsof_process {
    use super::*;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    const WRITER: &str =
        "printf 'p101\\000f3\\000aw\\000D0x10\\000i20\\000n/missing/session.jsonl\\000\\n'; ";

    #[test]
    fn lsof_requests_device_identity_and_avoids_blocking_kernel_calls() {
        let command = lsof_command();
        assert_eq!(command.get_program(), "/usr/sbin/lsof");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-b", "-nP", "-F", "pfnaDi0"]
        );
    }

    #[test]
    fn bounded_lsof_drains_stdout_without_stderr_backpressure() -> io::Result<()> {
        let slot = Mutex::new(None);
        let output = capture_lsof_output(
            &mut shell(concat!(
                "i=0; while [ $i -lt 4096 ]; do ",
                "printf 0123456789abcdef0123456789abcdef >&2; i=$((i+1)); done; ",
                "printf 'a\\000b'"
            )),
            &slot,
            Duration::from_secs(5),
            3,
        )?;
        assert!(output.complete);
        assert_eq!(output.stdout, b"a\0b");
        assert!(slot.lock().unwrap().is_none());

        let empty = capture_lsof_output(&mut shell("exit 0"), &slot, Duration::from_secs(5), 0)?;
        assert!(empty.complete);
        assert!(empty.stdout.is_empty());
        Ok(())
    }

    #[test]
    fn bounded_lsof_nonzero_exit_retains_observed_writer_identity() -> io::Result<()> {
        let slot = Mutex::new(None);
        let output = capture_lsof_output(
            &mut shell(&format!("{WRITER}exit 23")),
            &slot,
            Duration::from_secs(5),
            4096,
        )?;
        let census = lsof_writer_census(&output.stdout, output.complete, lsof_session_source);
        assert!(census.visibility_gap);
        assert_eq!(census.writers, HashMap::from([((16, 20), vec![(101, 3)])]));
        assert!(slot.lock().unwrap().is_none());
        Ok(())
    }

    #[test]
    fn bounded_lsof_deadline_covers_silent_children_and_inherited_stdout() -> io::Result<()> {
        for tail in [
            "exec sleep 30",
            "exec 1>&-; exec sleep 30",
            "sleep 30 & exit 0",
        ] {
            let slot = Mutex::new(None);
            let started = Instant::now();
            let output = capture_lsof_output(
                &mut shell(&format!("{WRITER}{tail}")),
                &slot,
                Duration::from_millis(150),
                4096,
            )?;
            assert!(started.elapsed() < Duration::from_secs(3), "{tail}");
            assert!(!output.complete, "{tail}");
            let census = lsof_writer_census(&output.stdout, output.complete, lsof_session_source);
            assert!(census.visibility_gap);
            assert_eq!(census.writers, HashMap::from([((16, 20), vec![(101, 3)])]));
            assert!(
                slot.lock().unwrap().is_none(),
                "direct child was not reaped"
            );
        }
        Ok(())
    }

    #[test]
    fn bounded_lsof_output_limit_stops_flood_and_preserves_complete_prefix() -> io::Result<()> {
        let slot = Mutex::new(None);
        let started = Instant::now();
        let output = capture_lsof_output(
            &mut shell(&format!(
                "{WRITER}while :; do printf 0123456789abcdef; done"
            )),
            &slot,
            Duration::from_secs(30),
            256,
        )?;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(output.stdout.len(), 256);
        assert!(!output.complete);
        let census = lsof_writer_census(&output.stdout, output.complete, lsof_session_source);
        assert!(census.visibility_gap);
        assert_eq!(census.writers, HashMap::from([((16, 20), vec![(101, 3)])]));
        assert!(slot.lock().unwrap().is_none());
        Ok(())
    }

    #[test]
    fn bounded_lsof_slot_does_not_wait_or_spawn_behind_an_existing_child() -> io::Result<()> {
        let slot = Mutex::new(None);
        let mut command = shell("exit 0");
        {
            let _held = slot.lock().unwrap();
            let started = Instant::now();
            let error =
                capture_lsof_output(&mut command, &slot, Duration::from_secs(30), 0).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        let mut pending = shell("exec sleep 30");
        pending
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        *slot.lock().unwrap() = Some(pending.spawn()?);
        let started = Instant::now();
        let result = capture_lsof_output(&mut command, &slot, Duration::from_secs(30), 0);
        let elapsed = started.elapsed();
        // Always terminate the test-owned child before asserting the outcome.
        stop_lsof_child(&mut slot.lock().unwrap(), Duration::from_secs(1));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(elapsed < Duration::from_secs(1));
        assert!(slot.lock().unwrap().is_none());
        assert!(
            capture_lsof_output(&mut command, &slot, Duration::from_secs(5), 0)?.complete,
            "a successfully reaped slot must admit the next census"
        );
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_lsof_identity_matches_metadata_for_a_live_session_writer() -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Keep the small source fixture, consistent with the Linux live tests.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cass-lsof-writer-{}-{nonce}.jsonl",
        std::process::id()
    ));
    let writer = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)?;
    let identity = file_identity(&path)?;
    let census = collect_writers(lsof_session_source);
    assert!(census.writers.contains_key(&identity));
    let probe = SourceWriterProbe {
        writers: census.writers,
    };
    drop(writer);
    assert!(
        probe.should_defer(identity),
        "census omissions cannot revoke a positive"
    );
    Ok(())
}
