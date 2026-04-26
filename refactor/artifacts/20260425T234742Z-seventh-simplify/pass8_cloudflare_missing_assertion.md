# Pass 8 - Assertion Helper

## Mission

DRY one repeated assertion cluster while preserving diagnostics.

## Scope

- `tests/deploy_cloudflare.rs`

## Change

Added a test-local `assert_missing_contains(...)` helper for Cloudflare prerequisite tests that repeatedly check whether a missing-prerequisite list contains a substring.

Before, each assertion repeated:

- iterate through `missing`
- call `contains(...)` on each message
- assert that at least one message matched

After, the helper performs the same substring search and includes the full `missing` list in the panic message.

## Isomorphism Check

- The exact checked substrings are unchanged:
  - `wrangler CLI not installed`
  - `npm install`
  - `not authenticated`
  - `CLOUDFLARE_API_TOKEN`
- The helper accepts `&str` and `String`-like values via `AsRef<str>`, matching the current `Prerequisites::missing()` return shape.
- The tests still verify the same negative prerequisite scenarios and still require each expected message fragment to appear.
- Diagnostics are preserved and improved: failures now show the expected substring and full missing-prerequisite list.

## Fresh-Eyes Review

Re-read the helper and every converted assertion after the first verification failure. The first helper type was too narrow for `Vec<&str>`, so it was corrected to a generic `T: AsRef<str> + Debug`. The final version avoids allocations, keeps all prior checks, and does not broaden any assertion.

## Verification

- `rustfmt --edition 2024 --check tests/deploy_cloudflare.rs`
- `git diff --check -- tests/deploy_cloudflare.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --test deploy_cloudflare test_prerequisites`

## Verdict

PRODUCTIVE.
