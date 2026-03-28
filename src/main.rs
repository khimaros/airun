use clap::{CommandFactory, Parser};
use rig::client::CompletionClient;
use rig::completion::ToolDefinition;
use rig::providers;
use rig::streaming::StreamingPrompt;
use rig::tool::{Tool, ToolError};
use serde::{Deserialize, Deserializer, de};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write, IsTerminal};
use std::path::{Path, PathBuf};
use std::process;
use futures_util::stream::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::EnvFilter;

/// when true, "ask" permissions are auto-accepted without prompting.
static AUTO_ACCEPT: AtomicBool = AtomicBool::new(false);

// --- permissions model (modeled after opencode.ai/docs/permissions) ---

#[derive(Debug, Clone, PartialEq)]
enum PermissionLevel {
    Allow,
    Ask,
    Deny,
}

impl<'de> Deserialize<'de> for PermissionLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "allow" => Ok(PermissionLevel::Allow),
            "ask" => Ok(PermissionLevel::Ask),
            "deny" => Ok(PermissionLevel::Deny),
            _ => Err(de::Error::custom(
                format!("invalid permission level '{}', expected 'allow', 'ask', or 'deny'", s),
            )),
        }
    }
}

/// a permission can be a single level or a map of glob patterns to levels.
/// patterns support gitignore-style globs: `*` (non-slash), `**` (any), `?`.
/// when multiple patterns match, the most specific (fewest wildcards) wins.
#[derive(Debug, Clone)]
enum Permission {
    Level(PermissionLevel),
    Patterns(HashMap<String, PermissionLevel>),
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        if let serde_yaml::Value::String(s) = &value {
            match s.as_str() {
                "allow" => return Ok(Permission::Level(PermissionLevel::Allow)),
                "ask" => return Ok(Permission::Level(PermissionLevel::Ask)),
                "deny" => return Ok(Permission::Level(PermissionLevel::Deny)),
                _ => return Err(de::Error::custom(
                    format!("invalid permission level '{}', expected 'allow', 'ask', or 'deny'", s),
                )),
            }
        }
        if let serde_yaml::Value::Mapping(map) = &value {
            let mut patterns = HashMap::new();
            for (k, v) in map {
                let key = k.as_str().ok_or_else(|| {
                    de::Error::custom(format!("expected string key, got {:?}", k))
                })?;
                let level_str = v.as_str().ok_or_else(|| {
                    de::Error::custom(format!("pattern '{}': expected 'allow', 'ask', or 'deny', got {:?}", key, v))
                })?;
                let level = match level_str {
                    "allow" => PermissionLevel::Allow,
                    "ask" => PermissionLevel::Ask,
                    "deny" => PermissionLevel::Deny,
                    _ => return Err(de::Error::custom(
                        format!("pattern '{}': invalid permission level '{}', expected 'allow', 'ask', or 'deny'", key, level_str),
                    )),
                };
                patterns.insert(key.to_string(), level);
            }
            return Ok(Permission::Patterns(patterns));
        }
        Err(de::Error::custom(
            format!("expected a permission level string or a map of patterns, got {:?}", value),
        ))
    }
}

impl Permission {
    /// checks the permission level for the given input.
    /// `path_mode`: when true, `*` stops at `/` boundaries (for file paths).
    /// when false, `*` matches any character (for commands).
    fn check(&self, input: &str, path_mode: bool) -> PermissionLevel {
        match self {
            Permission::Level(level) => level.clone(),
            Permission::Patterns(patterns) => {
                let mut result = PermissionLevel::Deny;
                let mut best_specificity = 0usize;
                for (pattern, level) in patterns {
                    // try the pattern as-is, and also with trailing ` *`/` **`
                    // stripped so "ls *" also matches "ls" (no args)
                    let candidates = [
                        glob_matches(pattern, input, path_mode),
                        pattern.strip_suffix(" **")
                            .or_else(|| pattern.strip_suffix(" *"))
                            .and_then(|prefix| glob_matches(prefix, input, path_mode)),
                    ];
                    for specificity in candidates.into_iter().flatten() {
                        if specificity >= best_specificity {
                            best_specificity = specificity;
                            result = level.clone();
                        }
                    }
                }
                result
            }
        }
    }
}

/// glob matching with optional path-aware semantics.
/// `**` always matches any sequence of characters.
/// in path mode: `*` matches non-`/` chars, `?` matches one non-`/` char.
/// in command mode: `*` and `?` match any character (including `/`).
fn glob_matches(pattern: &str, input: &str, path_mode: bool) -> Option<usize> {
    if glob_matches_recursive(pattern.as_bytes(), input.as_bytes(), path_mode) {
        let specificity = pattern.len() - pattern.matches('*').count() - pattern.matches('?').count();
        Some(specificity)
    } else {
        None
    }
}

