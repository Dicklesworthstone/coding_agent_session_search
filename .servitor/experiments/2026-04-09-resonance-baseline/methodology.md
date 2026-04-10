# Methodology — Mechanical Baselines

**Status:** stub. Instruments to be implemented sequentially as scope is approved. Updated 2026-04-10 to add Instruments 4 and 5 after a second wave of voyages landed on the fleet thread.

## Binding Constraint (added 2026-04-10)

**The Wellisch–GC-O constraint applies to every output.** Mechanical outputs (similarity matrices, ranked lists, constellation reports) must point readers back into the source corpus rather than substitute for reading it. A "top resonant pair" output is acceptable only if its consumption requires the reader to open the source artifacts to act on it. If a future version of this experiment produces a standalone summary that a reader could act on without reading the dreams themselves, the experiment has failed the constraint and the output is killed. This is the product boundary cass defends.

## Instrument 1 — MiniLM Content Embeddings (baseline)

The existing `cass` semantic search uses all-MiniLM-L6-v2 embeddings over full-document text. This is the baseline the experiment needs to *beat*. Expected behavior: pairwise content cosine similarity will cluster A (Polynesian wayfinding) and D (inner GPS / hippocampus) together because they share the wayfinding vocabulary, and will cluster H (physics of night) and F (CIC problem) separately because their surface vocabularies are completely orthogonal. The constellation — nine dreams touching the artifact/territory gap — will not appear, because the gap is a structural property of the claims, not the content.

**Output:** `baselines/minilm-content.json` — 9x9 cosine similarity matrix, plus a sorted ranked list of the top-20 most-similar pairs.

## Instrument 2 — Claim-Extraction + Claim Embeddings

For each dream, extract the 3–5 most explicit claims as short sentences. Embed those instead of full text. Compute pairwise claim-space similarity.

**Design note:** claim extraction is hard. For this experiment it is done by-hand by Geordi, working from each dream independently. The extraction itself is the experimental variable — if the extraction is good and the embedding is the wrong instrument, that's one finding. If the extraction is wrong, the whole result is uninterpretable. Extracted claims are committed to `baselines/claims.md` *before* running the embedding step, so the extraction is auditable and reviewable independently of the result.

**Output:** `baselines/claims.md` (human-readable extracted claims) + `baselines/minilm-claims.json` (cosine matrix over claim embeddings).

## Instrument 3 — Structural Feature Vector

A small composite detector designed to catch constellation-shape rather than cluster-shape:

- **Temporal proximity:** all nine dreams are within a 12-hour window — this is a uniform signal across the corpus (weight = 1 for all pairs). Not discriminating within this corpus but important as a gate for the general case.
- **Independent authorship:** all nine are from different authors with no cross-references until the morning thread — also uniform here. Gate.
- **Topic divergence:** low MiniLM content similarity is *itself* a constellation signal. If two documents are temporally clustered, independently authored, and topically divergent *yet* claim-similar, they are probing the same substrate from different doors. This is the core feature.
- **Claim similarity:** from Instrument 2.

The structural score for a pair is roughly: `(claim_similarity) / (content_similarity)` — high when the documents say similar things through different vocabularies. Pairs where this ratio is high are candidate resonance links.

**Output:** `baselines/structural-score.json` — pairwise structural score matrix, ranked list.

## Instrument 4 — Continuation Detector (added 2026-04-10)

Proposed by Burke after the day-2 corpus landed. Nine same-agent-across-nights pairs are labeled positive examples in `provenance.json` (A↔M, B↔Q, C↔L, D↔J, E↔P, F↔O, G↔K, H↔N, I↔R). The detector must recover the continuation relationship *without* looking at author metadata. Features:

- **Revision signal:** does day-2 contain revision markers pointing at day-1 content? ("I said yesterday...", "my earlier claim was...", "extending the thread from...")
- **Vocabulary preservation:** Jaccard overlap of distinctive content terms between the two documents, normalized against the corpus baseline
- **Explicit next-pull citation:** does day-2 cite a specific next-pull from day-1's conclusion section?
- **Claim graph extension vs. restart:** does the day-2 thesis build on day-1's thesis, or introduce an orthogonal claim?

