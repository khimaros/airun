# airun

unix philosophy for the agentic future. pipe a prompt in, get a streaming response out.

`airun` is a simple, one-shot agent runner compatible with `.claude/` and `.opencode/` directory structures. agents and skills are plain markdown files with yaml frontmatter — no frameworks, no daemons, no lock-in.

supports OpenAI, Anthropic, Gemini, Cohere, xAI, and OpenAI-compatible endpoints. written in Rust with minimal dependencies.

## quick start

```bash
# install
cargo install --path .

# create a config with your API key
airun --init
$EDITOR ~/.config/airun/config.toml

# ask a question (bare model query)
echo "explain quicksort in one sentence" | airun

# use an agent with an inline prompt
airun admin "update the debian system"

# pipe input to an agent
echo "what time is it?" | airun my-agent

# pipe agent output to a shell
airun admin "list installed packages" | bash
```

## demo

```
$ airun admin "check disk usage"
df -h --output=target,pcent,avail /

$ airun admin "check disk usage" | bash
Filesystem  Use% Avail
/           42%  120G
```

agents have scoped tools and permissions, so `admin` can run `bash` commands while other agents cannot. the response streams to stdout, tool calls log to stderr — composable with standard unix pipes and redirects.

## agents and skills

agents live in `.agents/agents/`, `.claude/agents/`, or `.opencode/agents/` as markdown with yaml frontmatter:

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

`airun` walks up from your current working directory to the git worktree root, searching for agents and skills in `.opencode/`, `.claude/`, or `.agents/` directories along the way.

if no local definitions are found, it falls back to global definitions at `~/.config/opencode/`, `~/.claude/`, or `~/.agents/`.

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

## usage

```
airun [OPTIONS] [AGENT_NAME] [PROMPT]
```

the prompt can be provided as a positional argument after the agent name, via `-p`/`--prompt`, or piped through stdin.

### options

| flag | description |
|------|-------------|
| `--init` | initialize a default configuration file |
| `-p, --prompt <PROMPT>` | prompt text (alternative to positional arg or stdin) |
| `-s, --system-prompt <PROMPT>` | override the system prompt |
| `-m, --model <MODEL>` | override model (`<provider>/<model>`) |
| `-t, --max-tokens <N>` | maximum output tokens (default: 16384) |
| `--tools <LIST>` | enable specific tools exclusively (comma-separated, e.g. `read,bash`) |
| `--skills <LIST>` | use specific skills exclusively (comma-separated, overrides agent) |
| `-n, --dry-run` | print what would be sent to the LLM and exit |
| `-q, --quiet` | suppress thinking and tool call output on stderr |
| `-y, --yes` | auto-accept "ask" permission prompts |
| `-v, --verbose` | enable verbose/debug logging |
| `--list-agents` | list discovered agents |
| `--list-skills` | list discovered skills |
| `--list-tools` | list available tools |
| `--list-providers` | list configured providers |

the response stream is written to stdout. reasoning tokens (from compatible models) are written to stderr in dim italic. tool calls and results are logged to stderr.

## license

GPLv3