fn glob_matches_recursive(pattern: &[u8], input: &[u8], path_mode: bool) -> bool {
    match (pattern, input) {
        ([], []) => true,
        // `**` matches zero or more of anything
        ([b'*', b'*', rest @ ..], _) => {
            let rest = skip_stars(rest);
            for i in 0..=input.len() {
                if glob_matches_recursive(rest, &input[i..], path_mode) {
                    return true;
                }
            }
            false
        }
        // `*` — in path mode stops at `/`, otherwise matches anything
        ([b'*', rest @ ..], _) => {
            let rest = skip_stars(rest);
            for i in 0..=input.len() {
                if path_mode && i > 0 && input[i - 1] == b'/' {
                    break;
                }
                if glob_matches_recursive(rest, &input[i..], path_mode) {
                    return true;
                }
            }
            false
        }
        // `?` — in path mode skips non-`/`, otherwise any char
        ([b'?', rest @ ..], [c, input_rest @ ..]) if !path_mode || *c != b'/' => {
            glob_matches_recursive(rest, input_rest, path_mode)
        }
        ([p, rest @ ..], [c, input_rest @ ..]) if p == c => {
            glob_matches_recursive(rest, input_rest, path_mode)
        }
        _ => false,
    }
}

/// skips consecutive `*` characters in a pattern.
fn skip_stars(pattern: &[u8]) -> &[u8] {
    let mut p = pattern;
    while let [b'*', rest @ ..] = p {
        p = rest;
    }
    p
}

#[derive(Deserialize, Debug, Default, Clone)]
struct PermissionsConfig {
    #[serde(flatten)]
    tools: HashMap<String, Permission>,
}

impl PermissionsConfig {
    fn merge(self, other: PermissionsConfig) -> PermissionsConfig {
        let mut merged = self.tools;
        merged.extend(other.tools);
        PermissionsConfig { tools: merged }
    }

    fn check(&self, tool_name: &str, input: &str, path_mode: bool) -> PermissionLevel {
        self.tools.get(tool_name)
            .map(|p| p.check(input, path_mode))
            .unwrap_or(PermissionLevel::Deny)
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
struct ToolsConfig {
    #[serde(flatten)]
    tools: HashMap<String, bool>,
}

impl ToolsConfig {
    fn merge(self, other: ToolsConfig) -> ToolsConfig {
        let mut merged = self.tools;
        merged.extend(other.tools);
        ToolsConfig { tools: merged }
    }

    /// checks if a tool is active. if `tools_override` is Some, only
    /// the listed tools are enabled (ignoring config). otherwise, uses config.
    fn is_active(&self, tool_name: &str, tools_override: &Option<Vec<String>>) -> bool {
        match tools_override {
            Some(list) => list.iter().any(|t| t == tool_name),
            None => *self.tools.get(tool_name).unwrap_or(&false),
        }
    }
}

// --- read tool ---

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

/// prompts the user for confirmation via /dev/tty (bypassing stdin which
/// may be piped). returns true immediately if --yes flag is set.
fn prompt_user_confirmation(tool_name: &str, input: &str) -> bool {
    if AUTO_ACCEPT.load(Ordering::Relaxed) {
        eprintln!("auto-accepting: {} \"{}\"", tool_name, input);
        return true;
    }
    let tty = match fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(f) => f,
        Err(_) => {
            eprintln!("cannot open /dev/tty for confirmation, denying: {} {}", tool_name, input);
            return false;
        }
    };
    let mut reader = io::BufReader::new(&tty);
    let mut writer = &tty;
    let _ = write!(writer, "allow {} \"{}\"? [y/N] ", tool_name, input);
    let _ = writer.flush();
    let mut response = String::new();
    if reader.read_line(&mut response).is_err() {
        return false;
    }
    matches!(response.trim(), "y" | "Y" | "yes" | "YES")
}

/// checks permission for a tool invocation, returning Ok(()) or a ToolError.
/// `path_mode`: true for file-path tools (read), false for command tools (bash).
fn check_tool_permission(
    permissions: &PermissionsConfig,
    tool_name: &str,
    input: &str,
    path_mode: bool,
) -> Result<(), ToolError> {
    match permissions.check(tool_name, input, path_mode) {
        PermissionLevel::Allow => Ok(()),
        PermissionLevel::Ask => {
            if prompt_user_confirmation(tool_name, input) {
                Ok(())
            } else {
                Err(ToolError::ToolCallError(
                    format!("permission denied (user rejected): {}", input).into(),
                ))
            }
        }
        PermissionLevel::Deny => {
            Err(ToolError::ToolCallError(
                format!("permission denied: {}", input).into(),
            ))
        }
    }
}

struct ReadTool {
    permissions: PermissionsConfig,
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";
    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_string(),
            description: "read the contents of a file at the given path".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "absolute or relative path to the file to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let resolved = resolve_path(&args.path);
        let path_str = resolved.to_string_lossy();
        check_tool_permission(&self.permissions, "read", &path_str, true)?;
        fs::read_to_string(&resolved).map_err(|e| {
            ToolError::ToolCallError(format!("{}: {}", path_str, e).into())
        })
    }
}

