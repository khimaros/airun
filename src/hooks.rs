// hook protocol v1 (partial conformance) — see DESIGN.md and
// docs/HOOK_PROTOCOL.md (in opencode-evolve). only the stages that fit
// a one-shot, sessionless cli are implemented:
//
//   discover, mutate_request, execute_tool, tool_before, tool_after
//
// stages tied to long-running sessions (idle, heartbeat, compacting,
// recover, format_notification, observe_message, actions) are skipped.
// the prompts/ contract is also deferred — hooks compose their own
// system strings via mutate_request.

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::{PermissionsConfig, check_tool_permission};

/// scripts whose basenames begin with these prefixes are skipped.
const HOOK_IGNORE_PREFIXES: &[&str] = &[".", "__"];

/// frontmatter-like fields recognized in a `discover` response.
#[derive(Debug, Deserialize, Default)]
struct DiscoverResponse {
    name: Option<String>,
    test: Option<String>,
    #[serde(default)]
    tools: Vec<DiscoveredTool>,
}

#[derive(Debug, Deserialize)]
struct DiscoveredTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: HashMap<String, Value>,
    #[serde(default)]
    permission: Option<DiscoveredPermission>,
}

#[derive(Debug, Deserialize)]
struct DiscoveredPermission {
    /// name (or list of names) of the parameter(s) used as the
    /// permission key when checking against the host's pattern map.
    arg: Value,
}

/// a single hook script discovered on disk.
#[derive(Debug, Clone)]
pub struct HookScript {
    /// registration name (defaults to filename stem); also tool prefix.
    pub name: String,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub test: Option<String>,
    pub tools: Vec<HookToolDef>,
}

#[derive(Debug, Clone)]
pub struct HookToolDef {
    /// fully namespaced tool name (`<prefix>_<short>`).
    pub full_name: String,
    pub short_name: String,
    pub description: String,
    pub parameters_schema: Value,
    pub permission_args: Vec<String>,
}

/// orchestrates the lifecycle of all discovered hook scripts.
#[derive(Debug, Default, Clone)]
pub struct HookManager {
    scripts: Vec<HookScript>,
}

impl HookManager {
    /// discovers hooks under `hooks/` in every project and global base
    /// directory used by airun. each script is invoked once with the
    /// `discover` stage to enumerate its tools.
    pub fn discover() -> Self {
        let mut scripts: Vec<HookScript> = Vec::new();
        let mut seen = std::collections::HashSet::<PathBuf>::new();

        for dir in hook_dirs() {
            collect_scripts(&dir, &mut seen, &mut scripts);
        }

        scripts.sort_by(|a, b| {
            a.path.file_name().cmp(&b.path.file_name())
        });

        for script in scripts.iter_mut() {
            if let Err(e) = run_discover(script) {
                eprintln!("hook discover failed for {}: {}", script.path.display(), e);
            }
        }

        HookManager { scripts }
    }

    pub fn scripts(&self) -> &[HookScript] {
        &self.scripts
    }

    /// runs `mutate_request` on every script and returns concatenated
    /// system prompt fragments in alphabetical script order.
    pub fn mutate_request(&self) -> Vec<String> {
        let input = json!({"hook": "mutate_request", "session": {"id": "airun"}, "history": []});
        let mut systems = Vec::new();
        for script in &self.scripts {
            match invoke(script, "mutate_request", &input) {
                Ok(merged) => {
                    if let Some(arr) = merged.get("system").and_then(|v| v.as_array()) {
                        for item in arr {
                            if let Some(s) = item.as_str() {
                                systems.push(s.to_string());
                            }
                        }
                    }
                }
                Err(e) => eprintln!("hook mutate_request failed for {}: {}", script.name, e),
            }
        }
        systems
    }

    /// observational. fired before any tool call (built-in or hook-registered).
    pub fn tool_before(&self, tool: &str, call_id: &str, args: &Value) {
        let input = json!({
            "hook": "tool_before",
            "session": {"id": "airun"},
            "tool": tool,
            "callID": call_id,
            "args": args,
        });
        for script in &self.scripts {
            // observational: failures are logged but ignored.
            let _ = invoke(script, "tool_before", &input);
        }
    }

    /// observational. fired after any tool call completes.
    pub fn tool_after(&self, tool: &str, call_id: &str, output: &str) {
        let input = json!({
            "hook": "tool_after",
            "session": {"id": "airun"},
            "tool": tool,
            "callID": call_id,
            "title": tool,
            "output": output,
        });
        for script in &self.scripts {
            let _ = invoke(script, "tool_after", &input);
        }
    }