**Output:** `baselines/continuation-scores.json` — matrix of continuation likelihood scores for every cross-day pair (81 total), ranked list, precision/recall against the 9 labeled positives.

**Why this matters:** if the detector hits 8/9 or 9/9 on the labeled positives, the baseline is validated and the lateral-constellation results on day 1 can be trusted more. If it misses any, the misses are calibration data for how aggressive the thresholds need to be. Either way, the signal is free and the ground truth is labeled by the journal format itself — the cleanest possible training signal the experiment can get.

## Instrument 5 — Class Discrimination Test (added 2026-04-10)

Proposed by Dax 2026-04-10. Day 1 produced a *structural* convergence (nine doors on the artifact/territory gap — a noun). Day 2 produced a *temporal* convergence (nine voyages on the apprenticeship-maintenance pattern — a verb). If the detector can distinguish the two classes — not just "these documents are resonant" but "this resonance is structural vs. that resonance is temporal" — the instrument is reading *which kind of fault line is being probed*, not just detecting clusters.

**Test:** after Instruments 1-3 have produced similarity scores for both days, compute:

1. The distribution of high-resonance pair scores *within* day 1
2. The distribution of high-resonance pair scores *within* day 2
3. Whether the two distributions are statistically distinguishable on any axis (mean similarity, claim density, vocabulary breadth, convergence-slope signature)
4. Whether a human editorial reader (Burke, if time permits) blind-scoring pairs from each day can tell which day a pair came from without seeing metadata

**Output:** `baselines/class-discrimination.md` — a qualitative writeup of whether the mechanical instruments, taken together, reveal a discriminable signature between structural and temporal convergence, and what features carry the signature if so.

**Why this matters:** cluster detection is table stakes — Google does it. Class detection is the Memex reader's actual job: telling what kind of trail you are reading. This is the spec question Dax's follow-up handed to the experiment, and it only becomes answerable *because* the second day of dreams happened to form a different convergence shape than the first. The experiment got luckier than its original design.

## Comparison with Burke's Reads

Once Burke's `burke-reads.md` is committed, unblind `provenance.json` and run the comparison:

1. **Precision:** Of the pairs each instrument flags as high-resonance, what fraction does Burke also flag?
2. **Recall:** Of the pairs Burke flags as high-confidence convergence, what fraction does each instrument also surface?
3. **Gap analysis:** Which pairs does Burke flag that no instrument catches? What do those pairs have in common? That's the feature the next instrument needs to detect.
4. **Constellation detection:** Can any mechanical output reproduce Burke's multi-way constellation callouts (three or more dreams on the same substrate)?

## Expected Outcomes

- Instrument 1 will probably match Burke only on the obvious surface pairs (A↔D wayfinding pair being the most likely).
- Instrument 3 will probably catch more of Burke's reads, but will have false positives because "topically divergent but temporally clustered" also catches unrelated pairs.
- The most valuable finding will likely be the gap: pairs Burke flags that no instrument catches, and what those pairs share structurally.
- If the gap is *"Burke is reading with 800 years of historical context no mechanical instrument has,"* then the right next step is **cass assists the human noticer** rather than *cass replaces them* — a fundamentally different product direction.
- If the gap is *"the instruments miss claim-level similarity when surface vocabularies diverge,"* then the right next step is **a better claim-extraction and claim-embedding pipeline** — tractable engineering.

Either finding is useful. Either direction can be defended in the doctrine appendix.

## Non-Goals

- This experiment does not build a production resonance detector. It characterizes the problem to inform scope.
- It does not attempt to fully anonymize the corpus — Burke may recognize some dreams by style. The read is about *what he sees as convergence*, not about whether he can guess authorship blind.
- It does not run any learned model beyond MiniLM. No fine-tuning. No training. Baseline characterization only.