// --- bash tool ---

/// shell metacharacters that could chain or redirect commands.
const SHELL_METACHARACTERS: &[char] = &[';', '|', '&', '`', '$', '(', ')', '{', '}', '<', '>', '\n', '\r', '!', '#'];

/// returns true if the command is a simple command without shell
/// metacharacters that could bypass permission checks.
fn is_simple_bash_command(command: &str) -> bool {
    !command.contains('\\') && !command.chars().any(|c| SHELL_METACHARACTERS.contains(&c))
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

struct BashTool {
    permissions: PermissionsConfig,
}

impl Tool for BashTool {
    const NAME: &'static str = "bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "execute a bash command and return its output. commands must be simple (no pipes, redirects, chaining, or shell metacharacters).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "the bash command to execute (no pipes, redirects, semicolons, or shell metacharacters)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if is_simple_bash_command(&args.command) {
            check_tool_permission(&self.permissions, "bash", &args.command, false)?;
        } else {
            // command contains shell metacharacters — can't trust pattern
            // matching on the full string, so fall back to the catch-all rule.
            // use an empty string to only match wildcard patterns.
            let level = self.permissions.check("bash", "", false);
            match level {
                PermissionLevel::Allow => {}
                PermissionLevel::Ask => {
                    if !prompt_user_confirmation("bash (complex)", &args.command) {
                        return Err(ToolError::ToolCallError(
                            format!("permission denied (user rejected): {}", args.command).into(),
                        ));
                    }
                }
                PermissionLevel::Deny => {
                    return Err(ToolError::ToolCallError(
                        format!("permission denied (shell metacharacters): {}", args.command).into(),
                    ));
                }
            }
        }
        let output = process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .output()
            .map_err(|e| {
                ToolError::ToolCallError(format!("failed to execute: {}", e).into())
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            Ok(stdout.into_owned())
        } else {
            let code = output.status.code().unwrap_or(-1);
            Ok(format!("exit code {}\nstdout:\n{}\nstderr:\n{}", code, stdout, stderr))
        }
    }
}

fn resolve_path(path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    }
}

const DEFAULT_MAX_TOKENS: u64 = 16384;
const DEFAULT_MAX_TURNS: usize = 32;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// the name of the agent to run (optional)
    agent_name: Option<String>,

    /// initialize a default configuration file
    #[arg(long)]
    init: bool,

    /// list available agents
    #[arg(long)]
    list_agents: bool,

    /// list available skills
    #[arg(long)]
    list_skills: bool,

    /// list available tools
    #[arg(long)]
    list_tools: bool,

    /// list configured providers
    #[arg(long)]
    list_providers: bool,

    /// prompt to send to the agent (if not provided via stdin)
    #[arg(short, long)]
    prompt: Option<String>,

    /// override the system prompt
    #[arg(short = 's', long)]
    system_prompt: Option<String>,

    /// override the model to use (format: <provider_name>/<model_name>)
    #[arg(short, long)]
    model: Option<String>,

    /// maximum output tokens (default: 16384)
    #[arg(short = 't', long)]
    max_tokens: Option<u64>,

    /// enable specific tools (comma-separated, e.g. --tools read,bash)
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// attach additional skills (comma-separated)
    #[arg(long, value_delimiter = ',')]
    skills: Option<Vec<String>>,

    /// print what would be sent to the LLM and exit
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// auto-accept "ask" permission prompts
    #[arg(short = 'y', long)]
    yes: bool,

    /// enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Deserialize, Debug, Default, Clone)]