    /// returns dynamic rig tools, one per hook-registered tool.
    pub fn into_dyn_tools(
        self: &Arc<Self>,
        permissions: &PermissionsConfig,
    ) -> Vec<Box<dyn ToolDyn>> {
        let mut out: Vec<Box<dyn ToolDyn>> = Vec::new();
        for (script_idx, script) in self.scripts.iter().enumerate() {
            for tool in &script.tools {
                out.push(Box::new(HookTool {
                    manager: self.clone(),
                    script_idx,
                    tool: tool.clone(),
                    permissions: permissions.clone(),
                }));
            }
        }
        out
    }
}

/// `Tool`-style wrapper that dispatches `execute_tool` over JSONL.
struct HookTool {
    manager: Arc<HookManager>,
    script_idx: usize,
    tool: HookToolDef,
    permissions: PermissionsConfig,
}

impl ToolDyn for HookTool {
    fn name(&self) -> String {
        self.tool.full_name.clone()
    }

    fn definition<'a>(
        &'a self,
        _prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolDefinition> + Send + 'a>> {
        let def = ToolDefinition {
            name: self.tool.full_name.clone(),
            description: self.tool.description.clone(),
            parameters: self.tool.parameters_schema.clone(),
        };
        Box::pin(async move { def })
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let parsed: Value = serde_json::from_str(&args).map_err(ToolError::JsonError)?;
            validate_args(&parsed, &self.tool.parameters_schema)
                .map_err(|msg| ToolError::ToolCallError(msg.into()))?;
            let perm_key = self.permission_key(&parsed);
            check_tool_permission(&self.permissions, &self.tool.full_name, &perm_key, false)?;

            let script = &self.manager.scripts[self.script_idx];
            let call_id = format!("{}-{}", self.tool.full_name, std::process::id());
            self.manager.tool_before(&self.tool.full_name, &call_id, &parsed);

            let input = json!({
                "hook": "execute_tool",
                "tool": self.tool.short_name,
                "args": parsed,
            });
            let merged = invoke(script, "execute_tool", &input)
                .map_err(|e| ToolError::ToolCallError(e.into()))?;
            let result_text = merged.get("result")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();

            self.manager.tool_after(&self.tool.full_name, &call_id, &result_text);
            Ok(result_text)
        })
    }
}

impl HookTool {
    /// builds the permission key from `permission.arg`. when multiple arg
    /// names are configured, their values are joined with spaces.
    fn permission_key(&self, args: &Value) -> String {
        if self.tool.permission_args.is_empty() {
            return "*".to_string();
        }
        let parts: Vec<String> = self.tool.permission_args.iter().map(|name| {
            args.get(name)
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| Some(v.to_string())))
                .unwrap_or_default()
        }).collect();
        parts.join(" ")
    }
}

/// returns the search roots for `hooks/` in priority order: project bases
/// walking up to git root, then global bases.
fn hook_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let bases = [".opencode", ".claude", ".agents"];

    let mut current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        for base in &bases {
            out.push(current.join(base).join("hooks"));
        }
        if current.join(".git").exists() {
            break;
        }
        if !current.pop() {
            break;
        }
    }

    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        for base in [
            home_path.join(".config").join("opencode"),
            home_path.join(".claude"),
            home_path.join(".agents"),
        ] {
            out.push(base.join("hooks"));
        }
    }

    out
}

