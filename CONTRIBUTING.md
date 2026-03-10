# Contributing to Morph Reth

Thanks for your interest in contributing to Morph Reth! This document provides guidelines and information for contributors.

## Getting Started

1. Fork the repository on GitHub
2. Clone your fork locally:
   ```bash
   git clone https://github.com/<your-username>/morph-reth.git
   cd morph-reth
   ```
3. Add the upstream remote:
   ```bash
   git remote add upstream https://github.com/morph-l2/morph-reth.git
   ```
4. Create a new branch for your work:
   ```bash
   git checkout -b feat/my-feature
   ```

## Development Environment

### Prerequisites

- Rust 1.88 or later
- Cargo

### Build

```bash
cargo build --release
```

### Running Tests

```bash
# Run all tests
cargo test --all

# Run tests for a specific crate
cargo test -p morph-consensus
```

### Code Quality

Before submitting a pull request, ensure all checks pass:

```bash
# Format code (requires nightly toolchain)
cargo +nightly fmt --all

# Run clippy lints
cargo clippy --all --all-targets -- -D warnings

# Build documentation
cargo doc --no-deps --document-private-items
```

## Pull Request Process

1. Ensure your branch is up to date with `main`:
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```
2. Write clear, concise commit messages following [Conventional Commits](https://www.conventionalcommits.org/):
   - `feat:` for new features
   - `fix:` for bug fixes
   - `refactor:` for code refactoring
   - `docs:` for documentation changes
   - `test:` for adding or updating tests
   - `chore:` for maintenance tasks
3. Include tests for new functionality
4. Update documentation if your changes affect public APIs
5. Open a pull request against `main` with a clear description of the changes

## Project Structure

See [README.md](README.md#architecture) for an overview of the crate structure.

## Code Style

- Follow standard Rust conventions and idioms
- Use `cargo fmt` formatting (nightly)
- All public items should have documentation comments
- Avoid `unsafe` code unless absolutely necessary and well-documented

## Reporting Issues

- Use [GitHub Issues](https://github.com/morph-l2/morph-reth/issues) to report bugs or request features
- Include steps to reproduce for bug reports
- Provide relevant logs, error messages, and environment details

## License

By contributing to Morph Reth, you agree that your contributions will be licensed under the [MIT License](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE), at your option.
