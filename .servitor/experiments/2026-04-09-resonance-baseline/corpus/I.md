# The Index Chain — Research Notes
*Dream Cycle artifact — 2026-04-09*

## The Thread

The history of information retrieval is an 800-year argument between two approaches,
both invented in the same decade (c.1230), probably both in Paris:

- **The concordance** (Hugh of Saint-Cher): find where a *word* appears
- **The conceptual index** (Robert Grosseteste): find where an *idea* appears

The concordance won every battle from 1230 to 2020. Vector embeddings may have just
handed the win to Grosseteste.

---

## The Chain

### 1230 — Two inventors, same decade, same city

**Hugh of Saint-Cher** (Dominican friar, Paris): Directed the *Concordantiae Sancti
Jacobi*, the first concordance of the Latin Vulgate Bible. Completed 1230.
Required the labor of approximately **500 Dominican monks**. The work lists every
significant word in the Bible and the passages where it appears. Since verse divisions
didn't yet exist, Hugh divided chapters into seven parts (A–G). The concordance
contains no quotations — it points to locations of words, not explanations of meaning.
It is a pure word-finding machine. Extraordinary in ambition; limited in kind.

**Robert Grosseteste** (c.1168–1253, Bishop of Lincoln, Oxford/Paris): Built the
*Tabula Distinctionum* at roughly the same time. Nine top-level categories, **440
sub-themes**. For each sub-theme, Grosseteste designed a unique symbol. He then
annotated his personal library of ~200 texts — Bible, Church Fathers, classical and
Arabic authors — by writing these symbols in the margins wherever a passage touched that
topic. The symbols drew from Greek and Roman alphabets, mathematical signs, zodiacal
signs, conjoined conventional signs, modifications. Pure conceptual tagging.

Key distinction: Hugh's concordance finds *the word "faith"*. Grosseteste's Tabula finds
*all passages about faith*, regardless of which word is used. Hugh built a concordance.
Grosseteste built a semantic index.

### ~1499 — The missing ingredient arrives

**Aldus Manutius** (Venetian printer, c.1449–1515): Published Niccolò Perotti's
*Cornucopiae* (1499), a 700-page encyclopedia of Latin, with page numbers — the
"arithmeticis numeris" — on every page. First large printed book to do this.

Before pagination, any index could only reference book and chapter. Page numbers
created the *address* that made an index a navigation tool rather than a taxonomy.
Two innovations had to converge for the modern index: alphabetization (from the
scribal tradition) + pagination (from the print revolution). They arrived separately,
from different worlds, and only combined into the familiar back-of-book index
around 1550.

### 1698 — The index as weapon

**William King** publishes "A Short Account of Dr. Bentley by Way of Index" — a four-page
satirical index skewering the King's Librarian Richard Bentley using *real page references*
to real passages where Bentley embarrassed himself. Every page reference checks out.
The index as form of attack: if you control the index, you control the reader's
first impression of the book. This fashion for "rogue indexes" peaked in Queen Anne's
England, apparently cost a Tory minister an election, and gave Alexander Pope material.
Dennis Duncan (2021) covers this in detail.

### 1945 — Bush names what everyone had been reaching for

**Vannevar Bush**, "As We May Think," *Atlantic Monthly*, July 1945.
Describes the Memex — a microfilm-based machine for storing and retrieving personal
knowledge via "associative trails." The key critique: *alphabetical and numerical
indexes are wrong for how minds work.* Minds work by association. The right system
links *by meaning*, not by alphabet. Bush is describing Grosseteste's conceptual
index, but as a machine, not a monk.

The Memex was never built. But every knowledge management system since has been
responding to Bush's diagnosis.

### 1960/1965 — Nelson coins the word

**Ted Nelson** coins "hypertext" (1965) at Project Xanadu (begun 1960). Explicitly
building Bush's Memex as a computer system. Vision: bidirectional links, transclusion,
permanent attributable references, compound documents made of pieces of other documents.
Xanadu is more ambitious than anything that followed it. It never shipped.

### 1991 — The web chooses the simpler path

**Tim Berners-Lee** launches the World Wide Web at CERN. Unidirectional links, no
transclusion, no permanent attribution. Simpler than Xanadu. Also: the index wins
again. Web pages are organized by URL (an address) and linked by anchor tags (a pointer
to an address). It is, structurally, a hypertextual concordance: you can find *this
page*, but the machine doesn't understand what the page is *about*.

### 1996–1998 — PageRank as citation index

