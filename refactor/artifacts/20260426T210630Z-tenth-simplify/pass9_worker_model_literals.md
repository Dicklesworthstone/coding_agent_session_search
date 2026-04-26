# Pass 9 - Worker Model Literals

- Mission: Literal Family Audit.
- Files changed: `src/daemon/worker.rs`.
- Simplification: pinned the daemon worker's recurring default model spellings in private constants: `HASH_EMBEDDER_MODEL` and `DEFAULT_SEMANTIC_MODEL`.
- Isomorphism proof: constants expand to the exact previous strings, and aliases, embedder IDs, supported-model error text, and all public model names remain unchanged.
- Fresh-eyes review: re-read default construction, semantic/hash comparison, `SemanticIndexer::new(...)`, and tests; verified no alias matching was removed, the default hash-vs-semantic distinction is unchanged, and tests still pin the exact public strings.
