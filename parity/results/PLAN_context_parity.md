# PLAN — Context parity: port skills + memory injection into Hermes Air

Owner: Bear. Author: Alfred. Date: 2026-11-17.
Repo: `gb99xbear/hermes-air` (Oracle `~/hermes-rust-dev`), main = `20abdb1`.

## 1. Problem statement

Parity run (14 prompts, same model) showed Air at **0.09x** Python's prompt tokens
(323,739 vs 3,788,090 total) and structural match 8/14. Root cause found in code, not
guessed:

| Layer | Python Hermes | Hermes Air (now) |
|---|---|---|
| SOUL.md | push | push (`prompt.rs`) |
| AGENTS.md / system.md | push | push, if file exists |
| MEMORY.md / USER.md (files) | push | push (`prompt.rs`) |
| Memory store entries | push | **`MemoryStore::format_for_system_prompt()` EXISTS but is never called from `prompt.rs`** |
| Skills catalog | push (tier-1: name + truncated description, grouped by category) | **absent — no reference to skills in `prompt.rs`** |
| Skill bodies | pull (`skill_view`) | pull (`skill_view` tool exists) |
| Tool schemas | dozens | 3 (`config.toml`) |

Key finding: **Python itself pushes only a name+description index and pulls bodies via
`skill_view`.** So faithful parity is "push the index", not "push everything".

Second finding: Air's `SkillRegistry` IS wired as *tools* (`skills_list`, `skill_view` in
`tools.rs`), and `SkillManager::validate_frontmatter` already references "the
system-prompt description budget" — the catalog-in-prompt design was intended but never
connected. Also `profile.skills: Vec<String>` exists in `ProfileConfig` and is unused for
prompt building.

Third finding: our test profile had `skills = []` and no skills tools, so Air had **zero**
skills available by any path.

## 2. Target (Bear picks one)

| Option | Behaviour | Cost | Parity |
|---|---|---|---|
| A. Full push | mirror Python exactly | ~Python's 118k tok | exact |
| **B. Hybrid (recommended)** | push skills **index** + memory entries; bodies pull-only | moderate | **matches what Python actually does** |
| C. Pull-only (today) | skills via tools only | lowest | not parity |

Recommended: **B**, because Python is already a hybrid.

## 3. Work plan

### P0 - Harness fixes before any re-measure (prerequisite)
- [ ] Pass long prompts via **file**, not argv (`-q`/`-z` hit the 128 KiB `MAX_ARG_STRLEN`).
      Add `--prompt-file <path>` to Air, and use a stdin/file path on the Python side.
- [ ] Build a **multi-turn** driver so a session persists across prompts (one-shot cannot
      exercise session/compaction parity).
- [ ] Slash-command comparison via an interactive/REPL surface, not `-z`/`-q`.
- Files: `hermes-gateway` arg parsing (`gateway-bin/src/main.rs`), harness in
  `parity/` (`air_runner.py`, `py_runner.py`, `compare_parity.py`).

### P1 - Memory store injection (cheap, high value)
- [ ] In `crates/gateway-core/src/prompt.rs`, add sections built from
      `MemoryStore::format_for_system_prompt(target)` for the memory + user targets.
- [ ] Keep file-based MEMORY.md/USER.md sections; decide dedupe (store vs raw file).
- [ ] Config gate on `PromptConfig`.
- Test: prompt contains the store section when entries exist; absent when empty; prompt is
  byte-stable for the same inputs.

### P2 - Skills catalog injection (the big one)
- [ ] Add a skills section to `PromptAssembler`, built from `SkillRegistry`
      (`crates/skills/src/lib.rs`: `list()`, `get_by_category()`, `SkillListItem`).
- [ ] Format = Python tier-1: `name` + truncated `description`, grouped by category, with
      `category_descriptions`. Enforce the description budget
      (`SkillManager` already names it).
- [ ] Honour `ProfileConfig.skills` as an enable filter (define empty semantics: all vs none).
- [ ] **Prompt-cache safety:** build once per session and keep byte-stable; do not rebuild
      per turn (Python treats this as sacred).
- [ ] Config gate: `[prompt] skills_catalog = "full" | "names" | "off"`.
- Test: catalog present/format/budget; stable across turns; `skills = []` semantics.

### P3 - File discovery parity
- [ ] `prompt.rs` loads AGENTS.md only from `cwd` + `/home/ubuntu`. Python walks up from cwd
      to root and reads nested `AGENTS.md` with a size cap. Add walk-up + cap.
- [ ] Verify SOUL/MEMORY/USER candidate order matches Python's resolution.

### P4 - Tool surface decision
- [ ] Decide the target toolset for the profile. Today: 3 tools. Python: dozens.
      Minimum for parity of the tested prompts: `terminal`, `read_file`, `write_file`,
      `patch`, `search_files`, `skills_list`, `skill_view`, `memory`.
- [ ] Enable the skills + memory tools (`skills_list`/`skill_view` already exist in
      `tools.rs`; memory tool exported from `lib.rs`).

### P5 - Re-measure + gate
- [ ] Re-run the 14-prompt set with the fixed harness on both sides.
- [ ] Gate: structural parity (api_calls + tool_calls + finish_reason) target **>= 12/14**
      with P0/P1/P2 landed; report prompt-token ratio against a stated band, not equality
      (Air stays lighter by design where tools differ).
