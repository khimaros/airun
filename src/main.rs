use clap::Parser;
use rig::client::CompletionClient;
use rig::completion::ToolDefinition;
use rig::providers;
use rig::streaming::StreamingPrompt;
use rig::tool::{Tool, ToolError};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write, IsTerminal};
use std::path::{Path, PathBuf};
use std::process;
use futures_util::stream::StreamExt;
use tracing_subscriber::EnvFilter;

// --- permissions model (modeled after opencode.ai/docs/permissions) ---

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
enum PermissionLevel {
    Allow,
    Ask,
    Deny,
}

/// a permission can be a single level or a map of patterns to levels.
/// patterns use prefix matching with trailing `*` (eg. "/home/*").
/// when multiple patterns match, the most specific (longest prefix) wins.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Permission {
    Level(PermissionLevel),
    Patterns(HashMap<String, PermissionLevel>),
}

impl Permission {
    fn check(&self, input: &str) -> PermissionLevel {
        match self {
            Permission::Level(level) => level.clone(),
            Permission::Patterns(patterns) => {
                let mut result = PermissionLevel::Deny;
                let mut best_specificity = 0usize;
                for (pattern, level) in patterns {
                    if let Some(specificity) = pattern_matches(pattern, input) {
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

/// returns Some(specificity) if input matches the pattern, None otherwise.
/// specificity is the length of the non-wildcard prefix.
fn pattern_matches(pattern: &str, input: &str) -> Option<usize> {
    if pattern == "*" {
        return Some(0);
    }
    if pattern.ends_with('*') {
        let prefix = &pattern[..pattern.len() - 1];
        if input.starts_with(prefix) {
            return Some(prefix.len());
        }
    } else if pattern == input {
        return Some(pattern.len());
    }
    None
}

#[derive(Deserialize, Debug, Default, Clone)]
struct PermissionsConfig {
    read: Option<Permission>,
}

impl PermissionsConfig {
    fn merge(self, other: PermissionsConfig) -> PermissionsConfig {
        PermissionsConfig {
            read: other.read.or(self.read),
        }
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
struct ToolsConfig {
    read: Option<bool>,
}

impl ToolsConfig {
    fn merge(self, other: ToolsConfig) -> ToolsConfig {
        ToolsConfig {
            read: other.read.or(self.read),
        }
    }

    fn is_read_active(&self, tools_flag: bool) -> bool {
        self.read.unwrap_or(false) || tools_flag
    }
}

// --- read tool ---

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
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
        let level = self.permissions.read.as_ref()
            .map(|p| p.check(&path_str))
            .unwrap_or(PermissionLevel::Deny);
        match level {
            PermissionLevel::Allow => {}
            PermissionLevel::Ask => {
                eprintln!("permission ask (treating as deny): read {}", path_str);
                return Err(ToolError::ToolCallError(
                    format!("permission denied (ask): {}", path_str).into(),
                ));
            }
            PermissionLevel::Deny => {
                return Err(ToolError::ToolCallError(
                    format!("permission denied: {}", path_str).into(),
                ));
            }
        }
        fs::read_to_string(&resolved).map_err(|e| {
            ToolError::ToolCallError(format!("{}: {}", path_str, e).into())
        })
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

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// the name of the agent to run (optional)
    agent_name: Option<String>,

    /// initialize a default configuration file
    #[arg(long)]
    init: bool,

    /// prompt to send to the agent (if not provided via stdin)
    #[arg(short, long)]
    prompt: Option<String>,

    /// override the model to use (format: <provider_name>/<model_name>)
    #[arg(short, long)]
    model: Option<String>,

    /// maximum output tokens (default: 16384)
    #[arg(short = 't', long)]
    max_tokens: Option<u64>,

    /// enable configured tools
    #[arg(long)]
    tools: bool,

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
    #[serde(default)]
    tools: ToolsConfig,
    #[serde(default, alias = "permission")]
    permissions: PermissionsConfig,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
}

#[derive(Deserialize, Debug, Default)]
struct AgentFrontmatter {
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

fn load_agent(agent_name: &str) -> Result<ParsedDoc<AgentFrontmatter>, Box<dyn std::error::Error>> {
    let filename = format!("{}.md", agent_name);
    let path = find_file_in_dirs(&filename, &["agents"]).ok_or_else(|| {
        format!("Agent '{}' not found in .opencode/, .claude/, or .agents/ directories", agent_name)
    })?;
    
    let content = fs::read_to_string(path)?;
    parse_markdown_with_frontmatter(&content)
}

fn load_skill(skill_name: &str) -> Result<ParsedDoc<SkillFrontmatter>, Box<dyn std::error::Error>> {
    let filename_md = format!("{}.md", skill_name);
    let filename_skill_md = format!("{}/SKILL.md", skill_name);
    
    let path = find_file_in_dirs(&filename_skill_md, &["skills"])
        .or_else(|| find_file_in_dirs(&filename_md, &["skills"]))
        .ok_or_else(|| {
            format!("Skill '{}' not found in .opencode/, .claude/, or .agents/ directories", skill_name)
        })?;
    
    let content = fs::read_to_string(path)?;
    parse_markdown_with_frontmatter(&content)
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
    (client: $client:expr, $model_name:expr, $max_tokens:expr, $tools:expr, $tools_flag:expr, $permissions:expr, $system_prompt:expr, $user_prompt:expr) => {{
        let mut builder = $client.agent($model_name).max_tokens($max_tokens);
        if !$system_prompt.is_empty() {
            builder = builder.preamble($system_prompt);
        }
        add_tools_and_stream!(builder, $tools, $tools_flag, $permissions, $user_prompt);
    }};
    (model: $model:expr, $max_tokens:expr, $tools:expr, $tools_flag:expr, $permissions:expr, $system_prompt:expr, $user_prompt:expr) => {{
        let mut builder = rig::agent::AgentBuilder::new($model).max_tokens($max_tokens);
        if !$system_prompt.is_empty() {
            builder = builder.preamble($system_prompt);
        }
        add_tools_and_stream!(builder, $tools, $tools_flag, $permissions, $user_prompt);
    }};
}

macro_rules! add_tools_and_stream {
    ($builder:expr, $tools:expr, $tools_flag:expr, $permissions:expr, $user_prompt:expr) => {{
        if $tools.is_read_active($tools_flag) {
            let read_tool = ReadTool { permissions: $permissions.clone() };
            let agent = $builder.tool(read_tool).build();
            stream_agent!(agent, $user_prompt);
        } else {
            let agent = $builder.build();
            stream_agent!(agent, $user_prompt);
        }
    }};
}

fn build_system_prompt(agent: &ParsedDoc<AgentFrontmatter>) -> String {
    let mut system_prompt = agent.body.clone();
    
    if let Some(skills) = &agent.frontmatter.skills {
        if !skills.is_empty() {
            system_prompt.push_str("\n\n# skills\n");
            for skill_name in skills {
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
        return Err("error: empty input from stdin or --prompt".into());
    }
    
    Ok(user_prompt)
}

async fn run_agent_stream(
    client_type: &str,
    model_name: &str,
    api_key: &str,
    base_url: Option<String>,
    max_tokens: u64,
    tools: &ToolsConfig,
    tools_flag: bool,
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
            build_and_stream!(model: model, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        "openai" | "openai_responses" => {
            let client = build_openai_client!();
            build_and_stream!(client: client, model_name, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        "anthropic" => {
            let mut builder = providers::anthropic::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Anthropic client");
            build_and_stream!(client: client, model_name, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        "gemini" => {
            let mut builder = providers::gemini::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Gemini client");
            build_and_stream!(client: client, model_name, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        "cohere" => {
            let mut builder = providers::cohere::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build Cohere client");
            build_and_stream!(client: client, model_name, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        "xai" => {
            let mut builder = providers::xai::Client::builder().api_key(api_key);
            if let Some(url) = base_url {
                builder = builder.base_url(&url);
            }
            let client = builder.build().expect("failed to build xAI client");
            build_and_stream!(client: client, model_name, max_tokens, tools, tools_flag, permissions, system_prompt, user_prompt);
        },
        _ => return Err(format!("unsupported client type: {}", client_type).into()),
    }
    
    println!();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    if args.init {
        if let Err(e) = init_config() {
            eprintln!("error initializing config: {}", e);
            process::exit(1);
        }
        process::exit(0);
    }

    let config = load_config()?;
    
    let (system_prompt, agent_model, agent_tools, agent_permissions) = if let Some(agent_name) = &args.agent_name {
        let agent = load_agent(agent_name)?;
        let prompt = build_system_prompt(&agent);
        (prompt, agent.frontmatter.model, agent.frontmatter.tools, agent.frontmatter.permissions)
    } else {
        (String::new(), None, ToolsConfig::default(), PermissionsConfig::default())
    };

    // agent frontmatter overrides config
    let tools = config.tools.clone().merge(agent_tools);
    let permissions = config.permissions.clone().merge(agent_permissions);

    let user_prompt = match get_user_prompt(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
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

    if api_key.is_empty() {
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

    run_agent_stream(
        client_type,
        model_name,
        &api_key,
        base_url,
        max_tokens,
        &tools,
        args.tools,
        &permissions,
        &system_prompt,
        &user_prompt,
    ).await
}
