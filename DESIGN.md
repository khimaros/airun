# architecture and design

## overview

`airun` is built as a minimalist rust application using `rig-core` for LLM provider abstractions and `tokio` for async streaming.

## components

- **configuration management:** cascading config from `airun.toml` (project) to `~/.config/airun/config.toml` (global). supports multiple named providers with per-provider client type, API key, and base URL.
- **markdown parser:** extracts yaml frontmatter (via `serde_yaml`) from markdown agent and skill documents, separating config metadata from prompt body.
- **file locator:** walks up from cwd to git root searching `.opencode/`, `.claude/`, `.agents/` directories for agents and skills. falls back to global dirs (`~/.config/opencode/`, `~/.claude/`, `~/.agents/`).
- **tools:** pluggable tool system using the `rig` `Tool` trait. tools are enabled per-agent via frontmatter, globally via config, or exclusively via `--tools` CLI flag.
  - `read`: reads file contents. permissions use path-mode glob matching (`*` stops at `/`).
  - `bash`: executes commands via `sh -c`. permissions use command-mode glob matching (`*` matches anything). commands with shell metacharacters fall back to the catch-all permission.
- **permissions:** unified model shared across all tools. each tool maps to either a flat level (`allow`/`ask`/`deny`) or a glob pattern map. `ask` prompts the user via `/dev/tty`, bypassable with `--yes`. `check_tool_permission()` is the single entry point.
- **glob matching:** supports `*`, `**`, `?` with two modes: path mode (read tool, `*` stops at `/`) and command mode (bash tool, `*` matches anything). most specific pattern wins.
- **streaming client:** uses `rig-core` provider abstractions (openai, anthropic, gemini, cohere, xai) to stream chat completions. response text goes to stdout, reasoning tokens to stderr (dim italic), tool calls/results logged to stderr.

## dependencies

kept minimal:
- `rig-core`: LLM provider abstractions and tool framework
- `tokio`: async runtime
- `serde`, `serde_json`, `serde_yaml`: parsing structs
- `toml`: configuration parsing
- `clap`: CLI argument parsing
- `futures-util`: stream combinators
- `tracing-subscriber`: debug logging
