# Pass 2 - Preview MIME Constants

- Mission: Literal/Constant Table Tightening
- Score: 2.0
- Files changed: `src/pages/preview.rs`

## Change

The preview server now uses private constants for two repeated MIME literals:

- `MIME_APPLICATION_OCTET_STREAM`
- `MIME_TEXT_PLAIN`

These replace repeated literal spellings in `guess_mime_type(...)`, plain-text error responses, and the test-only canonicalization wrapper.

## Isomorphism Proof

- The `.bin` MIME and unknown-extension fallback still return `application/octet-stream`.
- Plain-text 400, 404, 405, and 500 responses still use `text/plain`.
- The distinct `text/plain; charset=utf-8` mapping for `.txt` remains a literal because it is a different value.
- Existing preview tests still exercise MIME guessing and all request error paths touched by the substitutions.

## Fresh-Eyes Prompt

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

## Fresh-Eyes Result

Re-read the constants and every callsite substitution. No bugs found. The pass deliberately did not collapse `"text/plain; charset=utf-8"` because that would change the `.txt` response content type.

## Verification

- `rustfmt --edition 2024 --check src/pages/preview.rs`
- `git diff --check -- src/pages/preview.rs .skill-loop-progress.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_sixth_simplify cargo test --lib pages::preview::tests::`