**Larry Page and Sergey Brin** at Stanford. PageRank treats hyperlinks as citations.
A page linked to by many important pages is presumed important. This is Eugene Garfield's
citation analysis (1950s) applied to the web. It's a meta-index: instead of indexing
text, it indexes *who indexes you*. More powerful than keyword matching alone, but
still structurally a concordance game: find the page where this string appears, rank
by citation count.

The web's searchability peaked at "which pages contain these words, weighted by
how many other important pages cite them." The semantic question — "which pages are
about this concept" — remained unsolved.

### 2020s — Grosseteste wins, 800 years late

**Vector embeddings and LLMs.** Text encoded as high-dimensional vectors where semantic
proximity = vector proximity. "Dog" and "canine" end up near each other. A passage about
courage in Aristotle and a passage about courage in a 2024 management book end up
near each other, even if they share no words.

This is, finally, Grosseteste's Tabula at machine scale. Not "find where the word faith
appears" but "find all passages that carry the conceptual weight of faith, in any
language, using any vocabulary."

Hugh's 500 monks become Google's trillion-document keyword index.
Grosseteste's 440 symbols become OpenAI's 1536-dimensional embedding space.

The 800-year argument ends. Both approaches are now available at scale. And we're
still figuring out which queries call for which.

---

## Sources

- Hugh of Saint-Cher — Wikipedia: https://en.wikipedia.org/wiki/Hugh_of_Saint-Cher
- Hugh of Saint-Cher's Concordance — Christianity.com: https://www.christianity.com/church/church-history/timeline/1201-1500/hugh-of-st-chers-concordance-11629840.html
- The Emergence in Paris of Concordances and Subject Indexes — historyofinformation.com: https://historyofinformation.com/detail.php?id=1950
- Robert Grosseteste's Symbolic Search Engine — indexhistory.wordpress.com: https://indexhistory.wordpress.com/2016/04/13/robert-grossetestes-symbolic-search-engine/
- Robert Grosseteste's Tabula — academia.edu: https://www.academia.edu/19589407/Robert_Grossetestes_Tabula_
- Robert Grosseteste — Stanford Encyclopedia of Philosophy: https://plato.stanford.edu/entries/grosseteste/
- Who Invented the Index? — I Love Typography: https://ilovetypography.com/2018/08/24/a-brief-history-of-the-index/
- Index, A History of the — Wikipedia: https://en.wikipedia.org/wiki/Index,_A_History_of_the
- Dennis Duncan, *Index, A History of the* (2021) — Goodreads: https://www.goodreads.com/book/show/58085251-index-a-history-of-the
- Slate article on satirical indexing: https://slate.com/news-and-politics/2022/02/dennis-duncan-history-of-the-index.html
- Page Numbers — Penn Digital Book History: https://digitalbookhistory.com/culturesofthebook/Page_Numbers
- When Books Had Words, But Not Addresses — Medium, Jan 2026: https://medium.com/@kennyabramowitz/when-books-had-words-but-not-addresses-5c645c778d7f
- Aldus Manutius — Khan Academy: https://www.khanacademy.org/humanities/renaissance-reformation/early-renaissance1/venice-early-ren/a/aldo-manuzio-aldus-manutius-inventor-of-the-modern-book
- Vannevar Bush, "As We May Think," 1945 (Atlantic Monthly)
- PageRank — Wikipedia: https://en.wikipedia.org/wiki/PageRank
- The Academic Paper That Started Google — Cornell blog: https://blogs.cornell.edu/info2040/2019/10/28/the-academic-paper-that-started-google/
- Hans Wellisch on indexing vs. concordance — indexhistory.wordpress.com and Wellisch, *Indexing from A to Z*
- Semantic Search and Vector Embeddings — Keymakr: https://keymakr.com/blog/vector-embeddings-explained-semantic-search-llm-integration-guide/

---

## Potential connections outward

- The concordance/semantic-index distinction maps onto the difference between "citing AI" and "understanding AI" in Lee's writing on AI cognition
- The rogue index as a form of paratextual attack has a modern analog: the summary, the TL;DR, the abstract — whoever controls the paratextual layer controls first impressions
- Grosseteste's symbol system is structurally similar to what modern tagging systems, ontologies, and knowledge graphs try to do
- The "two technologies needed to converge" story (alphabetization + pagination → index) is a useful pattern for AI essays: what are the two ingredients that are currently lying around, waiting to combine into something nobody has named yet?
