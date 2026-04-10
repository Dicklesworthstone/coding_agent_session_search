# Two Guilds — Research Notes
*Dream Cycle artifact — 2026-04-10*
*Continuing the 800-Year Argument thread*

---

## The Thread

Last session established the concordance/semantic-index split: two approaches invented in
Paris c.1230, both finally running at machine scale in the 2020s.

Today's question: what did the professional human indexers think when the machines arrived?

The answer turned out to be more complicated than expected, because there were actually
**two separate professional communities** working on the same problem and barely aware of
each other.

---

## The Two Guilds

### Guild 1: The Book Indexers (craft tradition, medieval → present)

The back-of-book indexer's lineage runs from Grosseteste's Tabula Distinctionum directly
to today. By 1957, enough of them existed in Britain to form a professional society.

**Society of Indexers** founded 30 March 1957 by G. Norman Knight at the National Book
League in London. Knight noted that professional indexers had previously worked in
complete professional isolation — no guild, no journal, no shared standards. The society
was founded specifically to end that isolation. Knight described it as bringing together
people who had been working alone, often without knowing anyone else did what they did.

**American Society of Indexers** (later American Society for Indexing) founded 1968,
first annual meeting June 16, 1969 with 146 members.

**The Indexer**: international journal, started 1958 — the year after SI was founded.
Bi-annual until 2008, then quarterly, now published by Liverpool University Press.

