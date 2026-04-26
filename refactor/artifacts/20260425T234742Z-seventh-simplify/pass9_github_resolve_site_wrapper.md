# Pass 9 - Wrapper Hop Collapse

## Mission

Remove one private pass-through helper with no independent contract.

## Scope

- `src/pages/deploy_github.rs`

## Change

Removed the private `resolve_deploy_site_dir(...)` wrapper from the GitHub Pages deployer and replaced its call sites with the shared `pages::resolve_site_dir(...)` helper directly.

The removed wrapper was:

- private to `deploy_github.rs`
- a one-line forwarder
- not adding error context, mapping, validation, tracing, metrics, or type conversion

## Isomorphism Check

- `GitHubDeployer::check_size(...)` still resolves either a bundle root or direct `site` directory before walking files.
- `GitHubDeployer::deploy(...)` still resolves the deployable site directory before prerequisite checks and repo staging.
- `copy_bundle_to_repo(...)` still resolves the bundle root before copying only deployable site files.
- Existing site-directory tests now call the shared helper directly, preserving the symlink rejection and direct-site acceptance checks that covered the wrapper.
- No public names or JSON/CLI contracts changed.

## Fresh-Eyes Review

Re-read every replacement after formatting. The only follow-up issue was naming: the tests still mentioned the removed wrapper, so they were renamed to describe `resolve_site_dir` behavior directly. The call graph now has one fewer private hop while preserving the same validation path.

## Verification

- `rustfmt --edition 2024 --check src/pages/deploy_github.rs`
- `git diff --check -- src/pages/deploy_github.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::deploy_github::tests::test_resolve_site_dir`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::deploy_github::tests::test_copy_bundle_to_repo_resolves_bundle_root_without_copying_private_artifacts`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo test --lib pages::deploy_github::tests::test_size_check`

## Verdict

PRODUCTIVE.
