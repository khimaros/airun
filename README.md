# airun

`airun` is a CLI tool written in Rust that executes AI agents and skills defined in Markdown files from `.opencode/`, `.claude/`, or `.agents/` directory structures. It pipes the prompt from stdin to an OpenAI-compatible endpoint, streaming the standard response back to stdout and reasoning tokens to stderr.

## configuration

you can initialize a default configuration file by running:

```bash
airun --init
```

this creates a file at `~/.config/airun/config.toml` (you can also place it in your project directory at `airun.toml`):

```toml
# base_url = "https://api.openai.com/v1" # optional, defaults to openai
api_key = "your-api-key"
# model = "gpt-4o" # optional default model
```

## agents and skills

agents are stored in `.opencode/agents/`, `.claude/agents/`, or `.agents/agents/` as markdown files with yaml frontmatter:

`.claude/agents/admin.md`
```markdown
---
description: performs system administration tasks
model: gpt-4
skills:
  - system-maintenance
---

you are a system administrator...
```

skills are stored in `.opencode/skills/<skill_name>/SKILL.md` (or similarly for `.claude/` and `.agents/`). they can also use yaml frontmatter.

### discovery

`airun` walks up from your current working directory until it reaches the git worktree (where a `.git` folder exists). It searches for agents and skills in any `.opencode/`, `.claude/`, or `.agents/` directories along the way.

If no local definitions are found, it falls back to global definitions at:
- `~/.config/opencode/`
- `~/.claude/`
- `~/.agents/`

## usage

you can pipe your prompt into `airun`:

```bash
echo "what time is it?" | airun my-agent
```

or use the `-p` / `--prompt` argument for simpler inline queries:

```bash
airun admin -p "update the debian system" | bash
```

the response stream is written to stdout, and any reasoning tokens (from compatible models like deepseek) are written to stderr.
