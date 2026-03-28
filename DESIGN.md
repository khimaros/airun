# architecture and design

## overview

`airun` is built as a minimalist rust application. the execution flow relies heavily on async processing with `tokio` and streams with `reqwest`. 

## components

- **configuration management:** loading of global `~/.config/airun/config.toml` or local `airun.toml`. contains api key, default model, and the api endpoint URL.
- **markdown parser:** a custom parser extracting yaml frontmatter (using `serde-yaml`) from markdown agent and skill documents, allowing separation of config metadata and prompt payload.
- **file locator:** searches the workspace for `.claude/` or `.opencode/` directories to resolve agent and skill files, prioritizing local configurations.
- **streaming client:** handles `POST` requests to openai compatible chat completion endpoints. it parses SSE (`server-sent events`) chunks explicitly via string splitting to identify standard deltas and `reasoning_content` deltas, directing output to `stdout` and `stderr` respectively.

## dependencies

kept minimal to satisfy the initial requirement:
- `tokio`: async runtime
- `reqwest`: async http client
- `serde`, `serde_json`, `serde_yaml`: parsing structs
- `toml`: configuration parsing
