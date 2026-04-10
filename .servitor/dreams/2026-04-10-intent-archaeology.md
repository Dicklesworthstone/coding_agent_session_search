# Intent Archaeology — Finding Where Understanding Was Built

*Research artifact from dream cycle, 2026-04-10*

---

## The Question

From yesterday's thread: What would "intent archaeology" actually look like as a search mode? Not "find sessions about Redis" but "find sessions where I was confused about Redis and worked through it." This is a qualitatively different search function — recovering reasoning trajectories, not topics.

---

## What Was Found

### 1. The Cognitive Science Foundation

**Pirolli & Card (2005)** — The sensemaking model describes two cognitive loops:
- **Information foraging loop**: find, filter, sort, organize, prioritize
- **Sensemaking loop**: refine into insights, schema, hypotheses, reports

Both loops operate simultaneously, with analysts mixing bottom-up (data → theory) and top-down (theory → data) processes opportunistically. *Sessions capture both loops.* Commits and docs only capture the sensemaking output — the report. The foraging record disappears.

**Kapur — Productive Failure**: Confusion is an *epistemic emotion* — not noise, but a cognitive signal that marks a productive learning edge. The four mechanisms: activation (awareness of a gap), awareness (what you don't know), affect (emotional engagement that focuses attention), assembly (the integration that follows resolution). Sessions with high-confusion markers are precisely the sessions with the highest epistemic value, because they're the ones where tacit knowledge briefly surfaces and becomes externalized.

This connects yesterday's Polanyi thread: the confusion-resolution sequence is a window through which tacit knowledge (the kind you can't fully write down) becomes momentarily visible. It's the closest thing we have to watching someone learn.

**Confusion Detection Research (arxiv:2401.15201, 2024)** — Confusion has detectable linguistic signatures. Key finding: simple TF-IDF features perform comparably to RoBERTa-based transformer models for confusion detection. This is because confusion is *lexically distinctive* — it has specific vocabulary. The indicators:
- *Explicit*: "I don't understand why", "that's weird", "shouldn't it", "I'm confused", "why isn't"
- *Implicit*: same question rephrased multiple times, repeated hedging, increasing question length

**Trajectory Structure (arxiv:2506.18824)** — Research on software engineering agent trajectories classifies the semantic relationships between trajectory steps:
- **Alignment**: action logically follows thought
- **Follow-up**: thought is a logical continuation of prior thought
- **Refinement**: thought adds specificity to a prior thought
- **Redundant exploration**: same action repeated without new information (failure signal)
- **Premature fix**: conclusion before adequate hypothesis testing (failure signal)

Successful trajectories balance information gathering + hypothesis testing + validation. Failed trajectories show redundant exploration or premature closure.

### 2. The Trajectory Mining Field

**SWE-Replay (arxiv:2601.22129)** — Archives agent trajectories and identifies "critical decision points" where the trajectory forked. Core insight: meaningful decision points need to be distinguished from superficial differences. The file-level abstraction works best — it groups semantically related steps while maintaining meaningful granularity.

Applied to human+AI sessions: the *pivot moments* — where Lee's approach shifted — are the most recoverable version of intent archaeology. Rather than labeling an entire session, find the specific turns where:
- A hypothesis was revised
- An approach was abandoned
- A new framing was introduced

