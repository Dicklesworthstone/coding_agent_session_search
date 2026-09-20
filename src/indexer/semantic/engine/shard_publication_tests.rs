// Keep the existing file-backed publication tests beside the semantic facade,
// while exercising the unchanged engine implementation and its private helpers.
use super::*;

#[path = "../shard_publication_tests.rs"]
mod existing;
