# Model Test Fixtures

These files are used by tests that check model installation state.
`model.onnx` is a minimal valid ONNX identity model (see source below).

## Files

- `model.onnx` - Minimal valid ONNX identity model fixture
- `model.onnx.placeholder` - Legacy placeholder (retained for backwards compatibility)
- `tokenizer.json` - Minimal valid tokenizer config
- `config.json` - Minimal valid model config
- `special_tokens_map.json` - Standard BERT special tokens
- `tokenizer_config.json` - Tokenizer configuration

## Usage

Tests should copy these fixtures to temp directories rather than
creating synthetic "fake" content dynamically.

## Source

- `model.onnx` is sourced from ONNX test data:
  `onnx/backend/test/data/node/test_identity/model.onnx` (Apache-2.0)

## No-Mock Policy

Per the project's no-mock policy (see TESTING.md), tests should use
real fixtures with documented provenance rather than synthetic data.
