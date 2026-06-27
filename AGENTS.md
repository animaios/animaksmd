# AGENTS.md — Repository Guidelines for animaksm

**Agent Readiness:** This file contains two kinds of guidance:
- **Universal rules** — project structure, branching, architecture, and security patterns that apply regardless of what tooling you have.
- **Workflow recommendations** — tool-specific tips that are helpful when the relevant tools are available, but not required to complete tasks. Use whatever tools you have access to.

## 1. What is this repo?
`animaksm` is a Rust-based userspace daemon that drives Linux's Kernel Samepage Merging (KSM) subsystem with a PSI-aware governor, deduplicates compressed swap pages, and exports Prometheus metrics. It reduces memory pressure in cloud, container, and embedded workloads.

## 2. How is it structured?
```
animaksm/
├── Cargo.toml [workspace]
├── crates/
│   ├── common/      # Shared: config, procfs, PSI, KSM, error
│   ├── daemon/      # animaksm main binary
│   └── swap-proxy/  # Experimental deduplicating store
├── config/
│   └── animaksm.toml  # Runtime configuration
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
cargo run -p animaksm-daemon -- run --config config/animaksm.toml --dry-run
```

## 4. jcodemunch — Symbol Intelligence

Repo: `animaksm` (indexed). Symbol ID: `{file_path}::{qualified_name}#{kind}`

### 4.1 Core lookup
- `assemble_task_context(repo="animaksm", task="...")` — opening move; auto-classifies intent (explore/debug/refactor/extend/audit/review), surfaces symbols + ranked context
- `get_file_outline` → `get_symbol_source` / `get_context_bundle(symbol_ids=[...])` — targeted retrieval, never full files
- `search_symbols(repo="animaksm", query="...")` — find by name, signature, summary
  - `mode="context"` — query-less ranked context assembly
  - `mode="winnow"` — multi-axis constraint filter (kind, language, complexity, churn, etc.)
  - `semantic=true` — embedding-based search (requires embed provider)
- `search_text(repo="animaksm", query="...")` — full-text search across file contents (string literals, comments, configs)
- `search_ast(repo="animaksm", pattern="..." | category="...")` — structural anti-pattern scan (empty_catch, god_function, hardcoded_secret, etc.)

### 4.2 Impact & safety
- `get_blast_radius(symbol="...", include_source=true)` — check impact before changes
- `find_references` / `get_call_hierarchy` — trace who uses a symbol
- `check_safe(repo="animaksm", symbol="...", mode="edit"|"delete")` — composite preflight: can this symbol be safely edited/deleted?
- `plan_refactoring(repo="animaksm", symbol="...", refactor_type="rename"|"move"|"extract"|"signature")` — generate multi-file edit plan before refactoring
- `get_changed_symbols(repo="animaksm")` — map git diff to affected symbols
- `get_pr_risk_profile(repo="animaksm")` — unified risk assessment for a PR/branch

### 4.3 Repository intelligence
- `get_repo_health(repo="animaksm")` — one-call triage (dead code %, complexity, hotspots, cycle count)
- `get_repo_map(repo="animaksm")` — signature-level overview ranked by PageRank (cold-start orientation)
- `get_tectonic_map(repo="animaksm")` — logical module topology (hidden boundaries, misplaced files, drifters)
- `find_hot_paths(repo="animaksm")` — top-N symbols by runtime hit count (requires ingested traces)
- `get_dead_code_v2(repo="animaksm", min_confidence=0.67)` — multi-signal dead code detection
- `find_similar_symbols(repo="animaksm")` — cluster similar functions/methods (consolidation candidates)
- `get_symbol_provenance(repo="animaksm", symbol="...")` — git authorship lineage & evolution narrative
- `get_symbol_complexity(repo="animaksm", symbol_id="...")` — cyclomatic complexity, nesting, params
- `get_class_hierarchy(repo="animaksm", class_name="...")` — inheritance ancestors + descendants
- `find_implementations(repo="animaksm", symbol="...")` — find concrete impls of an interface/abstract
- `get_project_intel(repo="animaksm")` — auto-discover Dockerfiles, CI configs, deps, APIs
- `list_workspaces(repo="animaksm")` — enumerate monorepo workspace members
- `search_columns(repo="animaksm", query="...")` — search column metadata across indexed models

