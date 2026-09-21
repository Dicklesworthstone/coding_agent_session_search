async function runMemoryProfile(page, iterations) {
  const result = await page.evaluate(async (count) => {
    const { queryAll } = await import("./database.js");
    const readMem = () => {
      const jsHeap = performance && performance.memory ? performance.memory.usedJSHeapSize : null;
      return { jsHeapBytes: jsHeap };
    };

    const baseline = readMem();

    for (let i = 0; i < count; i += 1) {
      queryAll(
        `SELECT m.id, m.content
         FROM messages_fts
         JOIN messages m ON messages_fts.rowid = m.id
         WHERE messages_fts MATCH 'test'
         LIMIT 10`,
      );
    }

    const after = readMem();
    const leakBytes =
      baseline.jsHeapBytes !== null && after.jsHeapBytes !== null
        ? after.jsHeapBytes - baseline.jsHeapBytes
        : null;
    const leakMB = leakBytes !== null ? leakBytes / (1024 * 1024) : null;

    return {
      baseline,
      after,
      leakBytes,
      leakMB,
      ok: leakBytes !== null ? leakBytes < 10 * 1024 * 1024 : null,
    };
  }, iterations);

  return result;
}

module.exports = { runMemoryProfile };
