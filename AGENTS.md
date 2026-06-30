# AGENTS.md — Repository Guidelines for animaksm

**Agent Readiness:** This file contains two kinds of guidance:
- **Universal rules** — project structure, branching, architecture, and security patterns that apply regardless of what tooling you have.
- **Workflow recommendations** — tool-specific tips that are helpful when the relevant tools are available, but not required to complete tasks. Use whatever tools you have access to.

**Tool-Precedence Pledge (binding):** Whenever Serena and/or jcodemunch are available, the agent MUST use them for every symbol/intent/impact operation. Native tools (`Read`, `Grep`, `Glob`, `Edit`, `Create`, `LS`) are reserved for side-effects, raw-text editing, and OS-level operations (git, cargo, systemctl, kernel files). A native tool marked "forbidden" in §3.5 may only run after the specialized tool returned an error or empty result AND the fallback reason is recorded in the active TodoWrite entry.

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

## 3.5 Tool-Precedence Matrix

The order in which the agent attempts tools is **binding**. "Forbidden" native tools may only run when the specialized tool returned an error/empty result AND the fallback reason is recorded in the active TodoWrite entry.

| Intent | First tool | Fallback (only if first errors/empty) | Forbidden native |
|---|---|---|---|
| Locate a symbol (fn/struct/enum/trait) by name or signature | Serena `find_symbol` | jcodemunch `search_symbols` | `Grep` on source |
| Show a symbol's body, params, docs | Serena `get_symbol_contents` | jcodemunch `get_symbol_source` | `Read` of the whole file |
| Find callers / references of a symbol | Serena `find_referencing_symbols` | jcodemunch `get_blast_radius` | `Grep` for the name |
| Rename / move / extract / edit a symbol with line anchor | Serena `perform_rename` + Serena `replace_content` via `find_symbol` output | jcodemunch `plan_refactoring` for diff hints | Bulk `Edit` without line anchor from Serena |
| Full-text / comment / config / string-literal search (non-symbol) | jcodemunch `search_text` | native `Grep` (literal or regex) | native `Grep` when index is warm |
| Repo topology / top-N hot paths / dead-code triage | jcodemunch `get_repo_map` / `find_hot_paths` / `get_dead_code_v2` | jcodemunch `assemble_task_context` | nested `LS`/`Read` traversal |
| Cross-file context bundle for N symbols | jcodemunch `get_context_bundle` | `get_symbol_source` | Serial `Read` per file |
| Create a brand-new file | Serena `create_text_file` (when available) else native `Create` | — | — |
| Delete / trash a file or directory | native tool (careful!). Serena delete if available | — | — |
| Run cargo / make / systemctl / edit kernel pseudo-files | native `Execute` | — | — |
| Interact with a TUI / browser / desktop app | `droid-control` / `agent-browser` skill tools | native `Execute` only as last resort | — |
| Test assertions in existing tests, non-symbolic Cargo.toml/config/unit edits | native `Edit` (allowed: no symbol anchor needed) | — | — |

> **Service-level rule:** "If you can name a symbol, Serena owns it. If you need a bag of symbols or repo intel, jcodemunch owns it. Native tools only for side effects, raw-text editing, and OS-level commands."

## 4. jcodemunch — Symbol Intelligence

Repo: `animaksm` (indexed). Symbol ID: `{file_path}::{qualified_name}#{kind}`

### 4.1 Core lookup
- `resolve_repo(path=".")` — confirm repo is indexed. If not: `index_folder(path=".")`
- `assemble_task_context(repo="animaksm", task="...")` — opening move; auto-classifies intent (explore/debug/refactor/extend/audit/review), surfaces symbols + ranked context
- `get_file_outline` → `get_symbol_source` / `get_context_bundle(symbol_ids=[...])` — targeted retrieval, never full files
- `search_symbols(repo="animaksm", query="...")` — find by name, signature, summary
  - `mode="context"` — query-less ranked context assembly
  - `mode="winnow"` — multi-axis constraint filter (kind, language, complexity, churn, etc.)
  - `semantic=true` — embedding-based search (requires embed provider)
  - `detail_level="compact"` — 15 tokens/row (id/name/kind/file/line only), ideal for broad discovery
- `search_text(repo="animaksm", query="...")` — full-text search across file contents (string literals, comments, configs)
- `search_ast(repo="animaksm", pattern="..." | category="...")` — structural anti-pattern scan (empty_catch, god_function, hardcoded_secret, etc.)

