//! Source occurrence provenance for the existing primary/enrichment passes.
//!
//! Locate the pinned parser's existing rows in the bounded source; never
//! synthesize primary messages. Exact raw evidence is preferred when present.
//! Compact extras use the same primary identity tuple, one occurrence at a
//! time, after accounting for FAD's precise adjacent user-prompt pairing.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufWriter, Write};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    ModernCodexMessageSignature, NormalizedMessage, modern_codex_message_signature,
    parse_timestamp, reindex_messages,
};

#[derive(Default)]
pub(super) struct Observation {
    pub(super) index: Option<usize>,
    pub(super) primary: bool,
}

pub(super) struct SourceOrder {
    signature_writer: BufWriter<blake3::Hasher>,
    raw: HashMap<[u8; 32], VecDeque<usize>>,
    primary: HashMap<ModernCodexMessageSignature, VecDeque<usize>>,
    events: HashMap<ModernCodexMessageSignature, VecDeque<usize>>,
    raw_evidence: Vec<Option<[u8; 32]>>,
    consumed: Vec<bool>,
    positions: Vec<Option<usize>>,
    pending_prompt: Option<PendingPrompt>,
    native_count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptStream {
    Event,
    Response,
}

struct PromptIdentity {
    stream: PromptStream,
    trimmed_hash: [u8; 32],
    trimmed_bytes: usize,
    turn_id: Option<String>,
}

struct NativeRecord {
    signature: ModernCodexMessageSignature,
    prompt: Option<PromptIdentity>,
}

struct PendingPrompt {
    identity: PromptIdentity,
    signature: ModernCodexMessageSignature,
    raw: [u8; 32],
    physical: usize,
    message: usize,
    ordinal: usize,
}

impl PendingPrompt {
    fn pairs_with(&self, next: &Self) -> bool {
        let times_match = match (self.signature.created_at, next.signature.created_at) {
            (Some(left), Some(right)) => left.abs_diff(right) <= 1_000,
            (None, None) => true,
            _ => false,
        };
        self.identity.stream != next.identity.stream
            && self.physical.checked_add(1) == Some(next.physical)
            && self.message.checked_add(1) == Some(next.message)
            && self.identity.trimmed_bytes == next.identity.trimmed_bytes
            && self.identity.trimmed_hash == next.identity.trimmed_hash
            && self.identity.turn_id == next.identity.turn_id
            && times_match
    }
}

impl SourceOrder {
    pub(super) fn new(messages: &[NormalizedMessage]) -> Result<Self> {
        let mut signature_writer = BufWriter::new(blake3::Hasher::new());
        let mut raw: HashMap<_, VecDeque<_>> = HashMap::new();
        let mut primary: HashMap<_, VecDeque<_>> = HashMap::new();
        let mut raw_evidence = Vec::with_capacity(messages.len());
        for (index, message) in messages.iter().enumerate() {
            // An empty compacted extra is not the provenance of a literal {}
            // record. Only actual envelopes supply exact raw evidence.
            let evidence = if message.extra.get("type").and_then(Value::as_str).is_some() {
                let digest = raw_provenance(&mut signature_writer, &message.extra)?;
                raw.entry(digest).or_default().push_back(index);
                Some(digest)
            } else {
                None
            };
            raw_evidence.push(evidence);
            primary
                .entry(modern_codex_message_signature(message))
                .or_default()
                .push_back(index);
        }
        Ok(Self {
            signature_writer,
            raw,
            events: primary.clone(),
            primary,
            raw_evidence,
            consumed: vec![false; messages.len()],
            positions: vec![None; messages.len()],
            pending_prompt: None,
            native_count: 0,
        })
    }

