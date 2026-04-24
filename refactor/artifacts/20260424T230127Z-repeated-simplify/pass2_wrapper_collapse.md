## Pass 2/10 - Pass-Through Wrapper Collapse

### Candidate Accepted

- **Wrapper:** `src/pages/verify.rs::resolve_site_dir`
- **Target:** inline all callsites to `super::resolve_site_dir`
- **Reason:** the private wrapper only forwarded `path` to the module resolver with no validation, logging, error mapping, or type conversion.
- **Score:** `(LOC_saved 1 x Confidence 5) / Risk 1 = 5.0`

### Isomorphism Card

#### Equivalence contract

- **Inputs covered:** `verify_bundle` path resolution plus existing resolver tests for bundle root, direct `site/`, and symlink rejection.
- **Ordering preserved:** yes. The call happens at the same point before any verification checks run.
- **Tie-breaking:** unchanged / N/A.
- **Error semantics:** unchanged. Errors still originate from `pages::resolve_site_dir` with the same `anyhow::Result<PathBuf>` and message text.
- **Laziness:** unchanged / N/A.
- **Short-circuit eval:** unchanged. The `?` still returns before verbose output and checks.
- **Floating-point:** N/A.
- **RNG / hash order:** N/A.
- **Observable side-effects:** unchanged. The resolver only inspects filesystem metadata; the same function is called with the same path.
- **Type narrowing:** unchanged / N/A.
- **Rerender behavior:** N/A.

#### Verification plan

- `rustfmt --edition 2024 --check src/pages/verify.rs`
- `git diff --check -- src/pages/verify.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass2_wrapper_collapse.md`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test pages::verify --lib`
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets`

#### Verification results

- `rustfmt --edition 2024 --check src/pages/verify.rs` - passed.
- `git diff --check -- src/pages/verify.rs refactor/artifacts/20260424T230127Z-repeated-simplify/pass2_wrapper_collapse.md` - passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo test pages::verify --lib` - passed: 88 passed, 0 failed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo check --all-targets` - passed.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings` - failed on pre-existing unrelated `src/search/query.rs:5859` `clippy::assertions-on-constants`.
- `rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_simplify_loop cargo clippy --all-targets -- -D warnings -A clippy::assertions-on-constants -A clippy::doc-overindented-list-items` - passed.
- `git diff --numstat -- src/pages/verify.rs` - `7 insertions`, `11 deletions`, net `-4` lines.

### Wrappers Inspected

- `src/pages/verify.rs::resolve_site_dir` - accepted; pure private forwarding hop.
- `src/pages/preview.rs::resolve_site_dir` - rejected; maps the shared resolver error into `PreviewError::SiteDirectoryNotFound`.
- `src/sources/setup.rs::generated_source_name_for_host` - rejected for this pass; the name documents generated-source identity in setup selection and has two coupled local callsites.
- `src/search/model_manager.rs::{load_semantic_context, load_semantic_context_no_version_check}` - rejected; public functions encode version-check policy.
