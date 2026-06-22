# Repository Guidelines

## Project Structure & Module Organization

This is a Rust workspace for `zramdedup`. Workspace members live under `crates/`:

- `crates/common`: shared config, procfs, PSI, KSM, and error utilities.
- `crates/daemon`: `zramdedup` binary, including the PSI-aware KSM governor, scanner, metrics, and madvise logic.
- `crates/swap-proxy`: experimental `zramdedup-swap-proxy` binary and deduplicating page-store logic.

Operational files are outside the workspace crates: `config/zramdedup.toml` contains the sample runtime configuration, and `systemd/` contains service units. Tests are currently inline in each crate’s `src/*.rs` files under `#[cfg(test)]`.

## Build, Test, and Development Commands

- `cargo build --workspace`: build every crate and binary.
- `make check`: run `cargo check --workspace --all-targets`, `cargo fmt --check`, and Clippy with `-D warnings`.
- `make test`: run `cargo test --workspace`.
- `make coverage`: generate HTML coverage with `cargo llvm-cov` in `target/llvm-cov/html/`.
- `make coverage-ci`: write LCOV output to `target/llvm-cov/lcov.info`.
- `cargo run -p zramdedup-daemon -- run --config config/zramdedup.toml --dry-run`: exercise the daemon locally without sysfs writes or madvise calls.

Install `cargo-llvm-cov` before using coverage targets: `cargo install cargo-llvm-cov`.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and standard `rustfmt` formatting. Keep functions, modules, and variables in `snake_case`; types and traits in `PascalCase`; constants in `SCREAMING_SNAKE_CASE`. Prefer small modules with explicit ownership of kernel-facing behavior. Use `tracing` for operational logs, `anyhow` for binary-level errors, and typed errors where shared library code needs structured failure handling.

## Testing Guidelines

Place unit tests next to the code they cover in `#[cfg(test)] mod tests`. Name tests by observable behavior, for example `parses_valid_psi_line` or `governor_steps_down_after_hysteresis`. Use `#[tokio::test]` for async code and `tempfile` for filesystem-dependent tests. Run `make test` before submitting changes; run `make coverage` for governor, procfs, KSM, or swap-proxy logic changes.

## Commit & Pull Request Guidelines

Recent history follows Conventional Commits: `feat: ...`, `fix(scope): ...`, and `test(coverage): ...`. Keep subjects imperative and specific, such as `fix(ksm): restore advisor_mode before pages_to_scan`.

Pull requests should describe the behavior change, list validation commands run, and call out kernel, systemd, or configuration impacts. Include linked issues when available and attach logs or screenshots only when they clarify runtime behavior.

## Security & Configuration Tips

Be careful with code that touches `/sys/kernel/mm/ksm`, `/proc`, swap devices, or systemd capabilities. Prefer dry-run paths for local testing. Keep defaults conservative in `config/zramdedup.toml`, and document any new required capability or writable path in the relevant systemd unit.