    pub(super) fn observe(
        &mut self,
        raw: &Value,
        ordinal: usize,
        physical: usize,
    ) -> Result<Observation> {
        let Some(native) = primary_record(raw) else {
            // Enrichment-only events do not increment FAD's native message
            // counter or clear its pending prompt. This matters for multiple
            // recovered records sharing one physical line. Avoid serializing
            // ignored/encrypted fields merely to order them. Any unclassified
            // primary row still fails the complete-consumption check below.
            return Ok(Observation::default());
        };
        let digest = raw_provenance(&mut self.signature_writer, raw)?;
        let message = self.native_count;
        self.native_count += 1;
        if let Some(identity) = native.prompt {
            let prompt = PendingPrompt {
                identity,
                signature: native.signature,
                raw: digest,
                physical,
                message,
                ordinal,
            };
            if let Some(previous) = self.pending_prompt.take() {
                if previous.pairs_with(&prompt) {
                    // FAD retains the response side in either order. The
                    // discarded event must never consume a compacted row,
                    // especially before a later identical response record.
                    let index = if prompt.identity.stream == PromptStream::Response {
                        self.assign(&prompt.signature, prompt.raw, prompt.ordinal)
                    } else {
                        None
                    };
                    return Ok(Observation { index, primary: true });
                }
                self.finish_prompt(previous);
            }
            let index = if prompt.identity.stream == PromptStream::Response {
                // A response is always retained, even if the next event
                // pairs with it, so its occurrence can be assigned now.
                self.assign(&prompt.signature, prompt.raw, prompt.ordinal)
            } else {
                None
            };
            self.pending_prompt = Some(prompt);
            return Ok(Observation { index, primary: true });
        }

        // Any other native message makes FAD's message-index adjacency
        // impossible for the previous prompt; resolve it before this row.
        self.flush_prompt();
        Ok(Observation {
            index: self.assign(&native.signature, digest, ordinal),
            primary: true,
        })
    }

    fn finish_prompt(&mut self, prompt: PendingPrompt) {
        if prompt.identity.stream == PromptStream::Event {
            self.assign(&prompt.signature, prompt.raw, prompt.ordinal);
        }
    }

    fn flush_prompt(&mut self) {
        if let Some(prompt) = self.pending_prompt.take() {
            self.finish_prompt(prompt);
        }
    }

    fn assign(
        &mut self,
        signature: &ModernCodexMessageSignature,
        digest: [u8; 32],
        ordinal: usize,
    ) -> Option<usize> {
        // Prefer the original primary identity. A prior enrichment pass may
        // have retained a labeled display row with the same raw envelope as
        // another exact native output; the label must not steal that output.
        let candidate = self.primary.get_mut(signature).and_then(|queue| {
            while queue.front().is_some_and(|index| self.consumed[*index]) {
                queue.pop_front();
            }
            let index = *queue.front()?;
            if self.raw_evidence[index].is_some_and(|proof| proof != digest) {
                // Known, different raw provenance cannot be replaced by a
                // broad content/time match. A later exact raw row may match.
                return None;
            }
            queue.pop_front()
        });
        let index = candidate.or_else(|| self.take_raw(digest))?;
        self.consumed[index] = true;
        self.note(index, ordinal);
        Some(index)
    }

    fn take_raw(&mut self, digest: [u8; 32]) -> Option<usize> {
        let queue = self.raw.get_mut(&digest)?;
        while let Some(index) = queue.pop_front() {
            if !self.consumed[index] {
                self.consumed[index] = true;
                return Some(index);
            }
        }
        None
    }

    pub(super) fn match_event(&mut self, message: &NormalizedMessage) -> Option<usize> {
        // Retain the existing primary/event equivalence predicate, but only
        // one event-side occurrence per initial primary row. Newly enriched
        // rows never replenish this inventory or erase same-stream repeats.
        self.events
            .get_mut(&modern_codex_message_signature(message))?
            .pop_front()
    }

    pub(super) fn note(&mut self, index: usize, ordinal: usize) {
        self.positions[index] =
            Some(self.positions[index].map_or(ordinal, |old| old.min(ordinal)));
    }

    pub(super) fn push(&mut self, ordinal: usize) {
        self.positions.push(Some(ordinal));
    }

    pub(super) fn finish(mut self, messages: &mut [NormalizedMessage]) -> Result<()> {
        self.flush_prompt();
        anyhow::ensure!(
            self.consumed.iter().all(|matched| *matched),
            "Codex primary message has no classified source occurrence"
        );
        anyhow::ensure!(
            messages.len() == self.positions.len(),
            "Codex enrichment lost occurrence provenance"
        );
        for (message, position) in messages.iter_mut().zip(self.positions) {
            message.idx = i64::try_from(
                position.context("Codex primary message has no source occurrence")?,
            )?;
        }
        // Stable sorting retains both rows when a single physical record has
        // both an existing primary and an enrichment representation.
        messages.sort_by_key(|message| message.idx);
        reindex_messages(messages);
        Ok(())
    }
}

pub(super) fn is_enrichment_event(raw: &Value) -> bool {
    raw.get("type").and_then(Value::as_str) == Some("event_msg")
        && matches!(
            raw.pointer("/payload/type").and_then(Value::as_str),
            Some("agent_message" | "tool_result")
        )
}

/// Within one physical line, a following native message closes FAD's pending
/// event's message adjacency; ignored/enrichment-only records do not. Recovered
/// records must therefore be examined in native-emission order, not just by
/// inspecting the final JSON object.
pub(crate) fn physical_line_ends_in_user_event(records: &[Value]) -> bool {
    records
        .iter()
        .rev()
        .find_map(primary_record)
        .and_then(|native| native.prompt)
        .is_some_and(|prompt| prompt.stream == PromptStream::Event)
}

fn flatten(value: &Value) -> Cow<'_, str> {
    value.as_str().map_or_else(
        || Cow::Owned(franken_agent_detection::flatten_content(value)),
        Cow::Borrowed,
    )
}

