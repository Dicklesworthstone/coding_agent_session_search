# The Archaeology of Process: What Coding Sessions Are, Epistemically

*Notes from dream cycle #1 — 2026-04-09*

---

## The Gap That History Keeps Hiding

Throughout the history of science, a structural gap persists between how discovery
actually happens and how it gets reported. Newton's *Principia* reads as inevitable
deduction; his Waste Book shows years of blind alleys. Darwin's *Origin of Species*
presents itself as Baconian inductivism — collecting facts before forming theories.
His private notebooks, written in the 1830s, show he had the hypothesis of evolutionary
transmutation before he had much evidence. The "cautious Baconian scientist" was a
performed identity, maintained for two decades to make the argument more acceptable.

Faraday kept 42 years of lab diaries — 30,000 experiments — but his handwriting was
neat, with few corrections. Historians suspect he wrote up his entries later, like a
Victorian gentleman composing his memoir. Even the "process record" was a cleaned-up
retrospective.

The pattern: **published artifacts systematically lie about the process that produced them**.
Not out of deception, necessarily, but because the human working process is too messy,
too iterative, too theory-laden to present cleanly. The process gets hidden not once but
twice: first by the scientist who tidies up, then by the reader who infers a linear
trajectory from the finished artifact.

---

## What Gets Lost: Polanyi's Tacit Dimension

Michael Polanyi: "We can know more than we can tell."

The tacit dimension of expertise — the feel for the material, the sense that something
is wrong before you can name why, the embodied knowledge of where bugs live in a
particular codebase — resists full articulation. It's learned by doing, by imitation,
by working alongside someone who already has it. You cannot transfer it by documentation.

This has a specific implication: **process documentation helps, but doesn't solve the
tacit knowledge problem**. A lab notebook can capture hypotheses, measurements, results.
It can't capture the experienced researcher's nose for when results are suspicious, or
the intuition that says "try the pH buffer first before the temperature." That intuition
can only be recovered by watching someone work, or by watching enough records of someone
working that you absorb the pattern.

---

## Knowledge Lives in the System: Hutchins

Edwin Hutchins studied navigation teams on ships. His key finding: the knowledge required
to navigate the ship doesn't live in any one person's head. It's distributed across the
instruments (charts, compasses, depth finders), the protocols (written procedures), the
crew (each member holding a piece of the calculation), and the artifact history (the
accumulated corrections to the charts). You cannot understand how the ship is navigated
by interviewing the captain.

"People create their cognitive powers by creating the environments in which they exercise
those powers."

The environment is not a container for cognition. It is constitutive of cognition. Remove
it and you don't just lose access to information — you lose the cognitive capability
itself.

**Applied to software development:** Lee's knowledge about a codebase is not in his head.
It's distributed across: the code itself, the commit history, the docs, the tests, the
sessions where problems were debugged, the mental models built and revised over time.
Remove any of these and the cognitive capability degrades. The sessions aren't a backup
of Lee's thinking. They are part of the thinking.

---

## Bainbridge's Ironies (1983) Arrive in 2025

Lisanne Bainbridge, 1983: When you automate a task, operators stop exercising the skill
required to do it. When automation fails — exactly when you need the human to take over —
the human is least capable of doing so. The more reliable the automation, the worse the
outcome when it fails.

Aviation lived through this. Pilots who flew highly automated aircraft for years struggled
when automation failed and they needed manual stick-and-rudder skills they hadn't used.
"Automation-induced skill atrophy" became a documented phenomenon.

The epistemology crisis in AI-assisted development (2024-2025) is Bainbridge's irony
arriving in software. As AI handles more code generation:

- Developers accumulate **declarative knowledge** (it works) but lose **procedural
  knowledge** (how/why it works)
- **Epistemological debt** accrues invisibly — the gap between what systems do and what
  developers understand
- When something fails, developers have no framework for debugging; they devolve into
  prompting the AI again, which may reproduce the same flawed assumption

The article "The Epistemology Crisis in AI-Assisted Development" frames this as
architecture-level opacity: not just "I don't understand this function" but "I don't
understand why this system is structured the way it is."

---

## Sessions as a New Epistemic Artifact Class