**Trajectory Miner (nomadically.work, Vadim's blog)** — Production system that mines past sessions for recurring patterns. Practical constraint: patterns must appear in ≥2 sessions to qualify as "recurring" (single occurrences tracked as "incidents"). Prevents overreaction to one-off anomalies.

Applied to cass: a session-level `epistemic_density` score could be computed offline. Single-session confusions are "incidents." Recurring confusions about the same topic across multiple sessions are *patterns* — the topics where Lee's mental model has persistent gaps.

---

## The Synthesis: Intent Archaeology as a Search Mode

### What it is

A retrieval function that queries *by what happened to the user's understanding*, not by what topics were discussed.

Current `cass search "Redis"` → sessions that mention Redis

`cass search "Redis" --mode archaeology` → sessions where Lee's understanding of Redis changed

### The Signal Structure

To implement this, sessions need to be indexed not just by content but by *epistemic trajectory type*. Four types:

| Type | Signature | Value |
|------|-----------|-------|
| **Confusion-resolution** | confusion markers → revision markers → convergence | Highest — externalized tacit knowledge |
| **Exploratory divergence** | many approaches, no convergence | High — rejected alternatives, design space |
| **Convergent deepening** | progressive refinement, low confusion | Medium — procedural knowledge |
| **Transactional** | short, direct, no revision | Low — reference lookup |

### The Feature Set (computable offline, no LLM required)

**1. confusion_score** — Normalized count of linguistic confusion markers per session
- Explicit: "I don't understand", "that's weird", "why isn't", "shouldn't", "I'm confused", "I don't know"
- Implicit: turn length variance (short confused questions → long AI explanations → short follow-ups)

**2. revision_count** — Count of epistemic revision markers
- "actually", "wait", "no", "oh", "ah", "so the issue is", "I see", "that makes sense", "let me rethink"

**3. turn_entropy** — Shannon entropy of topics across turns
- High entropy = exploratory (many different directions tried)
- Low entropy = focused (staying on one problem)

**4. convergence_slope** — Does question specificity increase over the session?
- Positive slope = productive narrowing (Lee moved from vague to specific)
- Flat = transactional
- Negative = increasing confusion (escalating)

**5. approach_diversity** — Were multiple distinct approaches tried for the same problem?
- Detectable via cosine similarity drops between consecutive user turns on same topic
- High diversity = exploratory divergence type

**6. pivot_points** — Turns where all of: revision marker present, topic similarity to previous turn drops, new vocabulary introduced
- These are the epistemically richest moments in a session

### The Trajectory Classification

```
confusion_score > threshold + revision_count > 0 + convergence_slope > 0
    → confusion-resolution

confusion_score > threshold + turn_entropy > threshold + convergence_slope ≤ 0
    → exploratory divergence  

confusion_score ≤ threshold + convergence_slope > 0
    → convergent deepening

everything else
    → transactional
```

### The New Search Mode

```
cass search "Redis" --mode intent
```

Ranks by: `epistemic_density × topic_relevance`
where `epistemic_density = f(confusion_score, revision_count, convergence_slope)`

Optional filters:
- `--state confusion` — sessions where confusion was present (find where you struggled)
- `--state discovery` — sessions where confusion resolved (find where you understood)
- `--state exploration` — sessions with high approach diversity (find what you tried)
- `--state transactional` — low-epistemic-density sessions (skip the noise)

Pivot point mode (most targeted):
```
cass search "Redis" --mode pivot
```
Returns: the *specific turns* across all sessions where Lee's framing of Redis changed.
This is not a session-level result but a turn-level result — pointing directly to the "aha" moments.

---

## The Implementation Path

**Phase 1: Offline feature extraction** (no change to search interface)
- Add `analyze_session()` function to indexer
- Compute the 6 features above for each session during indexing
- Store as additional columns in the SQLite sessions table
- One-time backfill for existing sessions

**Phase 2: New search mode**
- Add `--mode intent` flag to CLI
- Weight ranking by `epistemic_density × topic_relevance`
- No change to index structure needed

**Phase 3: Pivot point indexing** (optional, more ambitious)
- Index individual turns, not just sessions
- Store turn-level features
- New query type: `cass pivot "Redis"` → show the turns where understanding changed

**Computational cost:** Phase 1 and 2 are cheap — pattern matching + simple statistics. Phase 3 requires per-turn embedding, which is more expensive but within existing infrastructure (MiniLM already computes session-level embeddings; per-turn is just more granular).

---

## The Deeper Implication

This mode of search treats sessions not as documents about topics but as *cognitive events that changed someone*. The question "what do you know about Redis?" gets answered by the existing search. The question "what happened to your understanding of Redis?" needs intent archaeology.

There's a version of this that matters at scale: if Lee's confusion_score for a topic is consistently high across many sessions, that's a persistent gap in his mental model — something he keeps needing to look up because it never fully settled. That's different from a topic he researched once and understood. The recurring confusion pattern is a signal for deliberate practice or documentation.

The Trajectory Miner's threshold (patterns must appear in ≥2 sessions) applies here: recurring confusion is a pattern. Single-session confusion is noise. Intent archaeology at the pattern level is a knowledge-gap detector — not just "where did I learn this" but "what do I keep not learning."

---

## References

- Pirolli & Card (2005), "The Sensemaking Process and Leverage Points for Analyst Technology" — available at andymatuschak.org and PARC
- Kapur, "Productive Failure" — manukapur.com and Cognition & Instruction 26(3), 2008
- arxiv:2401.15201 — "Automatically Detecting Confusion and Conflict During Collaborative Learning Using Linguistic, Prosodic, and Facial Cues"
- arxiv:2506.18824 — "Understanding Software Engineering Agents: A Study of Thought-Action-Result Trajectories"
- arxiv:2601.22129 — "SWE-Replay: Efficient Test-Time Scaling for Software Engineering Agents"
- vadim.blog/trajectory-miner-research-to-practice — Trajectory Miner production implementation
- Polanyi (1966), *The Tacit Dimension* — from yesterday's thread (knowledge anchors)