/// Match the actual pinned FAD tool-output flattening, including structured
/// Responses-API output. Serializing an object as JSON would be a different
/// primary identity after raw-extra compaction.
fn tool_output(payload: &Value) -> Option<Cow<'_, str>> {
    let output = payload.get("output")?;
    if let Some(text) = output.as_str() {
        return Some(Cow::Borrowed(text));
    }
    if let Some(content) = output.get("content") {
        let flattened = flatten(content);
        if !flattened.trim().is_empty() {
            return Some(flattened);
        }
    }
    Some(flatten(output))
}

/// Fingerprint only the identity fields the pinned FAD parser emits, before
/// CASS enriches tool text. This locates existing rows; it does not construct
/// or replace a primary message. Emission conditions and the default "agent"
/// role deliberately match the pinned parser, including legacy envelopes.
fn primary_record(raw: &Value) -> Option<NativeRecord> {
    let payload = raw.get("payload")?;
    let (role, author, content, stream, trim_empty) =
        match raw.get("type").and_then(Value::as_str)? {
            "response_item" => match payload.get("type").and_then(Value::as_str) {
                Some("function_call" | "custom_tool_call") => (
                    "assistant",
                    None,
                    Cow::Owned(format!(
                        "[Tool: {}]",
                        payload.get("name").and_then(Value::as_str).unwrap_or("unknown")
                    )),
                    None,
                    false,
                ),
                Some("function_call_output" | "custom_tool_call_output") => {
                    ("tool", None, tool_output(payload)?, None, true)
                }
                _ => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("agent");
                    (
                        role,
                        None,
                        flatten(payload.get("content")?),
                        (role == "user").then_some(PromptStream::Response),
                        true,
                    )
                }
            },
            "event_msg" => match payload.get("type").and_then(Value::as_str)? {
                "user_message" => (
                    "user",
                    None,
                    Cow::Borrowed(payload.get("message")?.as_str()?),
                    Some(PromptStream::Event),
                    false,
                ),
                "agent_reasoning" => (
                    "assistant",
                    Some("reasoning"),
                    Cow::Borrowed(payload.get("text")?.as_str()?),
                    None,
                    false,
                ),
                "tool_call" => (
                    "assistant",
                    None,
                    Cow::Owned(format!(
                        "[Tool: {}]",
                        payload.get("name").and_then(Value::as_str).unwrap_or("unknown")
                    )),
                    None,
                    false,
                ),
                _ => return None,
            },
            _ => return None,
        };
    if if trim_empty { content.trim().is_empty() } else { content.is_empty() } {
        return None;
    }
    let prompt = stream.map(|stream| {
        let text = content.trim();
        PromptIdentity {
            stream,
            trimmed_hash: *blake3::hash(text.as_bytes()).as_bytes(),
            trimmed_bytes: text.len(),
            turn_id: payload
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        }
    });
    Some(NativeRecord {
        signature: ModernCodexMessageSignature {
            role: role.to_owned(),
            author: author.map(str::to_owned),
            created_at: raw.get("timestamp").and_then(parse_timestamp),
            content_hash: *blake3::hash(content.as_bytes()).as_bytes(),
        },
        prompt,
    })
}

/// FAD adds token-usage annotations under the top-level "cass" member of the
/// raw extra. Exclude only that producer-owned annotation from occurrence
/// matching; all source bytes remain covered by the independent byte receipt.
fn raw_provenance(
    writer: &mut BufWriter<blake3::Hasher>,
    raw: &Value,
) -> Result<[u8; 32]> {
    if let Some(object) = raw.as_object() {
        let fields: BTreeMap<_, _> =
            object.iter().filter(|(key, _)| key.as_str() != "cass").collect();
        serde_json::to_writer(&mut *writer, &fields)?;
    } else {
        serde_json::to_writer(&mut *writer, raw)?;
    }
    writer.flush()?;
    let hasher = writer.get_mut();
    let digest = *hasher.finalize().as_bytes();
    hasher.reset();
    Ok(digest)
}
