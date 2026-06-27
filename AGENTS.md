# AGENTS.md — Repository Guidelines for zramdedup

**Agent Readiness:** This file contains two kinds of guidance:
- **Universal rules** — project structure, branching, architecture, and security patterns that apply regardless of what tooling you have.
- **Workflow recommendations** — tool-specific tips that are helpful when the relevant tools are available, but not required to complete tasks. Use whatever tools you have access to.

## 1. What is this repo?
`zramdedup` is a Rust-based userspace daemon that drives Linux's Kernel Samepage Merging (KSM) subsystem with a PSI-aware governor, deduplicates compressed swap pages, and exports Prometheus metrics. It reduces memory pressure in cloud, container, and embedded workloads.

## 2. How is it structured?
```
animaksm/
├── Cargo.toml [workspace]
├── crates/
│   ├── common/      # Shared: config, procfs, PSI, KSM, error
│   ├── daemon/      # zramdedup main binary
│   └── swap-proxy/  # Experimental deduplicating store
├── config/
│   └── zramdedup.toml  # Runtime configuration
├── systemd/            # Service units
├── Makefile            # check, test, coverage targets
```

## 3. How do I build/test/run?
```bash
# Mandatory commands
cargo build --workspace
make check          # cargo check + fmt + clippy
make test           # cargo test --workspace
make coverage       # HTML report in target/llvm-cov/html/
make coverage-ci    # LCOV → target/llvm-cov/lcov.info

# Exercise daemon locally
cargo run -p zramdedup-daemon -- run --config config/zramdedup.toml --dry-run
```

## 4. Tooling Tips (When Available)
- **Symbol-indexing LSP** (e.g., jCodeMunch): Rust analysis, symbol search, index freshness
- **`rg`/`grep`**: Quick text/identifier search
- **File read/edit tools**: Targeted file reads and surgical edits; avoid full-file slurping
- **Terminal/Shell**: Run build/test/coverage commands

## 5. ❗ Agent SOP — The Delegate-Verify Loop

**Follow this workflow for every code-change task:**

### Step 1: Analyze & Plan
- Identify relevant symbols/files and map affected areas
- Assess blast radius — understand downstream impact
- Break into **atomic steps** — tackle one step at a time

### Step 2: Delegate ONE Step (When Sub-Agent Tools Are Available)
- **Prefer delegation for code changes.** If a sub-agent tool is available, delegate edits there. Read-only tasks can be done directly.
- Include full context: repo (`animaksm`), target symbols, tool usage guidance
- Never bundle multiple steps into one delegation

### Step 3: **❗ Verify the Result (CRITICAL)**

**Subagents routinely claim success while omitting changes.** After every delegated task (or direct edit):

1. **Read the target file** — confirm the expected code is present; avoid relying solely on cached index reads
2. **Check call hierarchy / references** — confirm impact matches expectations
3. **Update indexes/caches** — re-index or invalidate caches if your tooling requires it
4. **Run tests** — `make test` or subcommand
5. **If wrong/missing**, re-delegate with **specific corrections** — never fix yourself silently

**Lesson learned — Coverage CI flake:**
The subagent for `TestFetchSubscriptionTimeout` claimed to add a `tokio::time::timeout` but only wrapped a subset of the futures. Missing coverage assertions were discovered during `make coverage-ci` — we now **read the test body** after every delegated test.

## 6. Git Rules

**Branch model**: GitHub flow — `main` branch, feature/user branches off `main`.

**Conventional Commits**:
```
feat(scope): description
fix(scope): description
test(scope): description
```

**Sync procedure**:
```bash
# 1. Branch
# 2. Edit → commit → push → PR
# 3. After merge → delete local + remote → rebase main
```

## 7. Testing Rules
- All tests live next to code (`#[cfg(test)]` mods)
- Async tests: `#[tokio::test]`
- Filesystem: use `tempfile` scoped to test
- **Before PR**: `make test` + `make coverage`
- **Known flake**: `TestFetchSubscriptionTimeout` (timing-sensitive)

## 8. Architecture Landmines
- **Kernel/sysfs**: `/sys/kernel/mm/ksm`, `/proc` — dry-run only in local dev
- **Systemd**: Service units in `systemd/` — document new capabilities
- **Config**: `config/zramdedup.toml` — keep defaults conservative

## 9. Secrets
No API keys/secrets in scope — dry-run mode is sufficient for local testing.

## 10. Rust/Component Gotchas
- Trait bounds must mesh with `anyhow`/`thiserror` conventions
- Async runtime: only Tokio v1 (installed in workspace)
- `unsafe` blocks: avoid outside KSM/scanner low-level kernel interactions