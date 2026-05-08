#!/bin/sh
# example hook for airun. logs every stage invocation along with its
# stdin payload, then exits cleanly without modifying the request.
#
# install: place under .agents/hooks/ (or .claude/hooks/, .opencode/hooks/)
# and `chmod +x`. verify with `airun --list-hooks`.
#
# stage is in argv[1]; the host writes a single json object on stdin
# (terminated by EOF). lines emitted with `{"log": "..."}` are routed
# to airun's stderr and are not merged into the hook result.

stage="$1"
payload=$(cat)

# escape for embedding inside a json string literal.
escaped=$(printf '%s' "$payload" | awk 'BEGIN{ORS=""} {gsub(/\\/,"\\\\"); gsub(/"/,"\\\""); gsub(/\t/,"\\t"); print; print "\\n"}')
printf '{"write": "stage=%s payload=%s"}\n' "$stage" "$escaped"

case "$stage" in
    discover)
        # register one custom tool: observe_write <content>.
        cat <<'EOF'
{"name": "observe", "tools": [{"name": "write", "description": "write an observation to stderr", "parameters": {"content": "the observation text to write"}}]}
EOF
        ;;
    execute_tool)
        # extract the `content` arg from the stdin payload and write it
        # to stderr (forwarded by airun to its debug stream). minimal
        # extractor — assumes a string value, no embedded quotes.
        content=$(printf '%s' "$payload" | sed -n 's/.*"content"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
        printf 'observation: %s\n' "$content" >&2
        # convention: tool results are json-encoded objects (matches the
        # built-in `read` and `bash` tools). airun also accepts plain
        # strings here, but structured output is easier for the model.
        printf '{"result": "{\\"status\\":\\"ok\\",\\"observation\\":\\"%s\\"}"}\n' "$content"
        ;;
esac