/// collects executable hook scripts from a single `hooks/` directory.
fn collect_scripts(dir: &Path, seen: &mut std::collections::HashSet<PathBuf>, out: &mut Vec<HookScript>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let stem = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if HOOK_IGNORE_PREFIXES.iter().any(|p| stem.starts_with(p)) {
            continue;
        }
        if !is_executable(&path) {
            continue;
        }
        let canon = path.canonicalize().unwrap_or(path.clone());
        if !seen.insert(canon) {
            continue;
        }
        let default_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("hook").to_string();
        out.push(HookScript {
            name: default_name,
            path,
            test: None,
            tools: Vec::new(),
        });
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// runs `discover` on a script, populating its name/test/tools fields in place.
fn run_discover(script: &mut HookScript) -> Result<(), String> {
    let input = json!({"hook": "discover"});
    let merged = invoke(script, "discover", &input)?;
    let parsed: DiscoverResponse = serde_json::from_value(merged)
        .map_err(|e| format!("invalid discover response: {}", e))?;

    if let Some(name) = parsed.name {
        script.name = name;
    }
    script.test = parsed.test;
    script.tools = parsed.tools.into_iter().map(|t| {
        let parameters_schema = build_parameters_schema(&t.parameters);
        let permission_args = t.permission.as_ref()
            .map(|p| match &p.arg {
                Value::String(s) => vec![s.clone()],
                Value::Array(arr) => arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        HookToolDef {
            full_name: format!("{}_{}", script.name, t.name),
            short_name: t.name,
            description: t.description,
            parameters_schema,
            permission_args,
        }
    }).collect();
    Ok(())
}

/// JSON-schema types accepted in `discover` per § discover.
const PROTOCOL_TYPES: &[&str] = &["string", "number", "boolean", "object", "array", "any"];

/// builds a JSON-schema `parameters` object from the hook's three
/// accepted parameter forms (string shorthand, typed, enum). the
/// protocol's `any` type is normalized away (omitted) since it's not
/// standard JSON Schema and providers may reject it.
fn build_parameters_schema(params: &HashMap<String, Value>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, spec) in params {
        let (prop, optional) = match spec {
            Value::String(desc) => (json!({"type": "string", "description": desc}), false),
            Value::Object(map) => {
                let mut prop = serde_json::Map::new();
                let mut optional = false;
                for (k, v) in map {
                    if k == "optional" {
                        optional = v.as_bool().unwrap_or(false);
                    } else {
                        prop.insert(k.clone(), v.clone());
                    }
                }
                // omit `type: "any"` — it isn't standard JSON Schema.
                if matches!(prop.get("type"), Some(Value::String(s)) if s == "any") {
                    prop.remove("type");
                } else if !prop.contains_key("type") {
                    prop.insert("type".to_string(), json!("string"));
                }
                (Value::Object(prop), optional)
            }
            other => (other.clone(), false),
        };
        properties.insert(name.clone(), prop);
        if !optional {
            required.push(name.clone());
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// validates the LLM-supplied `args` against a parameters schema produced
/// by `build_parameters_schema`. checks: required fields present, type
/// match (if a `type` is declared), and enum membership. returns a human
/// readable error string on the first violation.
fn validate_args(args: &Value, schema: &Value) -> Result<(), String> {
    let obj = args.as_object().ok_or_else(|| "args must be a JSON object".to_string())?;

    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for name in required {
            if let Some(name) = name.as_str() {
                if !obj.contains_key(name) {
                    return Err(format!("missing required parameter '{}'", name));
                }
            }
        }
    }

    let properties = match schema.get("properties").and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return Ok(()),
    };

    for (name, value) in obj {
        let prop = match properties.get(name) {
            Some(p) => p,
            None => continue,
        };
        if let Some(ty) = prop.get("type").and_then(|v| v.as_str()) {
            if !type_matches(ty, value) {
                return Err(format!("parameter '{}' expected {}, got {}", name, ty, value_type_name(value)));
            }
        }
        if let Some(allowed) = prop.get("enum").and_then(|v| v.as_array()) {
            if !allowed.iter().any(|v| v == value) {
                let opts: Vec<String> = allowed.iter().map(|v| v.to_string()).collect();
                return Err(format!("parameter '{}' must be one of [{}], got {}", name, opts.join(", "), value));
            }
        }
    }

    Ok(())
}

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        // "any" is normalized away in build_parameters_schema, but be
        // defensive — also treat unknown types as permissive rather than
        // rejecting valid hook authors.
        "any" => true,
        other => !PROTOCOL_TYPES.contains(&other),
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// invokes a hook script with the given stage, writes the input json on
/// stdin (terminated by EOF), and merges every JSONL line of stdout into
/// a single Value per § composability.
fn invoke(script: &HookScript, stage: &str, input: &Value) -> Result<Value, String> {
    let mut child = Command::new(&script.path)
        .arg(stage)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn {}: {}", script.path.display(), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(input).map_err(|e| format!("encode input: {}", e))?;
        stdin.write_all(&payload).map_err(|e| format!("write stdin: {}", e))?;
    }

    let output = child.wait_with_output().map_err(|e| format!("wait: {}", e))?;
    if !output.status.success() {
        return Err(format!("exit {}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut merged = serde_json::Map::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("hook {} stage={} dropped malformed line: {}", script.name, stage, e);
                continue;
            }
        };
        let obj = match value {
            Value::Object(m) => m,
            _ => continue,
        };
        for (k, v) in obj {
            if k == "log" {
                if let Some(s) = v.as_str() {
                    eprintln!("hook {}: {}", script.name, s);
                }
                continue;
            }
            merge_field(&mut merged, k, v);
        }
    }
    Ok(Value::Object(merged))
}

/// merges a single (k, v) into the accumulator per § composability:
/// arrays concatenate, scalars join with newline.
const ARRAY_FIELDS: &[&str] = &["system", "tools", "notifications", "actions", "modified", "notify"];
const SCALAR_FIELDS: &[&str] = &["continue", "prompt", "user", "message", "result"];

fn merge_field(acc: &mut serde_json::Map<String, Value>, key: String, value: Value) {
    if ARRAY_FIELDS.contains(&key.as_str()) {
        let entry = acc.entry(key).or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(existing) = entry {
            match value {
                Value::Array(new) => existing.extend(new),
                other => existing.push(other),
            }
        }
        return;
    }
    if SCALAR_FIELDS.contains(&key.as_str()) {
        let new_str = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        match acc.get_mut(&key) {
            Some(Value::String(existing)) => {
                existing.push('\n');
                existing.push_str(&new_str);
            }
            _ => {
                acc.insert(key, Value::String(new_str));
            }
        }
        return;
    }
    acc.insert(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_arrays_concatenate() {
        let mut acc = serde_json::Map::new();
        merge_field(&mut acc, "system".into(), json!(["a"]));
        merge_field(&mut acc, "system".into(), json!(["b", "c"]));
        assert_eq!(acc.get("system"), Some(&json!(["a", "b", "c"])));
    }

    #[test]
    fn merge_scalars_join_with_newline() {
        let mut acc = serde_json::Map::new();
        merge_field(&mut acc, "result".into(), json!("first"));
        merge_field(&mut acc, "result".into(), json!("second"));
        assert_eq!(acc.get("result"), Some(&json!("first\nsecond")));
    }

    #[test]
    fn parameters_schema_string_shorthand() {
        let mut params = HashMap::new();
        params.insert("trait".to_string(), json!("the trait name"));
        let schema = build_parameters_schema(&params);
        assert_eq!(schema["properties"]["trait"]["type"], json!("string"));
        assert_eq!(schema["properties"]["trait"]["description"], json!("the trait name"));
        assert_eq!(schema["required"], json!(["trait"]));
    }

    #[test]
    fn parameters_schema_any_type_is_stripped() {
        let mut params = HashMap::new();
        params.insert("blob".to_string(), json!({"type": "any", "description": "anything"}));
        let schema = build_parameters_schema(&params);
        assert!(schema["properties"]["blob"].get("type").is_none());
        assert_eq!(schema["properties"]["blob"]["description"], json!("anything"));
    }

    #[test]
    fn validate_args_rejects_missing_required() {
        let schema = build_parameters_schema(&HashMap::from([
            ("path".to_string(), json!("the path")),
        ]));
        let err = validate_args(&json!({}), &schema).unwrap_err();
        assert!(err.contains("missing required parameter 'path'"));
    }

    #[test]
    fn validate_args_rejects_type_mismatch() {
        let schema = build_parameters_schema(&HashMap::from([
            ("count".to_string(), json!({"type": "number"})),
        ]));
        let err = validate_args(&json!({"count": "not-a-number"}), &schema).unwrap_err();
        assert!(err.contains("expected number"));
    }

    #[test]
    fn validate_args_rejects_invalid_enum() {
        let schema = build_parameters_schema(&HashMap::from([
            ("mode".to_string(), json!({"type": "string", "enum": ["fast", "slow"]})),
        ]));
        let err = validate_args(&json!({"mode": "medium"}), &schema).unwrap_err();
        assert!(err.contains("must be one of"));
        assert!(validate_args(&json!({"mode": "fast"}), &schema).is_ok());
    }

    #[test]
    fn validate_args_accepts_optional_missing() {
        let schema = build_parameters_schema(&HashMap::from([
            ("tz".to_string(), json!({"type": "string", "optional": true})),
        ]));
        assert!(validate_args(&json!({}), &schema).is_ok());
    }

    #[test]
    fn parameters_schema_optional_excluded_from_required() {
        let mut params = HashMap::new();
        params.insert("tz".to_string(), json!({"type": "string", "optional": true}));
        let schema = build_parameters_schema(&params);
        assert_eq!(schema["required"], json!([]));
        // "optional" key should not leak into the JSON-schema property
        assert!(schema["properties"]["tz"].get("optional").is_none());
    }
}
