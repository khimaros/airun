use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn setup_project(dir: &TempDir, agent_content: &str) {
    let agents_dir = dir.path().join(".agents").join("agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(agents_dir.join("test-agent.md"), agent_content).unwrap();
    // create a .git dir so discovery stops here
    fs::create_dir_all(dir.path().join(".git")).unwrap();
}

fn airun(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("airun").unwrap();
    cmd.current_dir(dir.path());
    cmd.env("HOME", dir.path());
    cmd
}

// --- permission pattern matching ---

#[test]
fn test_wildcard_denies_by_default() {
    // ensures that "**": deny blocks unmatched patterns
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
tools:
  bash: true
permissions:
  bash:
    "**": deny
    "echo hello": allow
---
test agent
"#);
    // we can't easily test tool invocation without a real LLM,
    // but we can verify the agent loads and the CLI parses correctly
    let result = airun(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .arg("-v")
        .write_stdin("")
        .assert();
    // should fail due to missing API key, but after parsing agent config
    result.failure()
        .stderr(predicate::str::contains("no API key found"));
}

#[test]
fn test_agent_with_bash_and_read_tools() {
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
tools:
  read: true
  bash: true
permissions:
  read:
    "**": allow
  bash:
    "**": deny
    "ls **": allow
---
test agent
"#);
    let result = airun(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .arg("-v")
        .write_stdin("")
        .assert();
    result.failure()
        .stderr(predicate::str::contains("no API key found"));
}

// --- CLI flag parsing ---

#[test]
fn test_dry_run_outputs_prompts() {
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
tools:
  bash: true
permissions:
  bash:
    "*": deny
    "echo *": allow
---
you are a test agent
"#);
    airun(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("hello world")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("--- system prompt ---")
            .and(predicate::str::contains("you are a test agent"))
            .and(predicate::str::contains("--- user prompt ---"))
            .and(predicate::str::contains("hello world"))
            .and(predicate::str::contains("bash")));
}

#[test]
fn test_yes_flag_accepted() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    airun(&dir)
        .arg("-y")
        .arg("-p")
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no API key found"));
}

#[test]
fn test_system_prompt_flag() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let result = airun(&dir)
        .arg("-s")
        .arg("you are a pirate")
        .arg("-p")
        .arg("hello")
        .arg("-v")
        .write_stdin("")
        .assert();
    result.failure()
        .stderr(predicate::str::contains("no API key found"));
}

#[test]
fn test_system_prompt_overrides_agent() {
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
description: original agent
---
original system prompt
"#);
    let result = airun(&dir)
        .arg("test-agent")
        .arg("-s")
        .arg("override prompt")
        .arg("-p")
        .arg("hello")
        .arg("-v")
        .write_stdin("")
        .assert();
    result.failure()
        .stderr(predicate::str::contains("no API key found"));
}

// --- init ---

#[test]
fn test_init_creates_config() {
    let dir = TempDir::new().unwrap();
    airun(&dir)
        .arg("--init")
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized configuration"));

    let config_path = dir.path().join(".config").join("airun").join("config.toml");
    assert!(config_path.exists());
}

#[test]
fn test_init_does_not_overwrite() {
    let dir = TempDir::new().unwrap();
    let config_dir = dir.path().join(".config").join("airun");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), "existing").unwrap();

    airun(&dir)
        .arg("--init")
        .assert()
        .success()
        .stderr(predicate::str::contains("already exists"));

    let content = fs::read_to_string(config_dir.join("config.toml")).unwrap();
    assert_eq!(content, "existing");
}

// --- bash command validation (unit-style via the binary) ---
// these test that shell metacharacters are rejected before permission checks.
// since we can't invoke tools without an LLM, we test the validate function
// indirectly through the module. for now, the compile+parse tests above cover
// that the tool is wired in correctly.

// --- error messages ---

#[test]
fn test_invalid_permission_level_shows_value() {
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
tools:
  bash: true
permissions:
  bash:
    "*": dennny
---
test agent
"#);
    airun(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid permission level 'dennny'")
            .and(predicate::str::contains("agent 'test-agent'")));
}

#[test]
fn test_parse_error_includes_agent_name() {
    let dir = TempDir::new().unwrap();
    setup_project(&dir, r#"---
permissions:
  read: 12345
---
test
"#);
    airun(&dir)
        .arg("test-agent")
        .arg("-p")
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("agent 'test-agent'"));
}

// --- empty input ---

#[test]
fn test_empty_prompt_shows_usage() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    airun(&dir)
        .write_stdin("")
        .assert()
        .failure()
        .stdout(predicate::str::contains("Usage:"))
        .stderr(predicate::str::contains("one of --prompt or stdin must be non-empty"));
}
