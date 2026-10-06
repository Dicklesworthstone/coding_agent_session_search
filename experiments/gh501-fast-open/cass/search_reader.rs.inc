/// Explicit opt-in to metadata-based read-only admission. The normal reader,
/// writer, doctor and indexing configuration remains strict.
pub const CASS_LEXICAL_VERIFY_ENV: &str = "CASS_LEXICAL_VERIFY";
pub const CASS_LEXICAL_RECEIPT_DIR_ENV: &str = "CASS_LEXICAL_RECEIPT_DIR";

fn lexical_receipt_directory(
    mode: Option<&str>, directory: Option<std::ffi::OsString>, index: &Path,
) -> Result<Option<PathBuf>> {
    match mode.unwrap_or("full") {
        "full" => Ok(None),
        "local-receipts" => {
            let directory = directory.filter(|value| !value.is_empty())
                .map(PathBuf::from).ok_or_else(|| anyhow!(
                    "CASS_LEXICAL_VERIFY=local-receipts requires CASS_LEXICAL_RECEIPT_DIR"))?;
            if !directory.is_absolute() {
                return Err(anyhow!("CASS_LEXICAL_RECEIPT_DIR must be an absolute path"));
            }
            // Do not permit the advisory cache to change the index directory.
            // An absent/unreadable cache is an ordinary full-verification miss.
            if let (Ok(index), Ok(cache)) = (index.canonicalize(), directory.canonicalize())
                && cache.starts_with(index) {
                return Err(anyhow!("CASS_LEXICAL_RECEIPT_DIR must be outside the index"));
            }
            Ok(Some(directory))
        }
        _ => Err(anyhow!("CASS_LEXICAL_VERIFY must be full or local-receipts")),
    }
}

// Use the same dotenvy configuration layer as the existing CASS bridge.
fn lexical_receipt_environment(name: &str) -> Result<Option<String>> {
    match dotenvy::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Ok(None),
        Err(error) => Err(anyhow!("reading {name}: {error}")),
    }
}

/// Search-only admission. Sensitive callers continue to use open_cass_reader.
/// Setting CASS_LEXICAL_VERIFY=full is an unconditional full-verify escape hatch.
/// This does not turn the existing cass_quill_config shared by writers into an
/// environment-dependent integrity policy.
pub fn open_cass_search_reader(path: &Path) -> Result<QuillSearchIndex> {
    let mode = lexical_receipt_environment(CASS_LEXICAL_VERIFY_ENV)?;
    // Full verification does not even consult the cache setting, so an invalid
    // cache value cannot prevent the unconditional strict override.
    let cache = if mode.as_deref() == Some("local-receipts") {
        lexical_receipt_environment(CASS_LEXICAL_RECEIPT_DIR_ENV)?.map(Into::into)
    } else {
        None
    };
    let directory = lexical_receipt_directory(mode.as_deref(), cache, path)?;
    let Some(directory) = directory else { return open_cass_reader(path); };
    #[cfg(test)]
    READER_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let mut config = cass_quill_config();
    config.read_open_receipt_directory = Some(directory);
    drive(|cx| {
        let path = path.to_path_buf();
        async move {
            QuillSearchIndex::open_with_schema(&cx, path, CASS_SEMANTIC_SCHEMA, config).await
        }
    }).map_err(|error| anyhow!("opening Quill CASS search reader at {}: {error}", path.display()))
}

#[cfg(test)]
mod lexical_receipt_policy_tests {
    use super::*;
    #[test]
    fn full_is_default_and_overrides_an_unusable_cache() {
        let index = Path::new("unused");
        assert!(lexical_receipt_directory(None, None, index).unwrap().is_none());
        assert!(lexical_receipt_directory(Some("full"), Some("bad-relative".into()), index).unwrap().is_none());
        assert!(cass_quill_config().read_open_receipt_directory.is_none());
    }
    #[test]
    fn misspelled_policy_cannot_silently_disable_integrity() {
        for mode in ["", "FULL", "auto", "none", "skip", "local_receipts"] {
            assert!(lexical_receipt_directory(Some(mode), None, Path::new("unused")).is_err());
        }
    }
    #[test]
    fn receipt_cache_must_be_explicit_and_outside_index() {
        let root = tempfile::tempdir().unwrap();
        let index = root.path().join("index"); std::fs::create_dir(&index).unwrap();
        let nested = index.join("cache"); std::fs::create_dir(&nested).unwrap();
        let external = root.path().join("cache"); std::fs::create_dir(&external).unwrap();
        assert!(lexical_receipt_directory(Some("local-receipts"), None, &index).is_err());
        assert!(lexical_receipt_directory(Some("local-receipts"), Some("relative".into()), &index).is_err());
        assert!(lexical_receipt_directory(Some("local-receipts"), Some(nested.into_os_string()), &index).is_err());
        assert_eq!(lexical_receipt_directory(Some("local-receipts"), Some(external.clone().into_os_string()), &index).unwrap(), Some(external));
    }
}