### 4.2 Impact & safety
- `get_blast_radius(symbol="...", include_source=true)` — check impact before changes
  - `call_depth=N` — also find symbols that *call* this symbol (call-level analysis, max 3)
  - `include_decisions=true` — surface git commit intent (revert/perf/refactor/bugfix) from history
  - `source_budget=N` — max tokens for source snippets across all files (default 8000)
- `find_references(identifier="...", cross_repo=false)` — trace who uses a symbol
  - `mode="importers"` — find files importing a given file (former `find_importers`)
  - `mode="related"` — find symbols related to a given symbol (former `get_related_symbols`)
  - `quick=true` — lightweight `is_referenced` boolean check for fast dead-code detection
- `get_call_hierarchy(symbol_id="...")` — incoming callers / outgoing callees
  - `chains=true` — also discover signal chains (HTTP routes, CLI commands, events)
  - `kind="http"|"cli"|"event"|"task"|"main"|"test"` — filter chain gateways
  - `max_depth=N` — BFS depth limit per chain (1–8, default 5)
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
  - `force=true` — recompute all embeddings even if they already exist
  - `batch_size=N` — symbols per embedding batch (default 50)
- `summarize_repo(repo="animaksm", force=true)` — re-run AI summarization pipeline
- `index_file(path="...")` — surgical single-file reindex after edits
- `index_folder(path="...")` / `index_repo(url="...")` — full index/reindex
- `register_edit(repo="animaksm", file_paths=[...], reindex=true)` — invalidate caches after file edits
- `get_repo_map(repo="animaksm", mode="outline")` — lightweight directory/language/symbol count overview

### 4.5 Code Exploration Policy

**Always use the specialized toolbox (Serena + jcodemunch) for code navigation. Never fall back to native tools for exploration unless the matrix in §3.5 explicitly permits it.**

- Use Serena `find_symbol` + `get_symbol_contents` for reading/editing any symbol (owning symbol-level edits).
- Use Serena `create_text_file` / `replace_content` / `perform_rename` for symbol-safe writes.
- Use jcodemunch `search_symbols`, `get_context_bundle`, `get_symbol_source` for indexed symbol retrieval.
- Use jcodemunch `assemble_task_context` as your opening move — it auto-classifies intent and returns ranked context.
- Use jcodemunch `search_text` only for non-symbol content (string literals, comments, config values).

**Native-tool decision (always route through §3.5):**
- `Edit`/`Create` allowed ONLY for non-symbolic bulk fixes (tests, Cargo.toml, systemd units, kernel pseudo-files) OR as a fallback documented in TodoWrite.
- `Grep`/`Glob`/`Read`/`LS` allowed ONLY as fallbacks when the specialized tool returned an error/empty result AND the fallback reason is recorded in the active TodoWrite entry.

Rationale: native tools scan full files with no structural context. The index understands signatures, imports, types, and call graphs — flat text search wastes tokens and produces poor decisions.

### 4.6 Session-Aware Routing — Confidence & Negative Evidence

After every jCodemunch tool call, check the response envelope before deciding what to do next:

| `_meta.confidence` | Action |
|---|---|
| **high** (≥ 0.7) | Act directly on the result. Max 2 supplementary `read_file` calls for edit verification. |
| **medium** (0.4–0.69) | Explore the recommended files. Max 5 `read_file` calls; then commit or report. |
| **low** (< 0.4) | **Do not keep searching.** Report the gap, suggest re-indexing, and ask the user for direction. |

**Negative evidence — stop, don't re-search:**
- If a search returns `verdict: "no_implementation_found"`, **stop.** Do not re-search with different terms, different capitalization, or broader patterns. An absent implementation is a valid finding — report it.
- If `resolve_repo` or `list_repos` shows the repo is not indexed, **index first** (`index_folder`), then retry. Do not work around a missing index with `grep`.
- If `_meta.freshness` indicates stale data or `repo_is_stale=true`, suggest `index_folder` before trusting the results.

### 4.7 After Editing Files

**VERBATIM RULE — run after EVERY edit (Serena or native):**

1. jcodemunch `register_edit` invalidates the BM25 cache + search result cache so subsequent `search_symbols` / `search_text` calls see the new content.
2. Serena's LSP already tracks edits; you MUST re-issue `get_symbol_contents` / `find_symbol` if you need the edited body again later — never reuse the pre-edit Serena output captured earlier in the session.

```
register_edit(repo="animaksm", file_paths=["crates/common/src/foo.rs"], reindex=true)
```

For surgical reindex of a single file (lighter than full `register_edit`):
```
index_file(path="/absolute/path/to/crates/common/src/foo.rs")
```

