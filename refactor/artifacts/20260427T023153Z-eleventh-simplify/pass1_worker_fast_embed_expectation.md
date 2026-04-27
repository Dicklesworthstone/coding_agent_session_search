# Pass 1 - Worker Fast-Embed Expectation Helper

- Mission: Test Expectation Helper.
- Files changed: `src/daemon/worker.rs`.
- Simplification: added `fast_embed_kind(...)` in the daemon worker tests and reused it for repeated `WorkerEmbedderKind::FastEmbed` expected values.
- Isomorphism proof: helper constructs the same enum variant with the same `model_name.to_string()` and `embedder_id.to_string()` conversions previously written inline.
- Fresh-eyes review: verified alias inputs, public expected strings, and embedder IDs remain explicit at each assertion call site; only repeated enum construction moved.
