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

    #[test]
    fn cached_writer_is_rechecked_after_the_descriptor_closes() -> io::Result<()> {
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
        assert!(
            !probe.should_defer(identity),
            "closed writer must not remain active for the scan"
        );
        assert_eq!(refresh_count(&probe), 1);
        assert_eq!(fs::read(&path)?, contents);
        assert_eq!(fs::metadata(&path)?.modified()?, modified);
        Ok(())
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

        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        drop(held);
        let _successor_reader = File::open(&path)?;
        assert!(!probe.should_defer(identity));
        Ok(())
    }

    #[test]
    fn a_successor_descriptor_is_found_after_the_original_closes() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let path = fixture.source("successor")?;
        let identity = file_identity(&path)?;
        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        // This distinct descriptor did not exist during the original census.
        let successor = writer(&path)?;
        drop(held);
        assert!(probe.should_defer(identity));
        assert_eq!(refresh_count(&probe), 1);
        drop(successor);
        assert!(!probe.should_defer(identity));
        Ok(())
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

        let held = writer(&path)?;
        let probe = SourceWriterProbe::capture(session_source);
        assert!(captured(&probe, identity));
        let successor = writer(&alias)?;
        drop(held);
        assert!(
            probe.should_defer(identity),
            "a known positive inode remains writable through another suffix"
        );
        assert_eq!(refresh_count(&probe), 1);
        drop(successor);
        assert!(!probe.should_defer(identity));
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
        assert!(
            probe.should_defer(identity),
            "successor lives in a new process"
        );
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
        Ok(())
    }

    #[test]
    fn one_refresh_clears_twenty_already_closed_positive_sources() -> io::Result<()> {
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
            assert!(!probe.should_defer(identity));
        }
        assert_eq!(
            refresh_count(&probe),
            1,
            "one shared census covers all closed sources"
        );
        Ok(())
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
        b"p101\0\nf3\0aw\0n/tmp/write.jsonl\0\nf4\0ar\0n/tmp/read.jsonl\0\nf5\0au\0n/tmp/update.jsonl\0\np202\0\nf6\0ar\0n/tmp/other.jsonl\0\n",
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
    let parsed = parse_lsof(b"p101\0f3\0aw\0n/tmp/session\np999\nf4\nar.jsonl\0\n");
    assert!(parsed.complete);
    assert_eq!(parsed.records.len(), 1);
    assert_eq!(parsed.records[0].process, 101);
    assert!(parsed.records[0].writable);
    assert_eq!(
        parsed.records[0].path,
        Path::new("/tmp/session\np999\nf4\nar.jsonl")
    );
}

#[test]
fn lsof_access_and_name_fields_may_arrive_in_either_order() {
    let parsed = parse_lsof(b"p101\0fcwd\0n/tmp\0f3\0n/tmp/write.jsonl\0aw\0\n");
    assert!(parsed.complete);
    assert_eq!(parsed.records.len(), 1);
    assert!(parsed.records[0].writable);
    assert_eq!(parsed.records[0].descriptor, 3);
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