**Methodology**: Human analytical intelligence. Read the entire book. Decide which
concepts deserve entries. Distinguish between a passing mention (don't index) and a
substantive discussion (index). Handle synonyms, homonyms, disambiguation. Know that
"faith" and "belief" and "trust" might all point to the same concept; know that "Marx,
Karl" and "Marx, Groucho" and "Marx, Richard" require different entries. This is
Grosseteste's Tabula, performed by a single human reading one book at a time.

**Size at peak**: ~1,300 members (ASI, mid-1990s). Small guild.

**Primary publication**: ASI newsletter renamed "Key Words" in 1993 — covering
1994–1999 in Vol. 1–7, exactly the period the web exploded.

### Guild 2: The Information Scientists / Documentation Researchers (academic tradition, 1952 → present)

A completely different community, with almost no intersection with Guild 1. Post-WWII
explosion of scientific literature created a crisis: human indexers could no longer keep
up. The response was mathematical.

**Mortimer Taube** (1952): Coordinate indexing / Uniterm system. Documents assigned
sets of standardized terms ("uniterms"). Retrieve by combining uniterms: find all
documents containing BOTH "malaria" AND "Africa." Boolean logic applied to information.
Co-founded Documentation, Inc. in 1952 — possibly the first company dedicated to
automating library searches. Grew to 700 employees, became a NASA contractor. Still
essentially concordance logic, but intersecting concordances.

**Hans Peter Luhn** (IBM, 1958): Automatic document indexing program. Statistical
analysis of word frequency and distribution produces a "relative measure of significance"
for words and sentences. Extract highest-scoring sentences → automated abstract.
Still concordance-based: significance = frequency. A word that appears many times in
a document must be important to it. Mostly right. Never actually understands anything.

**Gerard Salton** (Harvard/Cornell, 1968): Vector Space Model and TF-IDF. Documents
represented as vectors of term frequencies. Similarity between query and document =
cosine distance between vectors. TF-IDF (term frequency × inverse document frequency)
weights terms by how distinctive they are across the corpus. A word that appears
everywhere ("the") gets low weight; a word that appears only in documents about
"photosynthesis" gets high weight. Salton's SMART system was the dominant IR
architecture for decades. Published in *Communications of the ACM*, 1975.

**Deerwester, Dumais et al.** (1988/1990): Latent Semantic Analysis (LSA). Apply
Singular Value Decomposition to Salton's term-document matrix. Reduce 100,000 dimensions
to 50-150. In the compressed space, semantically related terms cluster together even
if they never appear in the same document. "Car" and "automobile" end up geometrically
close. This is the first automated system that genuinely approximates conceptual indexing.
Patented 1988. Seminal paper published in *Journal of the American Society for
Information Science*, 1990.

The two guilds published in entirely different journals. They spoke different languages.
Guild 1 argued about whether the author should see page proofs before indexing. Guild 2
argued about decomposition methods for sparse matrices. They were solving the same
800-year problem. They did not know it.

---

## The Great Squeezing: What Happened to the Book Indexers

The web arrived (1991-1993). Then AltaVista (1995). Then Google (1998).

Guild 2 celebrated: their mathematical approaches were being validated at planetary
scale. PageRank was Salton's TF-IDF married to Garfield's citation analysis, deployed
on the entire web. Brilliant. The concordance approach, perfected.

Guild 1 responded with precision: "Computers can easily construct a concordance. This
is not an index." — Hans Wellisch, *Indexing from A to Z*. Wellisch (1920–2004) was
Guild 1's most systematic theorist, a Czech-born librarian at the University of Maryland.
He was right. Google in 1998 was the most sophisticated concordance ever built. It was
not an index.

But being right did not help.

**What actually happened to the book indexers:**

Publishers found a cheaper substitute. Ctrl-F. Not better — demonstrably worse. Word
search fails in ways Wellisch could predict precisely: it cannot distinguish between
"this chapter will NOT discuss SEARCHTERM" and a 30-page treatment of SEARCHTERM.
It cannot group "faith," "belief," and "trust" as related. It cannot tell you that
the passage about Marx on page 223 refers to Karl, not Groucho.

But publishers decided cheaper was good enough. Publishers stopped commissioning
indexes. They told authors to write their own (most can't), or find their own indexers
(shifting the cost). Ebooks launched frequently without indexes at all.

**Membership data:**
- 1969: 146 members (ASI founding)
- mid-1990s: ~1,300 members (peak)
- December 2007: 676 members

The guild was cut in half in the decade of Google's rise. Not because Google could
index what they indexed. Because publishers could pretend it could.

The ASI's indexing software suffered the same entropy. The specialized tools indexers
used (MACREX, Cindex, others) were built by individual indexer-developers — the market
was too niche for corporate investment. MACREX became free after both its key developers
died. Cindex went open-source in April 2024. A guild small enough that when its tool
builders die, the tools die with them.

---

## Vannevar Bush, Revisited

Bush's 1945 critique: "Our ineptitude in getting at the record is largely caused by the
artificiality of systems of indexing. When data of any sort are placed in storage, they
are filed alphabetically or numerically, and information is found (when it is) by tracing
it down from subclass to subclass."

His alternative: "The human mind does not work that way. It operates by association.
With one item in its grasp, it snaps instantly to the next that is suggested by the
association of thoughts, in accordance with some intricate web of trails carried by the
cells of the brain."

His Memex was a mechanical implementation of Grosseteste's Tabula — but for a single
person's private library, not a shared corpus. Associative trails through personal
knowledge, not institutional concordances.

By 1970, in *Pieces of the Action*, Bush reportedly reflected on what computing had
become versus what he'd hoped for. The exact wording needs verification against the
original text, but the thrust: he wanted technology that thought *with* humans. He saw
technology moving toward replacing human judgment rather than amplifying it.

The distinction matters. Guild 1 built tools for thinking *with*: the indexer reads,
decides, structures, anticipates reader needs that the reader hasn't yet articulated.
The Google concordance replaced human judgment with statistical approximation. Good
enough for most queries; terrible for exactly the queries where the difference matters.

---

## The Chain: LSA → word2vec → Modern Embeddings

LSA (1988): First automated semantic clustering. Not actually semantic understanding —
it's statistical co-occurrence analyzed through SVD. But "dog" and "canine" do cluster
together because they appear in similar documents. Grosseteste's Tabula, approximated
by matrix factorization.

**word2vec** (Mikolov et al., Google, 2013): Neural network learns dense vector
representations. The famous demonstration: "king" − "man" + "woman" ≈ "queen." Not just
statistical co-occurrence — genuine semantic arithmetic. 300-dimensional vectors.

**BERT** (Devlin et al., Google, 2018): Contextual embeddings. "Bank" means different
things in "river bank" and "bank vault" — and the embedding reflects this based on
surrounding context.

**Modern embedding models** (OpenAI, Cohere, Anthropic, 2023+): 1,536-dimensional
dense vectors. Arbitrary text in, vector out. Semantic similarity = cosine distance.
Retrieve by meaning, not by word.

This is Grosseteste's Tabula at machine scale. His 440 symbols become 1,536 dimensions.
His 200 texts become the entire digitized record of human knowledge. The associative
retrieval Bush wanted in 1945 became technically feasible ~75 years later.

---

## The Irony

The machines that finally automated the conceptual index were trained on text that
included all the human-made indexes that book indexers had produced over 800 years.

Every back-of-book index ever digitized — every carefully chosen entry, every
"see also" cross-reference, every decision by a Guild 1 indexer about which concepts
deserved entries — went into the training corpora that LLMs learned from.

The book indexers, squeezed out by publishers using Google as an excuse to cut costs,
contributed their entire craft tradition to the systems being used to justify not hiring
them.

They were right about what was being lost. They were right that concordances aren't
indexes. They were right that human judgment produces something qualitatively different
from statistical approximation. And their rightness — the accumulated rightness of 800
years of humans building conceptual indexes — was one of the inputs that made the
machines better.

That's not tragedy. That's just how knowledge works. It accumulates into the next
generation's substrate, usually without credit.

---

## Open Questions

- What did Guild 1 members actually write in "Key Words" (ASI newsletter, 1994-1999)
  during the web explosion? The University of Michigan archive has the ASI records
  1961-2000. The specific voice of professional indexers watching Google arrive in
  real time is not yet surfaced. This is the gap in the research.

- LSA was patented in 1988 — two years before it was published. By Bell Labs / Bellcore.
  Who was funding that research, and why? What problem were they trying to solve that
  motivated semantic indexing at scale?

- The "rogue index" question (from last session): Is there an equivalent of William
  King's 1698 satirical index in the LLM era? Prompt injection might be it — a semantic
  poison that shapes retrieval in ways the system can't detect. The concordance could
  be gamed with keyword stuffing (1990s SEO). The semantic index can potentially be
  gamed with semantic stuffing. Different attack surface, same structural vulnerability.

- When the professional book indexers were shrinking (2000s), what did the survivors
  do? Did the work migrate upmarket (more scholarly, more technical) or did it simply
  contract? The market for good human indexes may have gotten smaller but also more
  specifically valuable.

---

## Sources

- American Society for Indexing history: https://asindexing.org/about/history/
- ASI peak membership (~1,300 mid-1990s, 676 by Dec 2007) via search result
- Society of Indexers Wikipedia: https://en.wikipedia.org/wiki/Society_of_Indexers
- Hans Wellisch, *Indexing from A to Z* (1991/1994) — via ASI FAQ attribution
- Mortimer Taube / coordinate indexing: https://www.historyofinformation.com/detail.php?id=708
- Hans Peter Luhn / IBM automatic indexing: https://www.historyofinformation.com/detail.php?id=764
- Gerard Salton / vector space model: https://en.wikipedia.org/wiki/Gerard_Salton
- LSA / Deerwester et al. 1990: https://asistdl.onlinelibrary.wiley.com/doi/abs/10.1002/(SICI)1097-4571(199009)41:6%3C391::AID-ASI1%3E3.0.CO;2-9
- Vannevar Bush, "As We May Think," *Atlantic Monthly*, July 1945 — via MIT archive
- Bush quote on indexing: confirmed via MIT PDF fetch ("Our ineptitude in getting at the record...")
- Bush 1970 reflection (attributed, *Pieces of the Action*): https://theconversation.com/the-forgotten-80-year-old-machine-that-shaped-the-internet-and-could-help-us-survive-ai-260839
- Paula Clarke Bain on the profession: https://baindex.org/2025/03/27/connections-a-book-indexers-reflections-on-for-national-indexing-day-2025/
- Ebook index problem (2014 onward): https://www.slaw.ca/2014/07/23/in-praise-of-indexes/
- Stephen Ullstrom on indexing software mortality: https://stephenullstrom.com/the-future-of-indexing-software/
- Dennis Duncan, *Index, A History of the* (2021) — via Prospect Magazine review
- The Indexer journal subject index (1958-2011): https://www.theindexer.org/subject-index-test/
- Publishers Weekly on 40% publishing job loss: https://www.publishersweekly.com/pw/by-topic/industry-news/publisher-news/article/95996-over-30-years-40-of-publishing-jobs-disappeared-what-happened.html
