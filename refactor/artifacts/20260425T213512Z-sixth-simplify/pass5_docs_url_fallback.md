# Pass 5 - Docs URL Fallback Helper

- Mission: Option/Default Flow
- Score: 2.0
- Files changed: `src/pages/docs.rs`

## Change

README and about.txt generation now share `DocumentationGenerator::target_url_display()` for the optional deployment URL fallback.

## Isomorphism Proof

- When `DocConfig.target_url` is `Some`, generated docs still use the configured URL.
- When it is `None`, generated docs still use `[deployment URL]`.
- The fallback is still local to documentation generation; no config default changed.
- Existing README/about URL-present tests still pass, and a new about.txt no-URL test pins the second callsite.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the helper and both users. No bugs found. The helper returns `&str` from either the config-owned URL or the static placeholder, matching the previous `.as_deref().unwrap_or(...)` chain exactly.

## Verification

- `rustfmt --edition 2024 --check src/pages/docs.rs`
- `git diff --check -- src/pages/docs.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::docs::tests::`