struct ProviderConfig {
    name: String,
    /// openai (responses API), openai_completions, anthropic, gemini, cohere, xai
    client: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct Config {
    default_model: Option<String>,
    default_max_tokens: Option<u64>,
    default_max_turns: Option<usize>,
    #[serde(default)]
    tools: ToolsConfig,
    #[serde(default, alias = "permission")]
    permissions: PermissionsConfig,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
}

#[derive(Deserialize, Debug, Default)]
struct AgentFrontmatter {
    description: Option<String>,
    model: Option<String>,
    skills: Option<Vec<String>>,
    #[serde(default)]
    tools: ToolsConfig,
    #[serde(default, alias = "permission")]
    permissions: PermissionsConfig,
}

struct ParsedDoc<T> {
    frontmatter: T,
    body: String,
}

#[derive(Deserialize, Debug, Default)]
struct SkillFrontmatter {
    description: Option<String>,
}

fn parse_markdown_with_frontmatter<T: serde::de::DeserializeOwned + Default>(
    content: &str,
) -> Result<ParsedDoc<T>, Box<dyn std::error::Error>> {
    if content.starts_with("---\n") || content.starts_with("---\r\n") {
        if let Some(end_idx) = content[4..].find("\n---") {
            let frontmatter_str = &content[4..end_idx + 4];
            let body_start = end_idx + 4 + 4;
            let body_start = if content.len() > body_start && content[body_start..].starts_with('\n') {
                body_start + 1
            } else if content.len() > body_start + 1 && content[body_start..].starts_with("\r\n") {
                body_start + 2
            } else {
                body_start
            };

            let frontmatter: T = serde_yaml::from_str(frontmatter_str)?;
            let body = content[body_start..].to_string();
            return Ok(ParsedDoc { frontmatter, body });
        }
    }
    Ok(ParsedDoc {
        frontmatter: T::default(),
        body: content.to_string(),
    })
}

fn find_file_in_dirs(
    filename: &str,
    subdirs: &[&str],
) -> Option<PathBuf> {
    let base_dirs = [".opencode", ".claude", ".agents"];
    
    // walk up from current dir
    let mut current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    
    loop {
        for base in &base_dirs {
            let base_path = current_dir.join(base);
            for subdir in subdirs {
                let path = base_path.join(subdir).join(filename);
                if path.exists() {
                    return Some(path);
                }
            }
            let path = base_path.join(filename);
            if path.exists() {
                return Some(path);
            }
        }
        if current_dir.join(".git").exists() {
            break;
        }
        if !current_dir.pop() {
            break;
        }
    }
    
    // check global configurations
    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        let global_bases = [
            home_path.join(".config").join("opencode"),
            home_path.join(".claude"),
            home_path.join(".agents"),
        ];
        
        for base_path in global_bases {
            for subdir in subdirs {
                let path = base_path.join(subdir).join(filename);
                if path.exists() {
                    return Some(path);
                }
            }
            let path = base_path.join(filename);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

/// finds all .md files in the given subdirs across all base directories.
/// returns (name, path) pairs with the `.md` extension stripped.
fn find_all_in_dirs(subdirs: &[&str]) -> Vec<(String, PathBuf)> {
    let base_dirs = [".opencode", ".claude", ".agents"];
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut search = |dir: &Path| {
        for base in &base_dirs {
            let base_path = dir.join(base);
            for subdir in subdirs {
                let search_dir = base_path.join(subdir);
                if let Ok(entries) = fs::read_dir(&search_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() && path.join("SKILL.md").exists() {
                            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                                let name = name.to_string();
                                if seen.insert(name.clone()) {
                                    results.push((name, path.join("SKILL.md")));
                                }
                            }
                        } else if path.extension().map_or(false, |e| e == "md") {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                let name = stem.to_string();
                                if seen.insert(name.clone()) {
                                    results.push((name, path));
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // walk up from current dir
    let mut current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        search(&current_dir);
        if current_dir.join(".git").exists() {
            break;
        }
        if !current_dir.pop() {
            break;
        }
    }

    // global
    if let Ok(home) = env::var("HOME") {
        let home_path = PathBuf::from(home);
        for dir in [
            home_path.join(".config").join("opencode"),
            home_path.join(".claude"),
            home_path.join(".agents"),
        ] {
            search(&dir);
        }
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

fn load_agent(agent_name: &str) -> Result<ParsedDoc<AgentFrontmatter>, Box<dyn std::error::Error>> {
    let filename = format!("{}.md", agent_name);
    let path = find_file_in_dirs(&filename, &["agents"]).ok_or_else(|| {
        format!("Agent '{}' not found in .opencode/, .claude/, or .agents/ directories", agent_name)
    })?;
    
    let content = fs::read_to_string(&path)?;
    parse_markdown_with_frontmatter(&content)
        .map_err(|e| format!("agent '{}' ({}): {}", agent_name, path.display(), e).into())
}

fn load_skill(skill_name: &str) -> Result<ParsedDoc<SkillFrontmatter>, Box<dyn std::error::Error>> {
    let filename_md = format!("{}.md", skill_name);
    let filename_skill_md = format!("{}/SKILL.md", skill_name);
    
    let path = find_file_in_dirs(&filename_skill_md, &["skills"])
        .or_else(|| find_file_in_dirs(&filename_md, &["skills"]))
        .ok_or_else(|| {
            format!("Skill '{}' not found in .opencode/, .claude/, or .agents/ directories", skill_name)
        })?;
    
    let content = fs::read_to_string(&path)?;
    parse_markdown_with_frontmatter(&content)
        .map_err(|e| format!("skill '{}' ({}): {}", skill_name, path.display(), e).into())
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_paths = [
        "airun.toml",
        ".airun.toml",
        ".config/airun.toml",
        "~/.config/airun/config.toml",
    ];

    let mut config_content = String::new();
    for path_str in config_paths {
        let path = if path_str.starts_with("~/") {
            if let Ok(home) = env::var("HOME") {
                PathBuf::from(home).join(&path_str[2..])
            } else {
                continue;
            }
        } else {
            PathBuf::from(path_str)
        };

        if path.exists() {
            config_content = fs::read_to_string(path)?;
            break;
        }
    }

    if config_content.is_empty() {
        return Ok(Config::default());
    }

    let config: Config = toml::from_str(&config_content)?;
    Ok(config)
}

fn init_config() -> Result<(), Box<dyn std::error::Error>> {
    let home = env::var("HOME").map_err(|_| "HOME environment variable not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("airun");
    
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
        println!("created directory: {}", config_dir.display());
    }
    
    let config_path = config_dir.join("config.toml");
    
    if config_path.exists() {
        eprintln!("configuration file already exists at: {}", config_path.display());
        return Ok(());
    }

    let default_config = include_str!("../airun.example.toml");

    fs::write(&config_path, default_config)?;
    println!("initialized configuration at: {}", config_path.display());
    Ok(())
}

macro_rules! stream_agent {
    ($agent:expr, $user_prompt:expr) => {{
        let mut stream = $agent.stream_prompt($user_prompt).await;
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                    rig::streaming::StreamedAssistantContent::Text(t)
                )) => {
                    stdout.write_all(t.text.as_bytes())?;
                    stdout.flush()?;
                }
                Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                    rig::streaming::StreamedAssistantContent::ReasoningDelta { reasoning, .. }
                )) => {
                    // dim + italic
                    stderr.write_all(b"\x1b[2;3m")?;
                    stderr.write_all(reasoning.as_bytes())?;
                    stderr.write_all(b"\x1b[0m")?;
                    stderr.flush()?;
                }
                Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                    rig::streaming::StreamedAssistantContent::ToolCall { tool_call, .. }
                )) => {
                    // bold cyan tool name + dim args
                    write!(stderr, "\x1b[1;36m{}\x1b[0m\x1b[2m({})\x1b[0m\n",
                        tool_call.function.name, tool_call.function.arguments)?;
                    stderr.flush()?;
                }
                Ok(rig::agent::MultiTurnStreamItem::StreamUserItem(
                    rig::streaming::StreamedUserContent::ToolResult { tool_result, .. }
                )) => {
                    // dim tool result, truncated
                    let result_text: String = tool_result.content.iter()
                        .filter_map(|c| match c {
                            rig::message::ToolResultContent::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let truncated = if result_text.len() > 200 {
                        format!("{}... ({} bytes)", &result_text[..200], result_text.len())
                    } else {
                        result_text
                    };
                    write!(stderr, "\x1b[2m  -> {}\x1b[0m\n", truncated.replace('\n', "\\n"))?;
                    stderr.flush()?;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("stream error: {}", e);
                    break;
                }
            }
        }
    }};
}

/// builds and streams an agent, conditionally adding tools based on config.
/// uses a macro because `.tool()` changes the builder's type parameter.
macro_rules! build_and_stream {
    (client: $client:expr, $model_name:expr, $max_tokens:expr, $max_turns:expr, $tools:expr, $tools_override:expr, $permissions:expr, $system_prompt:expr, $user_prompt:expr) => {{
        let mut builder = $client.agent($model_name).max_tokens($max_tokens).default_max_turns($max_turns);
        if !$system_prompt.is_empty() {
            builder = builder.preamble($system_prompt);
        }
        add_tools_and_stream!(builder, $tools, $tools_override, $permissions, $user_prompt);
    }};
    (model: $model:expr, $max_tokens:expr, $max_turns:expr, $tools:expr, $tools_override:expr, $permissions:expr, $system_prompt:expr, $user_prompt:expr) => {{
        let mut builder = rig::agent::AgentBuilder::new($model).max_tokens($max_tokens).default_max_turns($max_turns);
        if !$system_prompt.is_empty() {
            builder = builder.preamble($system_prompt);
        }
        add_tools_and_stream!(builder, $tools, $tools_override, $permissions, $user_prompt);
    }};
}

macro_rules! add_tools_and_stream {
    ($builder:expr, $tools:expr, $tools_override:expr, $permissions:expr, $user_prompt:expr) => {{
        let has_read = $tools.is_active("read", $tools_override);
        let has_bash = $tools.is_active("bash", $tools_override);
        match (has_read, has_bash) {
            (true, true) => {
                let agent = $builder
                    .tool(ReadTool { permissions: $permissions.clone() })
                    .tool(BashTool { permissions: $permissions.clone() })
                    .build();
                stream_agent!(agent, $user_prompt);
            }
            (true, false) => {
                let agent = $builder
                    .tool(ReadTool { permissions: $permissions.clone() })
                    .build();
                stream_agent!(agent, $user_prompt);
            }
            (false, true) => {
                let agent = $builder
                    .tool(BashTool { permissions: $permissions.clone() })
                    .build();
                stream_agent!(agent, $user_prompt);
            }
            (false, false) => {
                let agent = $builder.build();
                stream_agent!(agent, $user_prompt);
            }
        }
    }};
}

/// appends skill contents to a system prompt.
fn append_skills(system_prompt: &mut String, skill_names: &[String]) {
    if skill_names.is_empty() {
        return;
    }
    system_prompt.push_str("\n\n# skills\n");
    for skill_name in skill_names {
        match load_skill(skill_name) {
            Ok(skill) => {
                system_prompt.push_str(&format!("\n## {}\n", skill_name));
                if let Some(desc) = skill.frontmatter.description {
                    system_prompt.push_str(&format!("description: {}\n", desc));
                }
                system_prompt.push_str(&format!("{}\n", skill.body));
            }
            Err(e) => {
                eprintln!("warning: failed to load skill '{}': {}", skill_name, e);
            }
        }
    }
}

fn build_system_prompt(agent: &ParsedDoc<AgentFrontmatter>) -> String {
    let mut system_prompt = agent.body.clone();
    if let Some(skills) = &agent.frontmatter.skills {
        append_skills(&mut system_prompt, skills);
    }
    system_prompt
}

fn get_user_prompt(args: &Args) -> Result<String, Box<dyn std::error::Error>> {
    let mut user_prompt = args.prompt.clone().unwrap_or_default();
    if user_prompt.is_empty() {
        if !io::stdin().is_terminal() {
            io::stdin().read_to_string(&mut user_prompt)?;
        }
    }
    
    if user_prompt.trim().is_empty() {
        return Err("one of --prompt or stdin must be non-empty".into());
    }
    
    Ok(user_prompt)
}

async fn run_agent_stream(
    client_type: &str,
    model_name: &str,
    api_key: &str,
    base_url: Option<String>,
    max_tokens: u64,
    max_turns: usize,
    tools: &ToolsConfig,
    tools_override: &Option<Vec<String>>,
    permissions: &PermissionsConfig,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    macro_rules! build_openai_client {
        () => {{
            let mut builder = providers::openai::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            builder.build().expect("failed to build OpenAI client")
        }};
    }

    match client_type {
        "openai_completions" => {
            let client = build_openai_client!();
            let model = client.completion_model(model_name).completions_api();
            build_and_stream!(model: model, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        "openai" | "openai_responses" => {
            let client = build_openai_client!();
            build_and_stream!(client: client, model_name, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        "anthropic" => {
            let mut builder = providers::anthropic::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Anthropic client");
            build_and_stream!(client: client, model_name, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        "gemini" => {
            let mut builder = providers::gemini::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Gemini client");
            build_and_stream!(client: client, model_name, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        "cohere" => {
            let mut builder = providers::cohere::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Cohere client");
            build_and_stream!(client: client, model_name, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        "xai" => {
            let mut builder = providers::xai::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build xAI client");
            build_and_stream!(client: client, model_name, max_tokens, max_turns, tools, tools_override, permissions, system_prompt, user_prompt);
        },
        _ => return Err(format!("unsupported client type: {}", client_type).into()),
    }
    
    println!();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.yes {
        AUTO_ACCEPT.store(true, Ordering::Relaxed);
    }

    if args.init {
        if let Err(e) = init_config() {
            eprintln!("error initializing config: {}", e);
            process::exit(1);
        }
        process::exit(0);
    }

    if args.list_agents {
        for (name, path) in find_all_in_dirs(&["agents"]) {
            let desc = fs::read_to_string(&path).ok()
                .and_then(|c| parse_markdown_with_frontmatter::<AgentFrontmatter>(&c).ok())
                .and_then(|doc| doc.frontmatter.description)
                .unwrap_or_default();
            println!("{}\t{}", name, desc);
        }
        process::exit(0);
    }

    if args.list_skills {
        for (name, path) in find_all_in_dirs(&["skills"]) {
            let desc = fs::read_to_string(&path).ok()
                .and_then(|c| parse_markdown_with_frontmatter::<SkillFrontmatter>(&c).ok())
                .and_then(|doc| doc.frontmatter.description)
                .unwrap_or_default();
            println!("{}\t{}", name, desc);
        }
        process::exit(0);
    }

    if args.list_tools {
        println!("read\tread the contents of a file");
        println!("bash\texecute a bash command");
        process::exit(0);
    }

    let config = load_config()?;

    if args.list_providers {
        for provider in &config.providers {
            let client = provider.client.as_deref().unwrap_or(&provider.name);
            let url = provider.base_url.as_deref().unwrap_or("-");
            println!("{}\t{}\t{}", provider.name, client, url);
        }
        process::exit(0);
    }
    
    let (system_prompt, agent_model, agent_tools, agent_permissions) = if let Some(agent_name) = &args.agent_name {
        let agent = load_agent(agent_name)?;
        if let Some(ref override_prompt) = args.system_prompt {
            // -s overrides the entire system prompt (no agent body or skills)
            (override_prompt.clone(), agent.frontmatter.model, agent.frontmatter.tools, agent.frontmatter.permissions)
        } else if args.skills.is_some() {
            // --skills overrides agent skills exclusively
            let mut prompt = agent.body.clone();
            append_skills(&mut prompt, args.skills.as_ref().unwrap());
            (prompt, agent.frontmatter.model, agent.frontmatter.tools, agent.frontmatter.permissions)
        } else {
            let prompt = build_system_prompt(&agent);
            (prompt, agent.frontmatter.model, agent.frontmatter.tools, agent.frontmatter.permissions)
        }
    } else {
        let mut prompt = args.system_prompt.clone().unwrap_or_default();
        if let Some(ref skill_list) = args.skills {
            append_skills(&mut prompt, skill_list);
        }
        (prompt, None, ToolsConfig::default(), PermissionsConfig::default())
    };

    // agent frontmatter overrides config
    let tools = config.tools.clone().merge(agent_tools);
    let permissions = config.permissions.clone().merge(agent_permissions);

    let user_prompt = match get_user_prompt(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}\n", e);
            let mut cmd = Args::command();
            cmd.print_help().ok();
            println!();
            process::exit(1);
        }
    };

    let full_model_name = args.model.clone()
        .or(agent_model)
        .or(config.default_model.clone())
        .unwrap_or_else(|| "openai/gpt-4o".to_string());
        
    let (provider_name, model_name) = full_model_name.split_once('/').unwrap_or(("openai", &full_model_name));
    
    let default_provider_config = ProviderConfig {
        name: provider_name.to_string(),
        client: Some(provider_name.to_string()),
        ..Default::default()
    };
    
    let provider_config = config.providers.iter()
        .find(|p| p.name == provider_name)
        .unwrap_or(&default_provider_config);
        
    let client_type = provider_config.client.as_deref().unwrap_or(provider_name);

    let api_key = provider_config.api_key.clone()
        .unwrap_or_else(|| {
            let env_var_name = match client_type {
                "openai" | "openai_completions" | "openai_responses" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "gemini" => "GEMINI_API_KEY",
                "cohere" => "COHERE_API_KEY",
                "xai" => "XAI_API_KEY",
                _ => "",
            };
            env::var(env_var_name).unwrap_or_default()
        });

    let base_url = provider_config.base_url.clone();

    if api_key.is_empty() && !args.dry_run {
        eprintln!("error: no API key found for provider '{}'. configure it in ~/.config/airun/config.toml or export it as an environment variable.", provider_name);
        process::exit(1);
    }

    if args.verbose {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("rig=debug"))
            )
            .with_writer(io::stderr)
            .init();
        eprintln!("debug: using client type '{}'", client_type);
        eprintln!("debug: using model '{}'", model_name);
        if let Some(ref url) = base_url {
            eprintln!("debug: using base url '{}'", url);
        }
        eprintln!("debug: tools {:?}", tools);
        eprintln!("debug: permissions {:?}", permissions);
    }

    let max_tokens = args.max_tokens
        .or(config.default_max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let max_turns = config.default_max_turns.unwrap_or(DEFAULT_MAX_TURNS);

    if args.dry_run {
        println!("--- model ---");
        println!("{}/{}", provider_name, model_name);
        println!("max_tokens: {}", max_tokens);
        println!("max_turns: {}", max_turns);

        let active_tools: Vec<&str> = ["read", "bash"].iter()
            .filter(|t| tools.is_active(t, &args.tools))
            .copied()
            .collect();
        println!("\n--- tools ---");
        if active_tools.is_empty() {
            println!("(none)");
        } else {
            for name in &active_tools {
                println!("{}", name);
            }
        }

        println!("\n--- permissions ---");
        for name in &active_tools {
            if let Some(perm) = permissions.tools.get(*name) {
                println!("{}: {:?}", name, perm);
            }
        }

        println!("\n--- system prompt ---");
        println!("{}", system_prompt);

        println!("\n--- user prompt ---");
        println!("{}", user_prompt);

        return Ok(());
    }

    run_agent_stream(
        client_type,
        model_name,
        &api_key,
        base_url,
        max_tokens,
        max_turns,
        &tools,
        &args.tools,
        &permissions,
        &system_prompt,
        &user_prompt,
    ).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- bash command validation ---

    #[test]
    fn test_simple_commands_pass_validation() {
        assert!(is_simple_bash_command("apt update"));
        assert!(is_simple_bash_command("ls -la /home"));
        assert!(is_simple_bash_command("find /home -name '*.jpg'"));
    }

    #[test]
    fn test_metacharacters_fail_validation() {
        assert!(!is_simple_bash_command("ls | grep foo"));
        assert!(!is_simple_bash_command("apt update; rm -rf /"));
        assert!(!is_simple_bash_command("apt update && malicious"));
        assert!(!is_simple_bash_command("echo `whoami`"));
        assert!(!is_simple_bash_command("echo $(whoami)"));
        assert!(!is_simple_bash_command("echo hi > /etc/passwd"));
        assert!(!is_simple_bash_command("apt\\ update"));
        assert!(!is_simple_bash_command("apt update\nrm -rf /"));
    }

    // --- glob matching ---

    // --- glob matching (path mode) ---

    #[test]
    fn test_glob_exact_match() {
        assert!(glob_matches("apt update", "apt update", true).is_some());
        assert!(glob_matches("apt update", "apt upgrade", true).is_none());
    }

    #[test]
    fn test_glob_star_path_mode() {
        assert!(glob_matches("/etc/*", "/etc/passwd", true).is_some());
        // `*` should NOT cross `/` in path mode
        assert!(glob_matches("/etc/*", "/etc/ssh/config", true).is_none());
    }

    #[test]
    fn test_glob_star_command_mode() {
        // `*` crosses `/` in command mode
        assert!(glob_matches("ls *", "ls -la /home/user", false).is_some());
        assert!(glob_matches("*", "anything/with/slashes", false).is_some());
    }

    #[test]
    fn test_glob_doublestar_matches_across_slashes() {
        assert!(glob_matches("/home/**", "/home/user/photos/pic.jpg", true).is_some());
        assert!(glob_matches("/home/**/pic.jpg", "/home/user/photos/pic.jpg", true).is_some());
        assert!(glob_matches("**/*.jpg", "/home/user/pic.jpg", true).is_some());
    }

    #[test]
    fn test_glob_question_mark_path_mode() {
        assert!(glob_matches("file?.txt", "file1.txt", true).is_some());
        assert!(glob_matches("file?.txt", "file12.txt", true).is_none());
        // `?` should not match `/` in path mode
        assert!(glob_matches("a?b", "a/b", true).is_none());
    }

    #[test]
    fn test_glob_question_mark_command_mode() {
        // `?` matches `/` in command mode
        assert!(glob_matches("a?b", "a/b", false).is_some());
    }

    #[test]
    fn test_glob_bare_star_path_mode() {
        assert!(glob_matches("*", "anything", true).is_some());
        // bare `*` does not cross slashes in path mode
        assert!(glob_matches("*", "a/b", true).is_none());
        // but `**` does
        assert!(glob_matches("**", "a/b", true).is_some());
    }

    #[test]
    fn test_glob_specificity_ordering() {
        let exact = glob_matches("/etc/passwd", "/etc/passwd", true).unwrap();
        let glob = glob_matches("/etc/*", "/etc/passwd", true).unwrap();
        assert!(exact > glob);
    }

    // --- permission resolution (path mode for read) ---

    #[test]
    fn test_permission_path_patterns() {
        let mut patterns = HashMap::new();
        patterns.insert("**".to_string(), PermissionLevel::Deny);
        patterns.insert("/etc/os-release".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(perm.check("/etc/os-release", true), PermissionLevel::Allow);
        assert_eq!(perm.check("/etc/shadow", true), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_doublestar_dir_pattern() {
        let mut patterns = HashMap::new();
        patterns.insert("**".to_string(), PermissionLevel::Deny);
        patterns.insert("/home/**".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(perm.check("/home/user/file.txt", true), PermissionLevel::Allow);
        assert_eq!(perm.check("/etc/passwd", true), PermissionLevel::Deny);
    }

    // --- permission resolution (command mode for bash) ---

    #[test]
    fn test_permission_command_star_matches_slashes() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("ls *".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        // in command mode, "ls *" matches args with slashes
        assert_eq!(perm.check("ls -la /home/user", false), PermissionLevel::Allow);
        assert_eq!(perm.check("rm -rf /", false), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_command_most_specific_wins() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("apt update".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        assert_eq!(perm.check("apt update", false), PermissionLevel::Allow);
        assert_eq!(perm.check("rm -rf /", false), PermissionLevel::Deny);
    }

    #[test]
    fn test_permission_trailing_wildcard_matches_no_args() {
        let mut patterns = HashMap::new();
        patterns.insert("*".to_string(), PermissionLevel::Deny);
        patterns.insert("ls *".to_string(), PermissionLevel::Allow);
        let perm = Permission::Patterns(patterns);
        // "ls *" should also match bare "ls" (no args)
        assert_eq!(perm.check("ls", false), PermissionLevel::Allow);
        assert_eq!(perm.check("ls -la /home", false), PermissionLevel::Allow);
        assert_eq!(perm.check("rm", false), PermissionLevel::Deny);
    }
}
