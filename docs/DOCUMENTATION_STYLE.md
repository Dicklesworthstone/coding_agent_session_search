# Documentation Style Guide

This guide defines standards for documentation in the cass project.

## General Principles

1. **Clarity over brevity** - Write clearly, even if it takes more words
2. **Audience awareness** - Consider who will read this (users vs. developers)
3. **Consistency** - Follow established patterns throughout
4. **Accuracy** - Keep documentation synchronized with code
5. **Completeness** - Document all public interfaces

## File Types

### README.md

The main README should include:

```markdown
# Project Name

Brief description (1-2 sentences).

## Features

- Feature 1
- Feature 2

## Installation

```bash
cargo install coding-agent-search
```

## Quick Start

Minimal example to get started.

## Usage

Detailed usage with examples.

## Configuration

Configuration options and environment variables.

## Security

Brief security notes (link to SECURITY.md for details).

## Contributing

How to contribute.

## License

License information.
```

### API Documentation (Rustdoc)

Follow Rust documentation conventions:

```rust
/// Brief description of the function.
///
/// Longer explanation if needed, including:
/// - Implementation details
/// - Performance characteristics
/// - Thread safety notes
///
/// # Arguments
///
/// * `arg1` - Description of arg1
/// * `arg2` - Description of arg2
///
/// # Returns
///
/// Description of return value.
///
/// # Errors
///
/// When and why this function returns an error.
///
/// # Panics
///
/// Conditions that cause a panic (if any).
///
/// # Examples
///
/// ```rust
/// let result = my_function(arg1, arg2)?;
/// assert!(result.is_valid());
/// ```
///
/// # Safety
///
/// (For unsafe functions) Why this is safe to call.
pub fn my_function(arg1: Type1, arg2: Type2) -> Result<Output, Error> {
    // ...
}
```

### Module Documentation

Each module should have a top-level doc comment:

```rust
//! Brief description of the module.
//!
//! This module provides:
//! - Capability 1
//! - Capability 2
//!
//! # Architecture
//!
//! Explain how components fit together.
//!
//! # Examples
//!
//! ```rust
//! use crate::module_name;
//!
//! // Example usage
//! ```
```

### SECURITY.md

Security documentation should include:

1. **Threat Model** - What we protect against
2. **Cryptographic Choices** - Algorithms and parameters
3. **Key Management** - How keys are derived and stored
4. **Attack Resistance** - Specific attacks mitigated
5. **Limitations** - What we don't protect against

## Markdown Style

### Headers

Use ATX-style headers with a blank line before and after:

```markdown
## Section Header

Content here.

### Subsection

More content.
```

### Code Blocks

Always specify the language:

````markdown
```rust
fn example() {
    println!("Hello");
}
```
````

For shell commands, use `bash` or `sh`:

````markdown
```bash
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-doc-example-target cargo build --release
```
````

### Lists

Use `-` for unordered lists, `1.` for ordered:

```markdown
- Item one
- Item two
  - Nested item

1. First step
2. Second step
3. Third step
```

### Links

Use reference-style links for readability in long documents:

```markdown
See the [installation guide][install] for details.

[install]: ./docs/INSTALLATION.md
```

### Tables

Align columns for readability:

```markdown
| Column 1 | Column 2 | Column 3   |
|----------|----------|------------|
| Value    | Value    | Long value |
| Short    | Medium   | Value      |
```

## CLI Help Text

CLI help should follow this structure:

```
tool-name [version]
Brief description

USAGE:
    tool-name [OPTIONS] <COMMAND>

COMMANDS:
    command1    Brief description
    command2    Brief description

OPTIONS:
    -h, --help       Print help information
    -V, --version    Print version information
    -v, --verbose    Enable verbose output

EXAMPLES:
    tool-name command1 --option value
    tool-name command2 input.txt
```

### Command Descriptions

- Start with a verb (Search, Index, Export)
- Keep to one line (< 60 chars)
- Use sentence case

### Option Descriptions

- Start lowercase
- No trailing period
- Include default values: `compression level [default: 6]`

## Generated Documentation

### HTML Documentation

Generated help.html and recovery.html should:

1. Be valid HTML5
2. Include proper meta tags
3. Be responsive (mobile-friendly)
4. Use semantic elements
5. Include accessibility attributes

### Dynamic README Content

When README content is generated, ensure:

1. Numbers match actual data (conversation counts, etc.)
2. Dates are accurate
3. URLs are valid
4. Version numbers are current

## Documentation Testing

### Automated Tests

The `tests/docs/` module verifies:

- README accuracy against actual data
- CLI help completeness
- Generated HTML validity
- Link validity

### Manual Review Checklist

Before release, verify:

- [ ] README reflects current features
- [ ] All public APIs have documentation
- [ ] Examples compile and run
- [ ] Links are not broken
- [ ] Security docs are up to date
- [ ] CHANGELOG is updated

## Common Mistakes to Avoid

1. **Stale documentation** - Update docs when code changes
2. **Missing examples** - Include at least one example per public function
3. **Undocumented errors** - Always document error conditions
4. **Jargon without explanation** - Define technical terms
5. **Assuming knowledge** - Don't assume readers know the codebase
6. **Broken links** - Verify all links work
7. **Outdated screenshots** - Update visuals when UI changes

## Tools

### Validation

Run documentation validation:

```bash
./scripts/validate_docs.sh
```

### Building Docs

Build Rust documentation:

```bash
cargo doc --no-deps --open
```

### Link Checking

The validation script checks links. For more thorough checking:

```bash
# Install markdown-link-check
npm install -g markdown-link-check

# Check a file
markdown-link-check README.md
```

## Version History

| Version | Date       | Changes                    |
|---------|------------|----------------------------|
| 1.0     | 2024-01-01 | Initial style guide        |
| 1.1     | 2024-06-01 | Added generated docs rules |