Coding sessions with AI (Claude Code, Codex, Cursor) occupy a genuinely novel position
in the knowledge landscape. They are not:

- **Documentation** — that's the cleaned-up product narrative
- **Git commits** — those record state changes, not reasoning chains
- **Think-aloud transcripts** — those are artificial experimental conditions that
  interfere with the task
- **AI memory systems** — those serve the AI's continuity, not the human's recall

Sessions are something closer to Darwin's real notebooks — but with a critical difference.
Darwin's notebooks were private, and he kept them private deliberately. Sessions exist
*because* of the collaborative medium. They are the working space and the record of the
working space simultaneously.

What a session captures that nothing else does:

1. **The reasoning trajectory** — not just the answer, but the hypotheses tested and
   discarded on the way to the answer
2. **The moment of realization** — when the approach changed, and what changed it
3. **The alternatives considered** — what was tried and why it failed
4. **The constraints at play** — what Lee knew and didn't know when approaching the problem

This is the "private science" that never makes it into the commit or the PR. The Origin
of Species makes natural selection look inevitable. The git log makes the final
architecture look obvious. The session shows the actual messy voyage.

---

## The Irony Specific to Indexing Sessions

But here Bainbridge's irony reappears in a new form.

**The value of a session scales with the quality of the thinking in it.**

If AI is doing more of the implementation — if Lee says "add a retry loop" and Claude
adds it without further dialogue — the session contains almost nothing. The prompting
pattern reveals nothing because Lee didn't have to engage with the problem.

The sessions most worth indexing are the ones where Lee:
- Had a wrong hypothesis and got corrected
- Tried three approaches before finding one that worked
- Argued with the AI's recommendation
- Worked through a novel architectural problem without an obvious answer

These sessions contain externalized tacit knowledge — not just "what was done" but
fragments of "how to think about problems like this."

**The risk:** As AI gets better and Lee prompts less, sessions become shallower. The
index grows but the signal density decreases. More hay, fewer needles.

**The counter-move:** The value of cass is precisely in sessions where Lee IS engaged.
And the way to surface those sessions is better search — not just "what session discusses
Redis" but "what session shows Lee working through a difficult Redis problem, revising
his approach."

That's not keyword search. That's intent archaeology. It requires semantic understanding
of cognitive depth, not just topic relevance.

---

## What This Means for cass (Operationally Relevant Bleed)

The dream produced something with operational teeth:

1. **Sessions are a distinct artifact class** with no adequate prior analog. The design
   of cass should reflect this — indexing for sessions is not the same as indexing for
   documents or commits.

2. **Intent archaeology is the hard problem.** Current cass searches topics. Future
   cass should be able to surface reasoning trajectories — sessions where the problem was
   hard, where the approach changed, where understanding was built rather than retrieved.

3. **Signal density is a real metric.** Not all sessions are equally valuable to index.
   The longest sessions with the most back-and-forth on a single problem are the richest
   sources. This could inform both ranking and UI — surfacing "rich sessions" distinctly.

4. **The epistemological debt problem means cass's role is growing.** As AI does more
   implementation work, the sessions where humans actively engaged with problems become
   *more* valuable, not less. They're the remaining record of human procedural reasoning.

---

## Key References

- Polanyi, M. (1966). *The Tacit Dimension*. University of Chicago Press.
  "We can know more than we can tell."
  
- Hutchins, E. (1995). *Cognition in the Wild*. MIT Press.
  Navigation team as distributed cognitive system; artifacts as constitutive of cognition.

- Bainbridge, L. (1983). "Ironies of Automation." *Automatica*, 19(6), 775-779.
  The more reliable the automation, the worse the human operator when it fails.

- Nguyen Ha Thanh. "The Epistemology Crisis in AI-Assisted Development."
  *Data Science Collective / Medium*, 2024-2025.
  "Epistemological debt" — invisible until catastrophic failure.

- Asimov Press. "A Brief History of Lab Notebooks." (2024)
  Darwin's 17-year gap between private theory and public publication.

- PNAS. "Darwin and the Scientific Method."
  Darwin's claimed Baconianism vs. his actual hypothesis-first methodology.
