//! GH426: real CLI interruption and exact-source resume over generated Claude logs.
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value,json};
use std::{fs,path::Path,process::{Child,Command,Stdio},time::{Duration,Instant}};

fn seed(home:&Path,count:usize) {
    let root=home.join(".claude/projects/resume");fs::create_dir_all(&root).unwrap();
    for source in 0..count {
        let rows=(0..32).map(|message|json!({"type":"user","sessionId":format!("resume-{source}"),
            "uuid":format!("resume-{source}-{message}"),"timestamp":"2026-08-01T10:00:00Z","cwd":"/work/resume",
            "message":{"role":"user","content":format!("resumeproof{source} message {message}")}}).to_string())
            .collect::<Vec<_>>().join("\n");
        fs::write(root.join(format!("session-{source}.jsonl")),format!("{rows}\n")).unwrap();
    }
}

fn command(home:&Path,streaming:&str)->Command {
    let mut command=Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear().env("HOME",home).env("USERPROFILE",home).env("PATH","/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR",home.join(".claude"))
        .env("XDG_CONFIG_HOME",home.join(".config")).env("XDG_DATA_HOME",home.join(".local/share"))
        .env("CASS_DATA_DIR",home.join("data")).env("CASS_IGNORE_SOURCES_CONFIG","1")
        .env("CASS_STREAMING_INDEX",streaming).env("CASS_AUTO_REFRESH","0")
        .env("RUST_MIN_STACK","134217728").env("RUST_LOG","coding_agent_search::indexer=debug")
        .env("NO_COLOR","1").current_dir(home).args(["--color","never"]);
    command
}

fn verify_archive(home:&Path,count:usize) {
    let storage=SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    let conversations=storage.list_conversations(1000,0).unwrap();
    assert_eq!(conversations.len(),count);
    for conversation in conversations {
        assert_eq!(storage.fetch_messages(conversation.id.unwrap()).unwrap().len(),32);
    }
    drop(storage);
    for source in 0..count {
        let output=assert_cmd::Command::from_std(command(home,"1"))
            .args(["search",&format!("resumeproof{source}"),"--mode","lexical","--json","--no-maintenance","--limit","100"])
            .timeout(Duration::from_secs(30)).assert().success().get_output().stdout.clone();
        let result:Value=serde_json::from_slice(&output).unwrap();
        assert!(!result["hits"].as_array().unwrap().is_empty(),"lexical gap for source {source}");
        assert_ne!(result.pointer("/budget/timed_out").and_then(Value::as_bool),Some(true));
    }
}

#[test]
fn gh426_bounded_stop_resumes_only_uncommitted_sources_in_both_modes() {
    for streaming in ["0","1"] {
        let home=tempfile::tempdir().unwrap();seed(home.path(),8);
        let stopped=assert_cmd::Command::from_std(command(home.path(),streaming))
            .env("CASS_INDEX_MAX_SOURCE_COMMITS","2").args(["index","--json"])
            .timeout(Duration::from_secs(120)).assert().failure().get_output().clone();
        assert!(String::from_utf8_lossy(&stopped.stderr).contains("interrupted"));
        let storage=SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
        assert_eq!(storage.source_ingest_ledger_entries().unwrap().len(),2);
        assert_eq!(storage.list_conversations(100,0).unwrap().len(),2);
        drop(storage);
        let resumed=assert_cmd::Command::from_std(command(home.path(),streaming))
            .args(["index","--json"]).timeout(Duration::from_secs(180)).assert().success().get_output().clone();
        let log=String::from_utf8_lossy(&resumed.stderr);
        assert_eq!(log.lines().filter(|line|line.contains("source_ingest_observation") && line.contains("skipped=true")).count(),2,"{log}");
        assert_eq!(log.lines().filter(|line|line.contains("source_ingest_observation") && line.contains("skipped=false")).count(),6,"{log}");
        verify_archive(home.path(),8);
        // A changed completed source must parse again, even with timestamps
        // inside the old provider history. Replaying identical rows stays idempotent.
        let changed=home.path().join(".claude/projects/resume/session-0.jsonl");
        let mut rows=fs::read_to_string(&changed).unwrap();rows.push('\n');
        fs::write(&changed,rows).unwrap();
        let replay=assert_cmd::Command::from_std(command(home.path(),streaming))
            .args(["index","--json"]).timeout(Duration::from_secs(180)).assert().success().get_output().clone();
        let log=String::from_utf8_lossy(&replay.stderr);
        assert_eq!(log.lines().filter(|line|line.contains("source_ingest_observation") && line.contains("skipped=false")).count(),1,"{log}");
        verify_archive(home.path(),8);
    }
}

#[cfg(unix)]
#[test]
fn gh426_sigterm_and_sigint_stop_at_commit_boundary_and_resume() {
    struct Guard(Child);
    impl Drop for Guard {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    for (signal,exit) in [("-TERM",143),("-INT",130)] {
        let home=tempfile::tempdir().unwrap();seed(home.path(),80);
        let stdout=home.path().join("stdout");let stderr=home.path().join("stderr");
        let mut child=Guard(command(home.path(),"1").args(["index","--json"])
            .stdout(Stdio::from(fs::File::create(stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap())).spawn().unwrap());
        let deadline=Instant::now()+Duration::from_secs(120);
        while !fs::read_to_string(&stderr).unwrap().contains("source_ingest_committed") {
            assert!(child.0.try_wait().unwrap().is_none(),"index exited before signal");
            assert!(Instant::now()<deadline,"no source commit before signal");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(Command::new("kill").args([signal,&child.0.id().to_string()]).status().unwrap().success());
        let status=loop {
            if let Some(status)=child.0.try_wait().unwrap(){break status;}
            assert!(Instant::now()<deadline,"graceful signal shutdown timed out: {}",fs::read_to_string(&stderr).unwrap());
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.code(),Some(exit),"{}",fs::read_to_string(&stderr).unwrap());
        assert_cmd::Command::from_std(command(home.path(),"1")).args(["index","--json"])
            .timeout(Duration::from_secs(240)).assert().success();
        verify_archive(home.path(),80);
    }
}