### 4.4 Runtime & indexing
- `import_runtime_signal(repo="animaksm", path="...", source="otel"|"sql_log"|"stack_log")` — ingest runtime traces
- `embed_repo(repo="animaksm")` — precompute symbol embeddings for semantic search
- `summarize_repo(repo="animaksm", force=true)` — re-run AI summarization pipeline
- `index_file(path="...")` — surgical single-file reindex after edits
- `index_folder(path="...")` / `index_repo(url="...")` — full index/reindex
- `register_edit(repo="animaksm", file_paths=[...], reindex=true)` — invalidate caches after file edits

### 4.5 Power User Guide

#### Golden Rules
1. **Always start with `assemble_task_context`** — it auto-classifies intent and returns ranked symbols + context in one call. Never manually hunt for entry points.
2. **Batch everything** — use `symbol_ids[]` in `get_context_bundle`, `get_symbol_source`, `search_symbols` instead of serial calls. Token budget is your friend.
3. **Verify with `verify=true` / `verify_against="git_sha"`** — catches index drift vs. working tree.
4. **Use `mode` switches** on `search_symbols`: `context` for query-less ranked context, `winnow` for multi-axis filters, `semantic=true` for embedding search.
5. **Prefer `get_context_bundle` over raw file reads** — deduplicates imports, respects token budget, returns ready-to-use context.

#### Common Workflows

##### Cold-start orientation (new repo / unfamiliar area)
```
get_repo_map(repo="animaksm", group_by="flat", top_n=30)     # Top symbols by PageRank
get_tectonic_map(repo="animaksm")                               # Logical module boundaries
get_repo_health(repo="animaksm", detailed=true)                 # Dead code %, complexity, cycles
```

##### Feature exploration — "How does X work?"
```
assemble_task_context(repo="animaksm", task="How does X work?")
# → returns ranked symbols + context
get_context_bundle(symbol_ids=[...], budget_strategy="core_first")
```

##### Refactoring safety (rename/move/extract)
```
check_safe(repo="animaksm", symbol="SymbolName", mode="edit")
plan_refactoring(repo="animaksm", symbol="SymbolName", refactor_type="rename", new_name="newName")
get_blast_radius(symbol="SymbolName", depth=2, include_source=true)
```

##### Dead code cleanup
```
get_dead_code_v2(repo="animaksm", min_confidence=0.67, file_pattern="crates/**")
find_similar_symbols(repo="animaksm", threshold=0.85, include_kinds=["function", "method"])
```

##### Performance hotspot triage
```
find_hot_paths(repo="animaksm", top_n=20)
get_repo_health(repo="animaksm", detailed=true, top_n=30)
get_symbol_complexity(repo="animaksm", symbol_id="...")
```

##### PR / change risk assessment
```
get_changed_symbols(repo="animaksm", include_blast_radius=true, max_blast_depth=3)
get_pr_risk_profile(repo="animaksm", base_ref="main", head_ref="HEAD")
```

##### Understanding unfamiliar code before modifying
```
get_symbol_provenance(repo="animaksm", symbol="SymbolName", max_commits=30)
get_call_hierarchy(symbol_id="...", direction="both", depth=3, include_impact=true)
find_implementations(repo="animaksm", symbol="InterfaceName", include_subclasses=true)
```

##### Finding config / string literals / comments (not symbols)
```
search_text(repo="animaksm", query="MAX_RETRIES", context_lines=3)
search_ast(repo="animaksm", category="security")              # hardcoded_secret, eval_exec
search_ast(repo="animaksm", pattern="string:/password/i")      # custom pattern
```

#### Parameter Cheatsheet

