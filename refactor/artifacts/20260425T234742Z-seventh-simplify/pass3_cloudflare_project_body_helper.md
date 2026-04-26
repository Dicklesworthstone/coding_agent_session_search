# Pass 3 - Projection Helper Narrowing

- Run: `20260425T234742Z-seventh-simplify`
- Timestamp: `2026-04-26T00:05:39Z`
- Mission: Projection Helper Narrowing
- Files changed: `src/pages/deploy_cloudflare.rs`

## Change

Extracted `project_create_body(...)` for the Cloudflare Pages project-create JSON body and added a focused shape test.

## Isomorphism Card

- `create_project_api(...)` still serializes the same JSON body before sending the request.
- The top-level keys remain `name`, `production_branch`, and `deployment_configs`.
- `deployment_configs` still contains exactly empty `production` and `preview` objects.
- The same `project_name` and `branch` arguments feed the same JSON fields.
- Request URL, method, auth token, content type, serialization error context, and response parsing are unchanged.

## Fresh-Eyes Review

Re-read the helper against the removed inline `json!` block and the callsite. The extracted helper preserves the full request-body shape; the module test also exercises the surrounding deployment tests to keep local Cloudflare deploy helpers intact.

## Verification

- `rustfmt --edition 2024 --check src/pages/deploy_cloudflare.rs`
- `git diff --check -- src/pages/deploy_cloudflare.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::deploy_cloudflare::tests::test_project_create_body_shape`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::deploy_cloudflare::tests::`

## Verdict

PRODUCTIVE