### 4.8 Serena tool surface

Serena is configured for this project (Rust LSP). It owns **all symbol-level read/write operations**. Native `Edit`/`Create` is only for non-symbolic bulk fixes and OS-level commands.

| Serena tool | When to use |
|---|---|
| `list_dir`, `find_file` | Orientation and path discovery (vscode-style globs for `find_file`) |
| `find_symbol` | Locate a symbol by name/sig with line anchor — the required first step before any edit |
| `get_symbol_contents` | Read one symbol's body + docs + params; used for verification after edits |
| `find_referencing_symbols` | Caller / reference analysis (mirrors `get_blast_radius`) |
| `create_text_file`, `replace_content`, `perform_rename` | Symbol-safe writes with LSP diagnostics; prefer over native `Edit` for Rust code |
| `read_memory` / `write_memory` | Episodic memory across activation (preferred over re-deriving context) |
| `search_for_pattern` | Repo-wide regex scan when `search_symbols` is not specific enough |

> **Invariant:** once you edit a file (via any tool), ALL prior Serena outputs for that file are considered stale. Re-issue Serena reads to get fresh structure.

### 4.9 Tool-Precedence Compliance

A task step is **incomplete** until:
1. The fallback chain in the §3.5 matrix is exhausted in order.
2. Every fallback reason is recorded in the active TodoWrite entry.
3. `register_edit` (§4.7) has been run if a file was modified.
4. Serena has been re-queried for any symbol whose body you will reason about post-edit.

Failure to follow the matrix without documenting the reason is a compliance violation in code review.

### 4.10 Interpreting Search Results

jCodemunch responses include metadata fields that inform decision-making:

| Field | Meaning | Action |
|---|---|---|
| `_meta.confidence` | Result quality score (0.0–1.0) | See §4.6 routing table |
| `_meta.freshness` | Index staleness indicator | If stale, suggest `index_folder` |
| `_meta.tokens_used` / `_meta.tokens_remaining` | Token budget consumption | Adjust budget on next call if exhausted |
| `verdict: "no_implementation_found"` | No matching implementation exists | **Stop searching** — report the gap |
| `repo_is_stale` | Index was built from an older commit | Re-index before trusting blast radius / reference data |
| `source_truncated: true` | Symbol body was truncated (bounded mode) | Use `get_symbol_source` without bounds if you need the full body |

### 4.11 Power User Guide

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
| `search_symbols` | `mode`, `semantic`, `fusion`, `token_budget`, `detail_level` | Symbol discovery; `mode=context` = ranked context w/o query |
| `get_context_bundle` | `symbol_ids[]`, `budget_strategy`, `token_budget` | Multi-symbol context in one call; `core_first` keeps primary symbol |
| `get_blast_radius` | `depth`, `include_source`, `include_depth_scores`, `call_depth`, `include_decisions`, `source_budget` | Pre-edit impact; `include_depth_scores` = per-hop risk |
| `check_safe` | `mode` (edit/delete), `include_runtime` | Preflight — returns verdict + top-5 blockers |
| `plan_refactoring` | `refactor_type`, `new_name`/`new_file`/`new_signature` | Returns `{old_text, new_text}` blocks ready for Edit tool |
| `get_repo_health` | `detailed`, `rules` (layer defs) | One-call triage; `detailed=true` adds cycles, coupling, hotspots |
| `get_tectonic_map` | `days`, `min_plate_size` | Module topology; finds drifters, nexus plates (coupled ≥4) |
| `find_similar_symbols` | `threshold`, `semantic_weight`, `include_tests` | Consolidation candidates; `semantic_weight=0.6` default |
| `get_symbol_provenance` | `max_commits` | Authorship lineage + evolution narrative |
| `search_ast` | `category`, `pattern`, `language` | Anti-pattern sweep; `category=all` runs everything |
| `get_changed_symbols` | `since_sha`, `until_sha`, `include_blast_radius` | Maps git diff → symbols + downstream impact |
| `get_pr_risk_profile` | `base_ref`, `head_ref`, `days` | Composite risk score (blast + complexity + churn + tests + volume) |
| `find_references` | `mode` (refs/importers/related), `quick`, `include_call_chain` | Import sites + dbt refs + call chain; `quick=true` for dead code |
| `get_call_hierarchy` | `chains`, `kind`, `max_depth`, `include_impact` | Call graph + signal chains (HTTP/CLI/event) |
| `embed_repo` | `force`, `batch_size` | Warm embedding cache for semantic search |
| `get_repo_map` | `mode` (map/outline), `group_by` (file/flat), `top_n` | Cold-start orientation; `mode=outline` = lightweight overview |
| `resolve_repo` | `path` | First call in new workspace — confirm repo is indexed |

