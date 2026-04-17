# Built-in Tools

aaOS ships with 16 built-in tools. Each is capability-checked at invocation — agents see only the tools granted in their manifest, and even granted tools enforce their own capability checks (file path, workspace scope, etc.) at the call boundary. For the security model see [Architecture: Capability security model](architecture.md#capability-security-model); for the adding-your-own-tool path see the `Tool` trait in `aaos-tools/src/tool.rs`.

## Catalog

| Tool | Capability | Description |
|------|-----------|-------------|
| `echo` | `tool: echo` | Returns input (testing) |
| `web_fetch` | `WebSearch` | HTTP GET a URL. Streams chunks to `max_bytes` cap; rejects bodies > 10× cap by Content-Length. |
| `file_read` | `FileRead { path_glob }` | Read file with optional `offset` (1-indexed) + `limit` (default 2000 lines). Output is line-numbered (cat -n style). |
| `file_list` | `FileRead { path_glob }` | List directory contents — use before guessing filenames |
| `file_read_many` | `FileRead { path_glob }` (per file) | Batch-read up to 16 files in parallel; partial failures OK |
| `file_write` | `FileWrite { path_glob }` | Write file, path-checked |
| `file_edit` | `FileRead` + `FileWrite { path_glob }` | Surgical find/replace. Refuses non-unique `old_string` unless `replace_all: true`. |
| `grep` | `FileRead { path_glob }` | Regex search backed by ripgrep. 200-match cap, 16 KB inline output cap, 30 s timeout. |
| `spawn_agent` | `SpawnChild { allowed_agents }` | Spawn child with narrowed capabilities |
| `spawn_agents` | `SpawnChild { allowed_agents }` (per child) | Spawn up to 3 independent children concurrently; best-effort per-child, wall-clock = slowest child |
| `memory_store` | `tool: memory_store` | Store a fact/observation/decision/preference |
| `memory_query` | `tool: memory_query` | Semantic search over stored memories (cosine over embeddings) |
| `memory_delete` | `tool: memory_delete` | Delete a stored memory by ID |
| `skill_read` | `tool: skill_read` | Load skill instructions or reference files |
| `cargo_run` | `CargoRun { workspace }` | Run `cargo check/test/clippy/fmt` in a Rust workspace. Subcommand allowlisted — no `install`, `publish`, arbitrary subcommands. 4-minute timeout. |
| `git_commit` | `GitCommit { workspace }` | Run `git add` + `git commit` in a git repository. Subcommand allowlisted — no push/rebase/reset/checkout/config. Flag-injection guard on the message. Returns commit SHA. |

## The coding surface

The subset `file_read(offset, limit)`, `file_edit`, `file_list`, `grep`, `cargo_run`, `git_commit` makes an aaOS agent a capable coding agent: it can navigate an unfamiliar codebase, make surgical edits, verify them by running the test suite, and persist the result to version control — all under capability enforcement. This matches the working tool set of Claude Code, Cursor, and OpenCode. Runs 7–12 of the self-reflection log document how each of these primitives earned its place.

The `cargo_run` + `file_edit` pair in particular closes the self-build loop: the agent whose code is being edited can read its own plan, patch its own source, and `cargo test` its own tests against itself. Run 12 shipped `git_commit` to close the last unbroken human-in-the-loop step; see [`docs/reflection/2026-04-17-git-commit-tool.md`](reflection/2026-04-17-git-commit-tool.md) for the run narrative.