- [ ] Slash + multi-turn results reported separately from the one-shot set.

## 4. Risks

| Risk | Mitigation |
|---|---|
| Skills catalog re-inflates Air's prompt | Option B (index only) + `skills_catalog` gate |
| Prompt rebuilt per turn breaks caching | Build once per session, byte-stable, test it |
| Mass-behaviour change for public users | All gates default to current behaviour; parity is opt-in |
| Air's registry loads 379+ user skills | Budget + category grouping + cap, as Python does |
| Test-ordering drift (SkillsListResponse) | Change-detector-free tests (assert relations, not snapshots) |

## 5. Definition of done

1. `cargo fmt --all --check`, `cargo clippy`, `cargo test --workspace` green on CI.
2. Air's prompt contains: SOUL, AGENTS (walk-up), MEMORY, USER, memory store, skills index.
3. Parity re-run shows structural match >= 12/14 and the token gap explained line by line.
4. All new behaviour behind config, default off (or default on only for the parity profile).
5. Report committed to `parity/results/`.

## 6. Open questions for Bear

1. Option A / B / C for the context model?
2. Tool surface: keep Air minimal, or match Python's set for the parity profile?
3. Default for the gates on a public clone: off (light) or on (parity)?

## STATUS - P1..P4 landed

- **P1 memory** - DONE. Memory was already injected via the raw file read; the
  MemoryStore snapshot is now used instead (Hermes' header + char-usage format),
  never both. Live delta: +100 prompt tokens.
- **P2 skills index** - DONE. `## Skills` + per-category blocks, name + description
  truncated at 1024 chars (Hermes' `MAX_DESCRIPTION_LENGTH`); bodies stay pull-only.
  Live delta: +1,268 tokens with 15 skills on this host (Python's index is ~75k
  tokens at 379 skills - `compact_categories` remains the lever there).
- **P3 context files** - DONE. AGENTS.md is now a git-root -> cwd chain, per
  directory the first non-empty of `AGENTS.override.md` / `AGENTS.md` / `agents.md`,
  capped at `CONTEXT_FILE_MAX_CHARS = 20_000`. The hardcoded `/home/ubuntu/...`
  fallbacks are gone (they were personal paths in a public repo).
- **P4 tool surface** - DONE. `ProfileConfig.tools` was dead config (never read);
  the schema list is now filtered by it, unknown names are logged, and the agent-level
  `memory` tool - handled in dispatch but missing from the schema - is included.
  Live delta: 3 tools 7,800 tokens vs 19 tools 12,755 tokens.

Verified: 580 tests pass (10 new), fmt clean, live A/B on the built binary.
Config gates: `[prompt] inject_memory`, `skills_catalog = full|names|off`,
`skill_description_budget`.

Note for existing users: with the personal-path fallbacks removed, the doctrine
file is loaded from the working directory chain only - run the gateway from the
directory that holds `AGENTS.md`.

## STATUS - P5 parity re-run (after P1..P4)

14 prompts, same model (`river/deepseek-ai/DeepSeek-V4.1-Flash-1017`), one-shot both
sides, Air rebuilt from `f7596fb` with the full 19-tool profile and the parity gates on.

**Structural match (api_calls + tool_calls + finish_reason): 8/14 - below the >=12 gate.**

| metric | Python | Air | air/py |
|---|---|---|---|
| api_calls | 31 | 17 | 0.55 |
| tool_calls | 23 | 7 | 0.30 |
| prompt_tokens | 3,823,473 | 244,323 | 0.06 |
| output_tokens | 6,585 | 1,107 | 0.17 |
| latency_ms | 126,635 | 31,740 | 0.25 |
| mean latency | 9,045 ms | 2,267 ms | - |

### The six mismatches, by cause

1. **p11 / p12 (slash commands) - harness artifact, not a divergence.**
   `hermes -z "/model"` sends the text as a prompt (6-9 tool rounds); Air's `-q`
   parses it as a slash command (0 API calls). One-shot is the wrong harness for
   slash parity - needs a REPL/REPL-equivalent driver.
2. **p08_tool_search - reproducible Air error.** Twice, the *second* API round failed:
   `Failed to connect to local 9Router endpoint at http://127.0.0.1:20128/v1/chat/completions`.
   The first round used the CLI `-m` override fine; the follow-up call resolved a
   different (local) endpoint. This is a real bug in Air's post-tool-call call path,
   not a transient blip - it reproduced on the re-run.
3. **p02 / p03 / p07 - tool-loop depth.** Python takes more rounds for the same ask
   (p03: 4 api / 3 tools vs 2 / 1; p07: 3 / 2 vs 2 / 1). Air reaches `stop` with a
   shorter loop. Injected context does not change this: the loop depth follows the
   model's choices against the tool descriptions and guidance text.

### Conclusion

P1-P4 closed the **context** gap (memory snapshot, skills index, AGENTS chain, real
tool filtering) but **did not move structural parity**, and that is expected: the
remaining differences are behavioural, not contextual. The next levers are
(a) the p08 endpoint bug, (b) tool-description/guidance parity, (c) a slash-capable
harness. Prompt-token ratio moved to 0.06 partly because P3 stopped loading a doctrine
file that was outside the working-directory chain - correct parity, worth stating.
