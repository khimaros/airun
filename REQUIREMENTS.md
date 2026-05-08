# requirements

1. execute agents and skills from `.claude/` or `.opencode/` directory structure.
2. support markdown agent configs and skills.
3. read prompt from standard input (stdin).
4. write streaming response to standard output (stdout).
5. write reasoning tokens to standard error (stderr).
6. written in rust with minimal external dependencies.
7. support openai compatible endpoints, configurable via toml.
8. support external hook scripts via the v1 hook protocol (subprocess + JSONL),
   covering at minimum `discover`, `mutate_request`, `execute_tool`, and the
   observational `tool_before` / `tool_after` stages. host-specific stages
   tied to long-running sessions (`idle`, `heartbeat`, `compacting`,
   `recover`, `format_notification`, `observe_message`, `actions`) are
   explicitly out of scope; airun declares partial conformance.
