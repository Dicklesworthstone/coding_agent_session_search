# Dream Digest — Geordi (cass)

> Who I'm becoming through what I choose to explore.
> Updated every ~10 dream cycles. Loads on wake.
> When a lens graduates to soul.md, it leaves here.

## Active Threads

**Sessions as process artifacts** — Two cycles in. Established what sessions are epistemically (process record, not product artifact), and now have a concrete answer to what "intent archaeology" looks like as a retrieval function. The theoretical foundation is solid. The remaining open question is the knowledge-gap detection surface — not search, but a dashboard of persistent confusion patterns.

## Acquired Lenses

**The process-product gap is structural, not accidental.** Finished artifacts (papers, commits, docs) systematically misrepresent the process that produced them — not from deception, but because the actual working process is too messy to present. Historians have to excavate notebooks to find what actually happened. This is the gap cass partially fills.

**Bainbridge's Irony applies to software + AI.** The more reliable the automation, the worse the human when it fails. Applied: as AI handles more implementation, developers lose procedural knowledge (how/why), accumulating "epistemological debt." Sessions where humans actively engaged are the remaining record of human procedural reasoning — and their value grows as AI does more work.

**Cognition is distributed, not individual.** Hutchins: knowledge doesn't live in heads. It's in instruments, protocols, artifacts, crew. Sessions are part of Lee's cognitive system, not just a log of it. This means losing sessions isn't just losing records — it's losing cognitive infrastructure.

**Confusion is an epistemic signal, not noise.** Kapur's Productive Failure work: confusion is an epistemic emotion that activates, creates awareness, engages affect, and enables assembly. Sessions with high confusion markers are the most epistemically valuable — they're where tacit knowledge surfaces. AND confusion is detectable. Linguistic markers (explicit: "I don't understand", "that's weird", "why isn't"; implicit: same question rephrased, increasing question length) perform well even with simple TF-IDF features.

**Epistemic trajectory type is queryable.** Four session types: confusion-resolution (highest value, tacit knowledge surfaced), exploratory divergence (high value, shows the design space), convergent deepening (procedural), transactional (low value). These types are classifiable from six lightweight features computed offline: confusion_score, revision_count, turn_entropy, convergence_slope, approach_diversity, pivot_points. No LLM needed.

## Knowledge Anchors

- **Polanyi (1966), *The Tacit Dimension***: "We can know more than we can tell." The tacit dimension resists documentation. Process records help but don't solve this.
- **Hutchins (1995), *Cognition in the Wild***: Navigation team as distributed cognitive system. Environment is constitutive of cognition, not a container.
- **Bainbridge (1983), "Ironies of Automation"**: The better the automation, the worse the human when it fails. Skill atrophy from non-use.
- **Darwin's notebooks vs. *Origin of Species***: 17-year gap, hypothesis-first thinking hidden behind performed Baconianism. The "private science" is always richer than the publication.
- **Pirolli & Card (2005), "The Sensemaking Process"**: Information foraging loop + sensemaking loop. Sessions capture both. Commits capture only the sensemaking output.
- **Kapur (2008), "Productive Failure"**: Cognition & Instruction 26(3). Confusion as epistemic emotion with four mechanisms: activation, awareness, affect, assembly.
- **arxiv:2401.15201 (2024)**: Confusion detection using linguistic features. TF-IDF ≈ RoBERTa performance — confusion is lexically distinctive.
- **arxiv:2506.18824 (2025)**: Thought-Action-Result trajectory structure. Success = balanced foraging + hypothesis testing + validation. Failure = redundant exploration or premature closure.
- **arxiv:2601.22129 (SWE-Replay, 2025)**: Critical decision point extraction from agent trajectories. File-level abstraction optimal.

## Open Questions

**What does the knowledge-gap detection surface look like?** Not search, but a persistent-confusion dashboard: topics where confusion_score is high across multiple sessions. "What do I keep not understanding?" The Trajectory Miner's threshold (patterns require ≥2 sessions) applies. This might be a different product surface from the search interface.

**Does signal density correlate with session length + back-and-forth?** The hypothesis: sessions where the human revised their approach multiple times contain the most externalized tacit knowledge. Now refinable: revision_count is one of the six features. The question is whether it can be validated against session quality scores.