| Tool | Key params | When to use |
|---|---|---|
| `assemble_task_context` | `task`, `token_budget` (8k default) | **First call for any task** — returns intent, symbols, context |
| `search_symbols` | `mode`, `semantic`, `fusion`, `token_budget` | Symbol discovery; `mode=context` = ranked context w/o query |
| `get_context_bundle` | `symbol_ids[]`, `budget_strategy`, `token_budget` | Multi-symbol context in one call; `core_first` keeps primary symbol |
| `get_blast_radius` | `depth`, `include_source`, `include_depth_scores` | Pre-edit impact; `include_depth_scores` = per-hop risk |
| `check_safe` | `mode` (edit/delete), `include_runtime` | Preflight — returns verdict + top-5 blockers |
| `plan_refactoring` | `refactor_type`, `new_name`/`new_file`/`new_signature` | Returns `{old_text, new_text}` blocks ready for Edit tool |
| `get_repo_health` | `detailed`, `rules` (layer defs) | One-call triage; `detailed=true` adds cycles, coupling, hotspots |
| `get_tectonic_map` | `days`, `min_plate_size` | Module topology; finds drifters, nexus plates (coupled ≥4) |
| `find_similar_symbols` | `threshold`, `semantic_weight`, `include_tests` | Consolidation candidates; `semantic_weight=0.6` default |
| `get_symbol_provenance` | `max_commits` | Authorship lineage + evolution narrative |
| `search_ast` | `category`, `pattern`, `language` | Anti-pattern sweep; `category=all` runs everything |
| `get_changed_symbols` | `since_sha`, `until_sha`, `include_blast_radius` | Maps git diff → symbols + downstream impact |
| `get_pr_risk_profile` | `base_ref`, `head_ref`, `days` | Composite risk score (blast + complexity + churn + tests + volume) |

#### Anti-patterns to Avoid
- ❌ Reading full files with `read_file` — use `get_context_bundle` or `get_symbol_source`
- ❌ Calling `search_symbols` repeatedly — batch with `symbol_ids[]` in `get_context_bundle`
- ❌ Skipping `check_safe` before edits/deletes — 5s call prevents hours of revert
- ❌ Not verifying with `verify=true` — index can drift from working tree
- ❌ Using `grep` for symbol lookup — `search_symbols` understands signatures, imports, types
- ❌ Manual blast radius tracing — `get_blast_radius(depth=2, include_source=true)` is instant

#### Pro Tips
- **`fusion=true` on `search_symbols`** — uses Weighted Reciprocal Rank across lexical/structural/similarity/identity channels; best for vague queries
- **`budget_strategy="compact"`** on `get_context_bundle` — returns signatures only (min tokens), great for call-chain mapping
- **`include_decisions=true`** on `get_blast_radius` / `get_call_hierarchy(include_impact=true)` — surfaces git commit intent (revert/perf/refactor/bugfix) from history
- **`embed_repo(repo="animaksm")` once** — then `semantic=true` on `search_symbols` works instantly for semantic queries
- **`index_file` after every edit** — keeps index fresh for subsequent tool calls in same session
- **`cross_repo=true`** on `get_blast_radius` / `find_references` — finds consumers in other indexed repos

#### Token Budget Discipline
- `assemble_task_context(token_budget=4000)` for focused tasks
- `get_context_bundle(token_budget=6000, budget_strategy="core_first")` for multi-symbol context
- `search_symbols(token_budget=3000)` with `detail_level="compact"` for broad discovery (15 tokens/row)
- Always check `_meta.tokens_used` / `_meta.tokens_remaining` in responses

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
- **Config**: `config/animaksm.toml` — keep defaults conservative

## 9. Secrets
No API keys/secrets in scope — dry-run mode is sufficient for local testing.

## 10. Rust/Component Gotchas
- Trait bounds must mesh with `anyhow`/`thiserror` conventions
- Async runtime: only Tokio v1 (installed in workspace)
- `unsafe` blocks: avoid outside KSM/scanner low-level kernel interactions