//! Connector for Grok Build (xAI's official `grok` coding CLI) sessions.
//!
//! Implementation lives in `franken_agent_detection::connectors::grok`.
//! Layout: `$GROK_HOME/sessions/<percent-encoded-cwd>/<session-uuid>/` with
//! `updates.jsonl` (authoritative ACP session-update stream), `summary.json`
//! (metadata), and `chat_history.jsonl` (raw model history, fallback).

pub use franken_agent_detection::GrokConnector;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
};
use anyhow::{Result, bail};

/// CASS indexes Grok Bot chat locally; mixed replicas are never remote inputs.
#[derive(Default)]
pub struct GrokBotConnector {
    inner: franken_agent_detection::GrokBotConnector,
}

impl GrokBotConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn local_sources(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        // Discovery only examines names/stat metadata. Refuse a matched remote
        // replica before FAD opens its mixed chat/secret JSON container. Other
        // providers' remote roots remain ordinary nonmatches.
        let sources = self.inner.discover_source_files(ctx)?;
        if sources.iter().any(|source| source.origin.is_remote()) {
            bail!(
                "Grok Bot remote replicas are disabled because they contain non-chat secrets and approval payloads; index the original local store with CASS_GROK_BOT_DATA_ROOT"
            );
        }
        Ok(sources)
    }
}

impl Connector for GrokBotConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.local_sources(ctx)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(
            ctx,
            &mut franken_agent_detection::connectors::SourceScanHooks::default(),
            on_conversation,
        )
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::connectors::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        if self.local_sources(ctx)?.is_empty() {
            return Ok(());
        }
        let mut local = ctx.clone();
        local.scan_roots.retain(|root| !root.origin.is_remote());
        // A remote replica appearing after admission cannot enter the second
        // discovery pass; an all-remote nonmatch returned above, so retaining
        // zero roots here cannot accidentally enable local default discovery.
        self.inner
            .scan_with_source_boundaries(&local, hooks, on_conversation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{Origin, ScanRoot};
    use franken_agent_detection::connectors::SourceScanHooks;

    #[test]
    fn gh447_grok_bot_local_chat_and_remote_refusal_preserve_grok_cli_identity() {
        let temp = tempfile::tempdir().unwrap();
        let key = "sand.client.slice.account.auth0%7Cuser_00000000000000000000000000.transcript.replicas.81da155f-cc4c-58b3-aadf-770858d5e55c";
        let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
        let bits: Vec<_> = key
            .bytes()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .collect();
        let encoded: String = bits
            .chunks(5)
            .map(|chunk| {
                let value = chunk.iter().fold(0_u8, |v, bit| (v << 1) | *bit) << (5 - chunk.len());
                char::from(alphabet[usize::from(value)])
            })
            .collect();
        let path = temp.path().join(format!("{encoded}.blob"));
        let bytes = br#"{"schemaVersion":1,"value":{"entries":[{"kind":"send-message","id":"native-1","message":{"type":"text","content":"local chat"}},{"kind":"secret-request","id":"private-1","content":"excluded secret"}]}}"#;
        std::fs::write(&path, bytes).unwrap();
        let local = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::local(path.clone())],
            None,
        );
        let connector = GrokBotConnector::new();
        let conversations = connector.scan(&local).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].agent_slug, "grok_bot");
        assert_eq!(conversations[0].messages.len(), 1);
        assert_eq!(conversations[0].messages[0].content, "local chat");
        assert!(
            !serde_json::to_string(&conversations)
                .unwrap()
                .contains("excluded secret")
        );
        let remote = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::remote(
                path.clone(),
                Origin::remote("laptop"),
                None,
            )],
            None,
        );
        assert!(connector.discover_source_files(&remote).is_err());
        assert!(connector.scan(&remote).is_err());
        let mut callbacks = 0;
        assert!(
            connector
                .scan_with_callback(&remote, &mut |_| {
                    callbacks += 1;
                    Ok(())
                })
                .is_err()
        );
        assert!(
            connector
                .scan_with_source_boundaries(&remote, &mut SourceScanHooks::default(), &mut |_| {
                    callbacks += 1;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(callbacks, 0);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let unrelated = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::remote(
                temp.path().join("codex"),
                Origin::remote("laptop"),
                None,
            )],
            None,
        );
        assert!(connector.scan(&unrelated).unwrap().is_empty());
        assert!(
            GrokConnector::new().scan(&local).unwrap().is_empty(),
            "Grok CLI must not claim Grok Bot replicas"
        );
        let factory = crate::connectors::get_connector_factories()
            .into_iter()
            .find(|(slug, _)| *slug == "grok_bot")
            .unwrap()
            .1;
        assert!(
            factory().scan(&remote).is_err(),
            "runtime factory must apply the CASS boundary"
        );
    }
}
