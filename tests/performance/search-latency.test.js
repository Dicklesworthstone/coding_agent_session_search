async function runSearchLatency(page, queries) {
  const results = await page.evaluate(async (qs) => {
    const { queryAll } = await import("./database.js");
    const rows = [];
    for (const query of qs) {
      const start = performance.now();
      const res = queryAll(
        `SELECT m.id, m.content, c.title
         FROM messages_fts
         JOIN messages m ON messages_fts.rowid = m.id
         JOIN conversations c ON m.conversation_id = c.id
         WHERE messages_fts MATCH ?
         ORDER BY rank
         LIMIT 100`,
        [query],
      );
      const elapsed = performance.now() - start;
      rows.push({ query, elapsed_ms: elapsed, count: res.length, ok: elapsed < 100 });
    }
    return rows;
  }, queries);

  return results;
}

module.exports = { runSearchLatency };
