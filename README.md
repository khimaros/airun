# airun

`airun` is a CLI tool written in Rust that executes AI agents and skills defined in Markdown files from `.opencode/`, `.claude/`, or `.agents/` directory structures. it supports multiple LLM providers (OpenAI, Anthropic, Gemini, Cohere, xAI, and OpenAI-compatible endpoints), streaming responses to stdout and reasoning tokens to stderr.

## configuration

initialize a default configuration file:

```bash
airun --init
```

this creates `~/.config/airun/config.toml`. config files are searched in order:
1. `airun.toml` (project root)
2. `.airun.toml` (project root)
3. `.config/airun.toml` (project root)
4. `~/.config/airun/config.toml` (global)

example configuration:

```toml
default_model = "anthropic/claude-sonnet-4-20250514"
default_max_tokens = 16384

[[providers]]
name = "anthropic"
client = "anthropic"
api_key = "sk-ant-..."

[[providers]]
name = "openai"
client = "openai"
api_key = "sk-..."

[[providers]]
name = "openai-compatible"
client = "openai_completions"
api_key = "fake"
base_url = "http://localhost:7860/v1"
```

supported client types: `openai` (responses API), `openai_completions`, `anthropic`, `gemini`, `cohere`, `xai`.

if no API key is set in the config, `airun` falls back to the corresponding environment variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `COHERE_API_KEY`, `XAI_API_KEY`).

### tools and permissions

tools can be registered with the LLM via config or the `--tools` flag. supported tools:
- `read` — read file contents at a given path
- `bash` — execute shell commands via `sh -c`

```toml
[tools]
read = false
bash = false

[permissions]
read = "allow"
bash = "deny"

# or with gitignore-style glob patterns (most specific match wins):
# [permissions.bash]
# "**" = "deny"
# "apt update" = "allow"
# "apt clean" = "allow"

# [permissions.read]
# "**" = "deny"
# "/home/**" = "allow"
# "/etc/shadow" = "deny"
```

permission levels:
- `allow` — permitted without prompting
- `ask` — prompts the user for confirmation via `/dev/tty`
- `deny` — blocked silently

patterns support glob syntax: `**` matches any characters (including `/`), `?` matches a single character. for file-path tools (read), `*` matches within a single path segment (stops at `/`). for command tools (bash), `*` matches any characters including `/`. the most specific (fewest wildcards) matching pattern wins.

a trailing ` *` or ` **` in a pattern also matches the command with no arguments (e.g. `"ls *"` matches both `ls` and `ls -la /home`).

when a bash command contains shell metacharacters (pipes, redirects, chaining, etc.), it cannot be safely matched against patterns, so the catch-all (`*`) permission level is used instead.

## agents and skills

agents are stored in `.opencode/agents/`, `.claude/agents/`, or `.agents/agents/` as markdown files with yaml frontmatter:

`.agents/agents/admin.md`
```markdown
---
description: "perform system administration tasks"
tools:
  read: true
  bash: true
permissions:
  read:
    "**": deny
    "/etc/os-release": allow
  bash:
    "*": deny
    "apt update": allow
    "apt clean": allow
skills:
  - system-maintenance
---

you are an expert systems administrator.
```

agent frontmatter supports: `description`, `model`, `tools`, `permissions`, `skills`. agent settings override global config.

skills are stored as `<skill_name>/SKILL.md` or `<skill_name>.md` in the `skills/` subdirectory of `.opencode/`, `.claude/`, or `.agents/`. they can also use yaml frontmatter.

### discovery

`airun` walks up from your current working directory until it reaches the git worktree root (where `.git` exists), searching for agents and skills in `.opencode/`, `.claude/`, or `.agents/` directories along the way.

if no local definitions are found, it falls back to global definitions at:
- `~/.config/opencode/`
- `~/.claude/`
- `~/.agents/`

## usage

```
airun [OPTIONS] [AGENT_NAME]
```

pipe a prompt via stdin:

```bash
echo "what time is it?" | airun my-agent
```

or use `-p` / `--prompt` for inline queries:

```bash
airun admin -p "update the debian system" | bash
```

override the model for a single invocation:

```bash
echo "hello" | airun -m gemini/gemini-2.0-flash my-agent
```

run without an agent (bare model query):

```bash
echo "explain quicksort" | airun -m openai/gpt-4o
```

### options

| flag | description |
|------|-------------|
| `--init` | initialize a default configuration file |
| `-p, --prompt <PROMPT>` | prompt text (alternative to stdin) |
| `-s, --system-prompt <PROMPT>` | override the system prompt |
| `-m, --model <MODEL>` | override model (`<provider>/<model>`) |
| `-t, --max-tokens <N>` | maximum output tokens (default: 16384) |
| `--tools <LIST>` | enable specific tools exclusively (comma-separated, e.g. `read,bash`) |
| `--skills <LIST>` | use specific skills exclusively (comma-separated, overrides agent) |
| `-n, --dry-run` | print what would be sent to the LLM and exit |
| `-y, --yes` | auto-accept "ask" permission prompts |
| `-v, --verbose` | enable verbose/debug logging |
| `--list-agents` | list discovered agents |
| `--list-skills` | list discovered skills |
| `--list-tools` | list available tools |
| `--list-providers` | list configured providers |

the response stream is written to stdout. reasoning tokens (from compatible models) are written to stderr in dim italic. tool calls and results are logged to stderr.

## license

GPLv3