#### Anti-patterns to Avoid
- ❌ Reading full files with native tools — use Serena `get_symbol_contents` or jcodemunch `get_context_bundle` / `get_symbol_source`
- ❌ Grep-first lookup of symbols — Serena `find_symbol` owns it; native `Grep` is forbidden until Serena + jcodemunch both error/empty
- ❌ Serial `Read` per file — batch with `symbol_ids[]` in `get_context_bundle`; for symbol groups, `find_symbol` + `get_symbol_contents` pattern
- ❌ Reusing pre-edit Serena output after an edit — re-issue `get_symbol_contents` (invariant in §4.8)
- ❌ Skipping `check_safe` before edits/deletes — 5s call prevents hours of revert
- ❌ Not verifying with `verify=true` — index can drift from working tree
- ❌ Manual blast radius tracing — `find_referencing_symbols` or `get_blast_radius(depth=2, include_source=true)` is instant
- ❌ Ignoring `_meta.confidence` < 0.4 — low confidence means widen the search or report a gap, not proceed as-is
- ❌ Editing without the §3.5 matrix anchor — you must have a Serena line anchor before any edit to a symbol
- ❌ Forgetting `register_edit` after any file change — invalidates BM25 + search caches (§4.7)

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

## 5. ❗ Agent SOP — Serena+jcodemunch Delegate-Verify Loop

**Follow this workflow for every code-change task. The order is binding — every intent flows through the §3.5 precedence matrix.**

### Step 1: Analyze & Plan
1. `assemble_task_context(repo="animaksm", task="…")` — auto-classifies intent, returns ranked symbols + context.
2. Pin line anchors: Serena `find_symbol(name, scope)` + `get_symbol_contents` for each candidate. Do not reason about a symbol you haven't anchored through Serena/index.
3. Map blast radius: Serena `find_referencing_symbols` or jcodemunch `get_blast_radius`. Use `get_call_hierarchy` for chains.
4. Break into **atomic TodoWrite steps** — one symbol/file per step. Record the target symbol ID (`{file}::{qualified_name}#{kind}`) in each step.

### Step 2: Delegate ONE Step
- **Serena owns the edit.** Delegate writes to Serena `replace_content` / `create_text_file` / `perform_rename` with the line anchors from Step 1. Sub-agents MUST continue calling Serena until the write is done or they record a documented fallback reason in TodoWrite.
- Native `Edit` allowed ONLY for non-symbolic bulk fixes (tests, Cargo.toml, systemd units, config). Sub-agents must still mark the TodoWrite reason.
- Read-only steps (exploration, blast radius) use Serena + jcodemunch; never native full-file reads while anchored on a known symbol.
- Include full context in the delegation: repo (`animaksm`), target symbol ID, §3.5 matrix reference, §4.7 cache-invalidation requirement.

### Step 3: **❗ Verify the Result (CRITICAL)**

**Subagents routinely claim success while omitting changes.** After every delegated task:

1. **Serena re-read:** `get_symbol_contents` for every symbol the sub-agent claimed to touch. Confirm the new body matches expectation; never trust the pre-edit Serena output captured earlier in the session (invariant in §4.8).
2. **Index cache invalidation (§4.7):** run `register_edit(repo="animaksm", file_paths=[…], reindex=true)` for every modified file. Without this, later `search_symbols` / `search_text` return stale data.
3. **Call hierarchy check:** `find_referencing_symbols` or `get_call_hierarchy` to confirm impact matches Step 1.
4. **Build/TYPECHECK:** `make check` first (cheap surface: cargo check + clippy + fmt).
5. **Run tests:** `make test` (whole workspace) or targeted `cargo test -p <crate>`.
6. **Coverage before PR:** `make coverage` + `make coverage-ci`.
7. **If wrong/missing:** re-delegate the specific failing anchor with the §3.5 fallback-reason line and the exact diff expected — never fix yourself silently.

**Compliance gate (§4.9):** a step is incomplete until the matrix fallback chain is exhausted, the fallback reason is recorded in TodoWrite, `register_edit` has been run, and Serena has been re-queried for post-edit bodies.

**Lesson learned — Coverage CI flake:**
The subagent for `TestFetchSubscriptionTimeout` claimed to add a `tokio::time::timeout` but only wrapped a subset of the futures. Missing coverage assertions were discovered during `make coverage-ci` — we now **Serena-re-read the test body** after every delegated test.

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