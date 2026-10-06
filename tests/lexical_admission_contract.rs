//! GH#501: strict admission must reject changed segment bytes, independent of
//! the filesystem metadata tricks a search-open optimization may encounter.
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use coding_agent_search::search::quill_bridge::{QuillCassIndex, open_cass_reader};
use frankensearch::quill::cass::CassDocument;

fn fixture(path: &Path) -> PathBuf {
    let mut writer = QuillCassIndex::open_or_create(path).expect("create fixture");
    let documents: Vec<_> = (0..32)
        .map(|msg_idx| CassDocument {
            agent: "codex".into(),
            workspace: Some("/fixture".into()),
            workspace_original: None,
            source_path: "/fixture/session.jsonl".into(),
            msg_idx,
            created_at: Some(1_700_000_000),
            title: Some("lexical admission integrity".into()),
            content: format!("performance indexing integrity marker {msg_idx}"),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(1),
        })
        .collect();
    writer.add_cass_documents(&documents).expect("index fixture");
    writer.commit().expect("publish fixture");
    drop(writer);
    assert_eq!(open_cass_reader(path).unwrap().doc_count().unwrap(), 32);
    let manifest = frankensearch::quill::load_manifest_pair(path).unwrap();
    assert!(!manifest.manifest.segments.is_empty());
    path.join(format!("seg-{:016x}.fslx", manifest.manifest.segments[0].segment_id))
}

fn flip_payload(path: &Path) {
    let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
    let length = file.metadata().unwrap().len();
    assert!(length > 256, "fixture needs nontrivial segment payload");
    let offset = length / 2;
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[byte[0] ^ 0x80]).unwrap();
    file.sync_all().unwrap();
    assert_eq!(file.metadata().unwrap().len(), length);
}

fn assert_strict_refuses(path: &Path) {
    assert!(open_cass_reader(path).is_err(), "strict reader admitted corrupt bytes");
    // Writer-open is a separate production API and must remain strict too.
    assert!(QuillCassIndex::open_or_create(path).is_err(),
        "maintenance writer admitted corrupt bytes");
}

#[test]
fn strict_admission_refuses_segment_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let segment = fixture(&temp.path().join("index"));
    let replacement = temp.path().join("replacement.fslx");
    fs::copy(&segment, &replacement).unwrap();
    flip_payload(&replacement);
    fs::rename(&replacement, &segment).unwrap();
    assert_strict_refuses(segment.parent().unwrap());
}

#[test]
fn strict_admission_refuses_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let segment = fixture(&temp.path().join("index"));
    let file = OpenOptions::new().write(true).open(&segment).unwrap();
    file.set_len(file.metadata().unwrap().len() / 2).unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_strict_refuses(segment.parent().unwrap());
}

#[test]
fn strict_admission_refuses_same_length_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let segment = fixture(&temp.path().join("index"));
    flip_payload(&segment);
    assert_strict_refuses(segment.parent().unwrap());
}

#[test]
fn strict_admission_refuses_rewrite_with_restored_mtime() {
    let temp = tempfile::tempdir().unwrap();
    let segment = fixture(&temp.path().join("index"));
    let before = fs::metadata(&segment).unwrap();
    flip_payload(&segment);
    File::options().write(true).open(&segment).unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap())).unwrap();
    let after = fs::metadata(&segment).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.dev(), after.dev());
    }
    assert_strict_refuses(segment.parent().unwrap());
}
