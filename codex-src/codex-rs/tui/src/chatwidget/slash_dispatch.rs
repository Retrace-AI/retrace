//! Slash-command dispatch and local-recall handoff for `ChatWidget`.
//!
//! `ChatComposer` parses slash input and stages recognized command text for local
//! Up-arrow recall before returning an input result. This module owns the app-level
//! dispatch step and records the staged entry once the command has been handled, so
//! slash-command recall follows the same submitted-input rule as ordinary text.

use super::goal_validation::GoalObjectiveValidationSource;
use super::*;
use crate::app_event::CodexOsProviderConfigureResult;
use crate::app_event::CodexOsProviderModelRow;
use crate::app_event::CodexOsProviderRemoveSnapshot;
use crate::app_event::CodexOsProviderRow;
use crate::app_event::DeleteConversationRow;
use crate::app_event::DeleteConversationTarget;
use crate::app_event::ThreadGoalSetMode;
use crate::bottom_pane::MultiSelectItem;
use crate::bottom_pane::MultiSelectPicker;
use crate::bottom_pane::prompt_args::parse_slash_name;
use crate::bottom_pane::slash_commands::BuiltinCommandFlags;
use crate::bottom_pane::slash_commands::ServiceTierCommand;
use crate::bottom_pane::slash_commands::SlashCommandItem;
use crate::bottom_pane::slash_commands::find_slash_command;
use crate::goal_display::GOAL_USAGE;
use codex_ansi_escape::ansi_escape_line;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelsResponse;
use ratatui::text::Line;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use url::Url;

/// Sentinel row id used in the `/model add` multiselect for the "add a custom
/// model / connect a new provider" entry. It is not a real model; selecting it
/// routes to the provider-add prompt instead of the enable/disable apply.
const ADD_CUSTOM_MODEL_SENTINEL: &str = "__retrace_add_custom_model__";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlashCommandDispatchSource {
    Live,
    Queued,
}

struct PreparedSlashCommandArgs {
    args: String,
    text_elements: Vec<TextElement>,
    local_images: Vec<LocalImageAttachment>,
    remote_image_urls: Vec<String>,
    mention_bindings: Vec<MentionBinding>,
    source: SlashCommandDispatchSource,
}

const SIDE_STARTING_CONTEXT_LABEL: &str = "Side starting...";
const SIDE_SLASH_COMMAND_UNAVAILABLE_HINT: &str =
    "Press Ctrl+C to return to the main thread first.";
const GOAL_USAGE_HINT: &str = "Example: /goal improve benchmark coverage";
const RAW_USAGE: &str = "Usage: /raw [on|off]";
const PROVIDER_HELP: &str = concat!(
    "Usage: /provider [add [url] | list | remove [provider-id] | help]\n",
    "\n",
    "Provider commands:\n",
    "  /provider\n",
    "    Same as /provider add. Prompts for API base URL, then API key.\n",
    "  /provider add [url]\n",
    "    Connects an OpenAI-compatible provider. It first lists models only; it does not probe thinking yet. After you select models, it probes only those selected models and adds usable ones to /model.\n",
    "  /provider list\n",
    "    Shows configured provider ids, base URLs, provider kind/dialect, and key status.\n",
    "  /provider remove [provider-id]\n",
    "    Lets you remove selected models from a provider or remove the entire provider.\n",
    "\n",
    "Provider/model labels:\n",
    "  provider-id: normalized id derived from the API host, used to group models.\n",
    "  base-url: provider API endpoint, normalized to a /v1-style URL when needed.\n",
    "  key: file/env/missing status; the key value is never shown.\n",
    "  on/off: whether a model is visible in /model.\n",
    "  context: detected or default context window.\n",
    "  output: output token cap; default means the provider/model default.\n",
    "  cap_thinking: whether the selected model passed a thinking/reasoning probe.\n",
    "  levels: detected reasoning choices such as off/on or low/medium/high.\n",
    "  method: provider option used for thinking, when detected.\n",
    "  usable: whether the selected model passed the capability probe.\n",
    "  normalize: whether system prompts are normalized before sending."
);
const AGENTCHECK_USAGE: &str = "Usage: /agentcheck [on|off|status|help]";
const AGENTCHECK_STATE_FILE: &str = "agentcheck";

#[derive(Debug, Deserialize)]
struct CodexOsProviderJson {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexOsProviderModelJson {
    id: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "upstreamModel")]
    upstream_model: Option<String>,
    #[serde(default)]
    context: Option<i64>,
    #[serde(default)]
    output: Option<serde_json::Value>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default, rename = "capThinking")]
    cap_thinking: bool,
    #[serde(default = "default_true")]
    usable: bool,
    #[serde(default, rename = "thinkingLevels")]
    thinking_levels: Vec<String>,
    #[serde(default, rename = "thinkingMethod")]
    thinking_method: Option<String>,
}

fn default_true() -> bool {
    true
}

fn codexos_home_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEXOS_HOME") {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".codexopensource");
    }
    PathBuf::from(".codexopensource")
}

fn agentcheck_state_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEXOS_AGENT_CHECK_FILE") {
        return PathBuf::from(path);
    }
    codexos_home_dir().join(AGENTCHECK_STATE_FILE)
}

fn parse_agentcheck_enabled(value: &str, fallback: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" | "enabled" | "enable" => true,
        "off" | "false" | "0" | "no" | "disabled" | "disable" => false,
        "" => fallback,
        _ => fallback,
    }
}

fn read_agentcheck_enabled() -> bool {
    let env_enabled = std::env::var("CODEXOS_AGENT_CHECK")
        .or_else(|_| std::env::var("CODEXOS_AGENT_CHECK_ENABLED"))
        .map(|value| parse_agentcheck_enabled(&value, false))
        .unwrap_or(false);
    match std::fs::read_to_string(agentcheck_state_path()) {
        Ok(text) => parse_agentcheck_enabled(&text, env_enabled),
        Err(_) => env_enabled,
    }
}

fn write_agentcheck_enabled(enabled: bool) -> Result<(), String> {
    let path = agentcheck_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    std::fs::write(&path, if enabled { "on\n" } else { "off\n" })
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn agentcheck_state_label(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[derive(Debug)]
pub(crate) struct CodexOsLocalCommand {
    label: String,
    program: PathBuf,
    args: Vec<String>,
}

fn codexos_bin(env_var: &str, binary: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_var) {
        return PathBuf::from(path);
    }
    // Legacy env name kept for compatibility with older setups.
    if env_var == "RETRACE_ADMIN"
        && let Some(path) = std::env::var_os("CODEXOS_ADMIN")
    {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/bin").join(binary);
    }
    PathBuf::from(binary)
}

pub(crate) fn codexos_admin_command(args: Vec<String>) -> CodexOsLocalCommand {
    let program = codexos_bin("RETRACE_ADMIN", "retrace-admin");
    let label = codexos_label(&program, &args);
    CodexOsLocalCommand {
        label,
        program,
        args,
    }
}

fn codexos_label(program: &std::path::Path, args: &[String]) -> String {
    let program_name = program
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| program.to_str().unwrap_or("codexos-command"));
    if args.is_empty() {
        program_name.to_string()
    } else {
        format!("{program_name} {}", args.join(" "))
    }
}

async fn run_codexos_local_command(command: CodexOsLocalCommand) -> String {
    let CodexOsLocalCommand {
        label,
        program,
        args,
    } = command;
    let mut text = format!("$ {label}\n");
    match Command::new(&program).args(&args).output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stdout.is_empty() {
                text.push_str(stdout.trim_end());
                text.push('\n');
            }
            if !stderr.is_empty() {
                text.push_str(stderr.trim_end());
                text.push('\n');
            }
            if !output.status.success() {
                text.push_str(&format!(
                    "[exit status: {}]\n",
                    codexos_status_display(output.status)
                ));
            }
        }
        Err(err) => {
            text.push_str(&format!("failed to run {}: {err}\n", program.display()));
        }
    }
    text
}

fn codexos_status_display(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string())
}

pub(crate) async fn run_codexos_local_command_checked(
    command: CodexOsLocalCommand,
) -> Result<String, String> {
    let CodexOsLocalCommand {
        label,
        program,
        args,
    } = command;
    let mut text = format!("$ {label}\n");
    match Command::new(&program).args(&args).output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stdout.is_empty() {
                text.push_str(stdout.trim_end());
                text.push('\n');
            }
            if !stderr.is_empty() {
                text.push_str(stderr.trim_end());
                text.push('\n');
            }
            if output.status.success() {
                Ok(text)
            } else {
                text.push_str(&format!(
                    "[exit status: {}]\n",
                    codexos_status_display(output.status)
                ));
                Err(text)
            }
        }
        Err(err) => Err(format!("failed to run {}: {err}\n", program.display())),
    }
}

/// Like [`run_codexos_local_command_checked`], but streams the child's **stderr**
/// line-by-line to the UI as live probe-progress events while capturing its
/// **stdout** for the returned display text. Provider/model capability probes
/// (`retrace-admin models probe …`) emit their per-step progress on stderr and
/// keep stdout for the human-readable summary, so this lets the user watch each
/// step ("reasoning effort… streaming… cache…") instead of staring at a spinner.
async fn run_codexos_local_command_streaming(
    command: CodexOsLocalCommand,
    tx: AppEventSender,
) -> Result<String, String> {
    let CodexOsLocalCommand {
        label,
        program,
        args,
    } = command;
    let mut text = format!("$ {label}\n");
    let mut child = match Command::new(&program)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return Err(format!("failed to run {}: {err}\n", program.display())),
    };

    // Forward each stderr line to the UI as a progress event as it arrives.
    let stderr_task = child.stderr.take().map(|stderr| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut collected = String::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    tx.send(AppEvent::CodexOsProbeProgress {
                        line: trimmed.to_string(),
                    });
                }
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        })
    });

    // Read stdout to completion (the human-readable summary).
    let mut stdout_buf = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut stdout_buf).await;
    }

    let status = child.wait().await;
    let stderr_text = match stderr_task {
        Some(handle) => handle.await.unwrap_or_default(),
        None => String::new(),
    };

    let stdout = String::from_utf8_lossy(&stdout_buf);
    if !stdout.trim().is_empty() {
        text.push_str(stdout.trim_end());
        text.push('\n');
    }
    match status {
        Ok(status) if status.success() => Ok(text),
        Ok(status) => {
            if !stderr_text.trim().is_empty() {
                text.push_str(stderr_text.trim_end());
                text.push('\n');
            }
            text.push_str(&format!(
                "[exit status: {}]\n",
                codexos_status_display(status)
            ));
            Err(text)
        }
        Err(err) => Err(format!("failed to run {}: {err}\n", program.display())),
    }
}

fn codexos_output_error(label: &str, output: std::process::Output) -> String {
    let mut text = format!("{label} failed\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        text.push_str(stdout.trim_end());
        text.push('\n');
    }
    if !stderr.is_empty() {
        text.push_str(stderr.trim_end());
        text.push('\n');
    }
    text.push_str(&format!(
        "[exit status: {}]\n",
        codexos_status_display(output.status)
    ));
    text
}

async fn connect_codexos_provider_with_key(
    provider_id: String,
    base_url: String,
    api_key: String,
) -> Result<Vec<CodexOsProviderModelRow>, String> {
    let program = codexos_bin("RETRACE_ADMIN", "retrace-admin");
    let args = vec![
        "provider".to_string(),
        "connect".to_string(),
        provider_id.clone(),
        "--base-url".to_string(),
        base_url,
        "--stdin".to_string(),
    ];
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run {}: {err}", program.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(api_key.trim().as_bytes())
            .await
            .map_err(|err| format!("failed to write API key to provider setup: {err}"))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|err| format!("failed to finish API key input: {err}"))?;
    } else {
        return Err("provider setup could not open stdin for the API key".to_string());
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| format!("provider setup did not finish: {err}"))?;
    if !output.status.success() {
        return Err(codexos_output_error("provider connect", output));
    }

    list_codexos_provider_models(&provider_id).await
}

async fn list_codexos_providers() -> Result<Vec<CodexOsProviderRow>, String> {
    let program = codexos_bin("RETRACE_ADMIN", "retrace-admin");
    let output = Command::new(&program)
        .args(["provider", "list", "--json"])
        .output()
        .await
        .map_err(|err| format!("failed to run {}: {err}", program.display()))?;
    if !output.status.success() {
        return Err(codexos_output_error("provider list --json", output));
    }

    let json_rows: Vec<CodexOsProviderJson> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse provider JSON: {err}"))?;
    Ok(json_rows
        .into_iter()
        .map(|row| CodexOsProviderRow {
            id: row.id.clone(),
            name: row.name.unwrap_or(row.id),
            base_url: row.base_url.unwrap_or_default(),
            key: row.key.unwrap_or_else(|| "unknown".to_string()),
        })
        .collect())
}

async fn list_codexos_all_models() -> Result<Vec<CodexOsProviderModelRow>, String> {
    let program = codexos_bin("RETRACE_ADMIN", "retrace-admin");
    let output = Command::new(&program)
        .args(["models", "list", "--all", "--json"])
        .output()
        .await
        .map_err(|err| format!("failed to run {}: {err}", program.display()))?;
    if !output.status.success() {
        return Err(codexos_output_error("models list --all --json", output));
    }

    let json_rows: Vec<CodexOsProviderModelJson> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse model JSON: {err}"))?;
    Ok(json_rows
        .into_iter()
        .map(codexos_model_row_from_json)
        .collect())
}

/// Prior per-model context/output limits, as previously saved via
/// `models set <model> --context <n> --output <n>`. Used so the `/model`
/// sizing popups can pre-select the value the user chose last time, letting
/// them just press Enter to keep it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SavedModelLimits {
    pub(crate) context_window: Option<i64>,
    pub(crate) output_tokens: Option<i64>,
}

/// Look up the saved context/output limits for a single model id. Returns
/// `SavedModelLimits::default()` (all `None`) when the model has no saved
/// limits yet or the registry cannot be read — the popups then fall back to
/// their normal, un-defaulted presentation.
pub(crate) async fn fetch_saved_model_limits(model: String) -> SavedModelLimits {
    let Ok(rows) = list_codexos_all_models().await else {
        return SavedModelLimits::default();
    };
    let Some(row) = rows.into_iter().find(|row| row.id == model) else {
        return SavedModelLimits::default();
    };
    let output_tokens = parse_token_count(&row.output);
    SavedModelLimits {
        context_window: row.context,
        output_tokens,
    }
}

/// Re-fetches the model list from every connected provider (so newly-added
/// upstream models appear), then returns the full registry model list. Used by
/// `/model add` so the picker reflects the provider's *current* catalog, not
/// just what was registered on the last connect.
async fn refresh_and_list_all_models() -> Result<Vec<CodexOsProviderModelRow>, String> {
    let providers = list_codexos_providers().await.unwrap_or_default();
    let program = codexos_bin("RETRACE_ADMIN", "retrace-admin");
    for provider in &providers {
        // Best-effort: a provider that's briefly unreachable shouldn't block the
        // picker — we still list whatever is already registered.
        let _ = Command::new(&program)
            .args(["models", "refresh", "--provider", provider.id.as_str()])
            .output()
            .await;
    }
    list_codexos_all_models().await
}

/// Enumerates saved conversations (rollout files) under `<codex_home>/sessions`,
/// newest first, each labelled with its date and first user message.
async fn list_saved_conversations(codex_home: std::path::PathBuf) -> Vec<DeleteConversationRow> {
    tokio::task::spawn_blocking(move || {
        let root = codex_home.join("sessions");
        let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
        for entry in walkdir_rollouts(&root) {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, entry));
        }
        files.sort_by(|a, b| b.0.cmp(&a.0));
        files
            .into_iter()
            .map(|(_, path)| {
                let label = conversation_label(&path);
                DeleteConversationRow { path, label }
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Recursively collects `rollout-*.jsonl` files under `root`.
fn walkdir_rollouts(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            {
                out.push(path);
            }
        }
    }
    out
}

/// Builds a short label for a conversation: `YYYY-MM-DD HH:MM · <first user msg>`.
fn conversation_label(path: &std::path::Path) -> String {
    let stamp = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("rollout-"))
        .map(|rest| rest.chars().take(16).collect::<String>().replace('T', " "))
        .unwrap_or_default();
    let preview = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines().take(60).find_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                let payload = v.get("payload").unwrap_or(&v);
                if payload.get("type").and_then(|t| t.as_str()) == Some("user_message") {
                    payload
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.trim().replace('\n', " "))
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "(no messages)".to_string());
    let preview: String = preview.chars().take(60).collect();
    if stamp.is_empty() {
        preview
    } else {
        format!("{stamp} · {preview}")
    }
}

/// Deletes the target conversation(s); returns how many files were removed.
async fn delete_conversations(
    codex_home: std::path::PathBuf,
    target: DeleteConversationTarget,
) -> Result<usize, String> {
    tokio::task::spawn_blocking(move || {
        let paths: Vec<std::path::PathBuf> = match target {
            DeleteConversationTarget::One(path) => vec![path],
            DeleteConversationTarget::All => walkdir_rollouts(&codex_home.join("sessions")),
        };
        let mut removed = 0usize;
        for path in paths {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(err) => return Err(format!("failed to delete {}: {err}", path.display())),
            }
        }
        Ok(removed)
    })
    .await
    .map_err(|err| format!("delete task failed: {err}"))?
}

async fn load_codexos_provider_remove_snapshot() -> Result<CodexOsProviderRemoveSnapshot, String> {
    let providers = list_codexos_providers().await?;
    let models = list_codexos_all_models().await?;
    Ok(CodexOsProviderRemoveSnapshot { providers, models })
}

async fn list_codexos_provider_models(
    provider_id: &str,
) -> Result<Vec<CodexOsProviderModelRow>, String> {
    Ok(list_codexos_all_models()
        .await?
        .into_iter()
        .filter(|row| row.provider == provider_id)
        .collect())
}

fn codexos_model_row_from_json(row: CodexOsProviderModelJson) -> CodexOsProviderModelRow {
    let output = match row.output {
        Some(serde_json::Value::String(value)) => value,
        Some(serde_json::Value::Number(value)) => value.to_string(),
        Some(serde_json::Value::Null) | None => "default".to_string(),
        Some(other) => other.to_string(),
    };
    CodexOsProviderModelRow {
        upstream_model: row.upstream_model.clone().unwrap_or_else(|| row.id.clone()),
        provider: row.provider.unwrap_or_default(),
        id: row.id,
        enabled: row.enabled,
        context: row.context,
        output,
        thinking: row.thinking.unwrap_or_else(|| "auto".to_string()),
        cap_thinking: row.cap_thinking,
        usable: row.usable,
        thinking_levels: row.thinking_levels,
        thinking_method: row.thinking_method,
    }
}

async fn configure_codexos_provider_models(
    provider_id: String,
    model_ids: Vec<String>,
    tx: AppEventSender,
) -> Result<CodexOsProviderConfigureResult, String> {
    if model_ids.is_empty() {
        return Err("No models were selected.".to_string());
    }

    let mut output = "/provider probe selected models\n\n".to_string();
    let mut probe_args = vec!["models".to_string(), "probe".to_string()];
    probe_args.extend(model_ids.iter().cloned());
    probe_args.extend(["--provider".to_string(), provider_id.clone()]);
    output.push_str(
        &run_codexos_local_command_streaming(codexos_admin_command(probe_args), tx.clone()).await?,
    );

    let rows = list_codexos_provider_models(&provider_id).await?;
    let usable_model_ids: Vec<String> = if rows.is_empty() {
        model_ids.clone()
    } else {
        model_ids
            .iter()
            .filter(|model_id| {
                rows.iter()
                    .find(|row| row.id == **model_id)
                    .is_some_and(|row| row.usable)
            })
            .cloned()
            .collect()
    };
    let skipped_model_ids: Vec<String> = model_ids
        .iter()
        .filter(|model_id| !usable_model_ids.iter().any(|usable| usable == *model_id))
        .cloned()
        .collect();
    if !skipped_model_ids.is_empty() {
        output.push_str(&format!(
            "\nskipped unusable model(s): {}\n",
            skipped_model_ids.join(", ")
        ));
    }
    if usable_model_ids.is_empty() {
        output.push_str(
            "\nNo selected models passed the capability probe; nothing was added to /model.\n",
        );
        return Err(output);
    }

    output.push('\n');
    let mut enable_args = vec!["models".to_string(), "enable".to_string()];
    enable_args.extend(usable_model_ids.iter().cloned());
    output.push_str(&run_codexos_local_command_checked(codexos_admin_command(enable_args)).await?);

    output.push('\n');
    output.push_str(
        &run_codexos_local_command_checked(codexos_admin_command(vec![
            "models".to_string(),
            "list".to_string(),
            "--all".to_string(),
        ]))
        .await?,
    );
    let model_catalog = load_codexos_model_catalog()?;
    Ok(CodexOsProviderConfigureResult {
        output,
        model_catalog,
    })
}

/// Runs the live capability probe (thinking formats, effort levels, cache,
/// streaming) for one model and returns the refreshed catalog for hot reload.
async fn reprobe_codexos_model(
    model_id: String,
    tx: AppEventSender,
) -> Result<CodexOsProviderConfigureResult, String> {
    let mut output = format!("/model reprobe {model_id}\n\n");
    output.push_str(
        &run_codexos_local_command_streaming(
            codexos_admin_command(vec!["models".to_string(), "probe".to_string(), model_id]),
            tx.clone(),
        )
        .await?,
    );
    let model_catalog = load_codexos_model_catalog()?;
    Ok(CodexOsProviderConfigureResult {
        output,
        model_catalog,
    })
}

/// Applies a `/model add` selection diff: newly checked models are probed and
/// enabled (grouped per provider), unchecked previously-enabled models are
/// disabled, and the refreshed catalog is returned for hot reload.
/// Parses a user-entered token count. Rejects anything that is not a whole
/// number of at least 1024, so a typo cannot silently shrink a context window.
/// Accepts thousands separators and a `k`/`m` suffix (e.g. `128k`, `1m`).
pub(crate) fn parse_token_count(value: &str) -> Option<i64> {
    let text = value.trim().replace([',', '_'], "").to_lowercase();
    let (digits, multiplier) = match text.strip_suffix('k') {
        Some(rest) => (rest, 1_024),
        None => match text.strip_suffix('m') {
            // A bare "1m" means a round 1,000,000: overstating a 1M window
            // causes context-overflow rejections, so never round it up.
            Some(rest) => (rest, 1_000_000),
            None => (text.as_str(), 1),
        },
    };
    let parsed: i64 = digits.trim().parse().ok()?;
    let tokens = parsed.checked_mul(multiplier)?;
    (tokens >= 1024).then_some(tokens)
}

async fn apply_model_add_selection(
    rows: Vec<CodexOsProviderModelRow>,
    selected_ids: Vec<String>,
    tx: AppEventSender,
) -> Result<CodexOsProviderConfigureResult, String> {
    use std::collections::BTreeMap;
    use std::collections::HashSet;
    let selected: HashSet<&str> = selected_ids.iter().map(String::as_str).collect();
    let mut enable_by_provider: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut to_disable: Vec<String> = Vec::new();
    for row in &rows {
        let is_selected = selected.contains(row.id.as_str());
        if is_selected && !row.enabled {
            enable_by_provider
                .entry(row.provider.clone())
                .or_default()
                .push(row.id.clone());
        } else if !is_selected && row.enabled {
            to_disable.push(row.id.clone());
        }
    }
    if enable_by_provider.is_empty() && to_disable.is_empty() {
        return Err("No changes: the selection matches the currently enabled models.".to_string());
    }

    let mut output = "/model add\n\n".to_string();
    for (provider_id, model_ids) in &enable_by_provider {
        let mut probe_args = vec!["models".to_string(), "probe".to_string()];
        probe_args.extend(model_ids.iter().cloned());
        probe_args.extend(["--provider".to_string(), provider_id.clone()]);
        output.push_str(
            &run_codexos_local_command_streaming(codexos_admin_command(probe_args), tx.clone())
                .await?,
        );
        output.push('\n');
        let mut enable_args = vec!["models".to_string(), "enable".to_string()];
        enable_args.extend(model_ids.iter().cloned());
        output.push_str(
            &run_codexos_local_command_checked(codexos_admin_command(enable_args)).await?,
        );
        output.push('\n');
    }
    if !to_disable.is_empty() {
        let mut disable_args = vec!["models".to_string(), "disable".to_string()];
        disable_args.extend(to_disable.iter().cloned());
        output.push_str(
            &run_codexos_local_command_checked(codexos_admin_command(disable_args)).await?,
        );
        output.push('\n');
    }
    let model_catalog = load_codexos_model_catalog()?;
    Ok(CodexOsProviderConfigureResult {
        output,
        model_catalog,
    })
}

async fn remove_codexos_provider(
    provider_id: String,
) -> Result<CodexOsProviderConfigureResult, String> {
    let mut output = format!("/provider remove {provider_id}\n\n");
    output.push_str(
        &run_codexos_local_command_checked(codexos_admin_command(vec![
            "provider".to_string(),
            "remove".to_string(),
            provider_id,
        ]))
        .await?,
    );
    output.push('\n');
    output.push_str(
        &run_codexos_local_command_checked(codexos_admin_command(vec![
            "models".to_string(),
            "list".to_string(),
            "--all".to_string(),
        ]))
        .await?,
    );
    let model_catalog = load_codexos_model_catalog()?;
    Ok(CodexOsProviderConfigureResult {
        output,
        model_catalog,
    })
}

async fn remove_codexos_provider_models(
    provider_id: String,
    model_ids: Vec<String>,
) -> Result<CodexOsProviderConfigureResult, String> {
    if model_ids.is_empty() {
        return Err("No models were selected.".to_string());
    }

    let mut output = format!(
        "/provider remove {provider_id} selected models: {}\n\n",
        model_ids.join(", ")
    );
    let mut remove_args = vec!["models".to_string(), "remove".to_string()];
    remove_args.extend(model_ids);
    remove_args.extend(["--provider".to_string(), provider_id]);
    output.push_str(&run_codexos_local_command_checked(codexos_admin_command(remove_args)).await?);
    output.push('\n');
    output.push_str(
        &run_codexos_local_command_checked(codexos_admin_command(vec![
            "models".to_string(),
            "list".to_string(),
            "--all".to_string(),
        ]))
        .await?,
    );
    let model_catalog = load_codexos_model_catalog()?;
    Ok(CodexOsProviderConfigureResult {
        output,
        model_catalog,
    })
}

fn codexos_model_catalog_path() -> PathBuf {
    for env_var in ["RETRACE_MODEL_CATALOG_JSON", "CODEXOS_MODEL_CATALOG_JSON"] {
        if let Some(path) = std::env::var_os(env_var) {
            return PathBuf::from(path);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".retrace").join("models.json");
    }
    PathBuf::from("models.json")
}

fn load_codexos_model_catalog() -> Result<Vec<ModelPreset>, String> {
    let path = codexos_model_catalog_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read model catalog {}: {err}", path.display()))?;
    let response: ModelsResponse = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse model catalog {}: {err}", path.display()))?;
    let mut presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    Ok(presets)
}

fn codexos_provider_id_from_url(base_url: &str) -> String {
    let host = Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| base_url.to_string());
    let host = host
        .trim_start_matches("www.")
        .trim_start_matches("api.")
        .to_ascii_lowercase();
    let mut id = String::new();
    let mut last_was_dash = false;
    for ch in host.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !id.is_empty() {
            id.push('-');
            last_was_dash = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    if id.is_empty() {
        "provider".to_string()
    } else {
        id
    }
}

fn codexos_model_description(row: &CodexOsProviderModelRow) -> String {
    let context = row
        .context
        .map(|value| format!("{value} ctx"))
        .unwrap_or_else(|| "unknown ctx".to_string());
    let output = format!("out {}", row.output);
    format!("{context}; {output}; capability probe pending")
}

fn codexos_model_registry_description(row: &CodexOsProviderModelRow) -> String {
    let state = if row.enabled { "on" } else { "off" };
    let context = row
        .context
        .map(|value| format!("{value} ctx"))
        .unwrap_or_else(|| "unknown ctx".to_string());
    let thinking = if row.thinking_levels.is_empty() {
        "thinking none".to_string()
    } else {
        format!("thinking {}", row.thinking_levels.join("/"))
    };
    format!(
        "{state}; upstream {}; {context}; out {}; {thinking}",
        row.upstream_model, row.output
    )
}

impl ChatWidget {
    /// Dispatch a bare slash command and record its staged local-history entry.
    ///
    /// The composer stages history before returning `InputResult::Command`; this wrapper commits
    /// that staged entry after dispatch so slash-command recall follows the same "submitted input"
    /// rule as normal text.
    pub(super) fn handle_slash_command_dispatch(&mut self, cmd: SlashCommand) {
        self.dispatch_command(cmd);
        if cmd == SlashCommand::Goal {
            self.bottom_pane.drain_pending_submission_state();
        }
        self.bottom_pane.record_pending_slash_command_history();
    }

    pub(super) fn handle_service_tier_command_dispatch(&mut self, command: ServiceTierCommand) {
        if self.active_side_conversation {
            self.add_error_message(format!(
                "'/{}' is unavailable in side conversations. {SIDE_SLASH_COMMAND_UNAVAILABLE_HINT}",
                command.name
            ));
            self.bottom_pane.drain_pending_submission_state();
            self.bottom_pane.record_pending_slash_command_history();
            return;
        }
        self.toggle_service_tier_from_ui(command);
        self.bottom_pane.record_pending_slash_command_history();
    }

    /// Dispatch an inline slash command and record its staged local-history entry.
    ///
    /// Inline command arguments may later be prepared through the normal submission pipeline, but
    /// local command recall still tracks the original command invocation. Treating this wrapper as
    /// the only input-result entry point avoids double-recording commands with inline args.
    pub(super) fn handle_slash_command_with_args_dispatch(
        &mut self,
        cmd: SlashCommand,
        args: String,
        text_elements: Vec<TextElement>,
    ) {
        self.dispatch_command_with_args(cmd, args, text_elements);
        self.bottom_pane.record_pending_slash_command_history();
    }

    fn start_provider_list_command(&mut self) {
        self.add_info_message("Running /provider list".to_string(), /*hint*/ None);
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let output = run_codexos_local_command(codexos_admin_command(vec![
                "provider".to_string(),
                "list".to_string(),
            ]))
            .await;
            tx.send(AppEvent::CodexOsProviderListResult(output));
        });
    }

    pub(crate) fn start_provider_remove_flow(&mut self, provider_id: Option<String>) {
        self.add_info_message(
            "Loading providers and models".to_string(),
            Some(
                "Choose a provider, then remove selected models or the entire provider."
                    .to_string(),
            ),
        );
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = load_codexos_provider_remove_snapshot().await;
            tx.send(AppEvent::CodexOsProviderRemoveSnapshotLoaded {
                provider_id,
                result,
            });
        });
    }

    fn show_provider_help(&mut self) {
        let lines = PROVIDER_HELP
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
        self.add_plain_history_lines(lines);
    }

    fn handle_provider_command_args(&mut self, args: String) {
        let args = args.trim();
        if args.is_empty() {
            self.open_codexos_provider_url_prompt(String::new());
            return;
        }

        let Some(mut parts) = shlex::split(args) else {
            self.add_error_message(format!(
                "Could not parse /provider arguments. Check quote escaping.\n\n{PROVIDER_HELP}"
            ));
            return;
        };
        if parts.is_empty() {
            self.open_codexos_provider_url_prompt(String::new());
            return;
        }

        let subcommand = parts.remove(0).to_ascii_lowercase();
        match subcommand.as_str() {
            "add" => {
                if parts.len() > 1 {
                    self.add_error_message(PROVIDER_HELP.to_string());
                    return;
                }
                self.open_codexos_provider_url_prompt(parts.pop().unwrap_or_default());
            }
            "list" => {
                if !parts.is_empty() {
                    self.add_error_message(PROVIDER_HELP.to_string());
                    return;
                }
                self.start_provider_list_command();
            }
            "remove" => {
                if parts.len() > 1 {
                    self.add_error_message(PROVIDER_HELP.to_string());
                    return;
                }
                self.start_provider_remove_flow(parts.pop());
            }
            "help" | "--help" | "-h" => {
                self.show_provider_help();
            }
            _ if args.contains("://") && parts.is_empty() => {
                self.open_codexos_provider_url_prompt(args.to_string());
            }
            other => {
                self.add_error_message(format!(
                    "Unknown /provider subcommand '{other}'.\n\n{PROVIDER_HELP}"
                ));
            }
        }
    }

    fn show_agentcheck_help(&mut self) {
        let enabled = read_agentcheck_enabled();
        let path = agentcheck_state_path();
        let lines = vec![
            Line::from(format!(
                "/agentcheck is {}",
                agentcheck_state_label(enabled)
            )),
            Line::from(""),
            Line::from(
                "Agent Check is a Retrace proxy check that reviews the assistant draft before it is returned.",
            ),
            Line::from(
                "When it decides the draft is only a progress update or did not finish the latest request, it can retry internally before returning the final answer.",
            ),
            Line::from(
                "It is off by default, runs only on normal text answers with no tool calls, and no longer prints Agent Check retry banners unless CODEXOS_AGENT_CHECK_SHOW_NOTES=1 is set.",
            ),
            Line::from(""),
            Line::from(AGENTCHECK_USAGE),
            Line::from(format!("State file: {}", path.to_string_lossy())),
        ];
        self.add_plain_history_lines(lines);
    }

    fn set_agentcheck_enabled_from_command(&mut self, enabled: bool) {
        match write_agentcheck_enabled(enabled) {
            Ok(()) => self.show_agentcheck_help(),
            Err(message) => self.add_error_message(message),
        }
    }

    fn handle_agentcheck_command_args(&mut self, args: String) {
        let args = args.trim();
        if args.is_empty() {
            self.set_agentcheck_enabled_from_command(!read_agentcheck_enabled());
            return;
        }

        let Some(parts) = shlex::split(args) else {
            self.add_error_message(format!(
                "Could not parse /agentcheck arguments. Check quote escaping.\n\n{AGENTCHECK_USAGE}"
            ));
            return;
        };
        if parts.len() > 1 {
            self.add_error_message(AGENTCHECK_USAGE.to_string());
            return;
        }

        match parts
            .first()
            .map(|part| part.to_ascii_lowercase())
            .as_deref()
        {
            Some("on" | "enable" | "enabled" | "true" | "1" | "yes") => {
                self.set_agentcheck_enabled_from_command(true);
            }
            Some("off" | "disable" | "disabled" | "false" | "0" | "no") => {
                self.set_agentcheck_enabled_from_command(false);
            }
            Some("status" | "help" | "--help" | "-h") => {
                self.show_agentcheck_help();
            }
            Some(other) => {
                self.add_error_message(format!(
                    "Unknown /agentcheck option '{other}'.\n\n{AGENTCHECK_USAGE}"
                ));
            }
            None => {
                self.show_agentcheck_help();
            }
        }
    }

    pub(crate) fn open_codexos_provider_url_prompt(&mut self, initial_text: String) {
        let tx = self.app_event_tx.clone();
        let view = CustomPromptView::new(
            "Connect provider".to_string(),
            "Paste provider API URL, for example https://host/v1".to_string(),
            initial_text,
            Some("Step 1 of 2: API base URL".to_string()),
            Box::new(move |base_url: String| {
                tx.send(AppEvent::CodexOsProviderUrlSubmitted { base_url });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn open_codexos_provider_key_prompt(&mut self, base_url: String) {
        let base_url = base_url.trim().to_string();
        if Url::parse(&base_url).is_err() {
            self.add_error_message(
                "Provider URL must be a full URL, for example https://host/v1".to_string(),
            );
            return;
        }

        let provider_id = codexos_provider_id_from_url(&base_url);
        let tx = self.app_event_tx.clone();
        let context = format!("Provider: {provider_id}  URL: {base_url}");
        let view = CustomPromptView::new_secret(
            "Provider API key".to_string(),
            "Paste API key and press Enter".to_string(),
            String::new(),
            Some(format!("Step 2 of 2: API key  {context}")),
            Box::new(move |api_key: String| {
                let tx = tx.clone();
                let provider_id = provider_id.clone();
                let base_url = base_url.clone();
                tx.send(AppEvent::CodexOsProviderProbeStarted {
                    provider_id: provider_id.clone(),
                });
                tokio::spawn(async move {
                    let result =
                        connect_codexos_provider_with_key(provider_id.clone(), base_url, api_key)
                            .await;
                    tx.send(AppEvent::CodexOsProviderConnected {
                        provider_id,
                        result,
                    });
                });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn on_codexos_provider_probe_started(&mut self, provider_id: String) {
        self.add_info_message(
            format!("Listing models for provider '{provider_id}'"),
            Some("Normalizing the URL, validating the API key, and fetching the model list. This can take a while.".to_string()),
        );
        self.begin_probe_status(format!("Connecting to {provider_id}"));
    }

    pub(crate) fn on_codexos_provider_connected(
        &mut self,
        provider_id: String,
        result: Result<Vec<CodexOsProviderModelRow>, String>,
    ) {
        // The "Connecting…" probe spinner started in `on_codexos_provider_probe_started`
        // is done now; clear it before showing the model picker or any message.
        self.clear_model_probe_spinner();
        let rows = match result {
            Ok(rows) => rows,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };

        if rows.is_empty() {
            self.add_info_message(
                format!("Provider '{provider_id}' connected, but no models were returned."),
                Some("Check the provider endpoint, then run /provider add again.".to_string()),
            );
            return;
        }

        let items: Vec<MultiSelectItem> = rows
            .iter()
            .map(|row| MultiSelectItem {
                id: row.id.clone(),
                name: row.id.clone(),
                description: Some(codexos_model_description(row)),
                enabled: row.enabled,
                orderable: false,
                section_break_after: false,
            })
            .collect();
        let provider_for_event = provider_id.clone();
        let rows_for_event = rows.clone();
        let list_keymap = crate::keymap::RuntimeKeymap::from_config(&self.config.tui_keymap)
            .map(|keymap| keymap.list)
            .unwrap_or_else(|_| crate::keymap::RuntimeKeymap::defaults().list);
        let picker = MultiSelectPicker::builder(
            "Select models".to_string(),
            Some(format!(
                "{provider_id}: {} discovered. Enter selects models to probe and add to /model.",
                rows.len()
            )),
            self.app_event_tx.clone(),
        )
        .items(items)
        .list_keymap(list_keymap)
        .confirm_label(|selected| match selected {
            0 => "Add selected models (nothing selected yet)".to_string(),
            1 => "Add 1 selected model".to_string(),
            n => format!("Add {n} selected models"),
        })
        .cancel_label("Discard and close")
        .on_preview(|items| {
            let selected: Vec<&str> = items
                .iter()
                .filter(|item| item.enabled)
                .map(|item| item.name.as_str())
                .collect();
            if selected.is_empty() {
                return None;
            }
            let shown = selected
                .iter()
                .take(3)
                .copied()
                .collect::<Vec<_>>()
                .join(" · ");
            let more = selected.len().saturating_sub(3);
            let text = if more > 0 {
                format!("Selected: {shown} · +{more} more")
            } else {
                format!("Selected: {shown}")
            };
            Some(Line::from(text))
        })
        .on_confirm(move |ids, tx| {
            tx.send(AppEvent::CodexOsProviderModelsSelected {
                provider_id: provider_for_event.clone(),
                rows: rows_for_event.clone(),
                model_ids: ids.to_vec(),
            });
        })
        .build();
        self.bottom_pane.show_view(Box::new(picker));
        self.request_redraw();
    }

    pub(crate) fn on_codexos_provider_models_selected(
        &mut self,
        provider_id: String,
        _rows: Vec<CodexOsProviderModelRow>,
        model_ids: Vec<String>,
    ) {
        if model_ids.is_empty() {
            self.add_info_message(
                "No provider models selected.".to_string(),
                Some("Run /provider add again to select models.".to_string()),
            );
            return;
        }

        self.add_info_message(
            format!(
                "Probing and adding {} model(s) from {provider_id}",
                model_ids.len()
            ),
            Some("This runs selected-model capability probes. It can take a while.".to_string()),
        );
        self.begin_probe_status(format!(
            "Probing {} model(s) from {provider_id}",
            model_ids.len()
        ));
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result =
                configure_codexos_provider_models(provider_id, model_ids, tx.clone()).await;
            tx.send(AppEvent::CodexOsProviderConfigured {
                result,
                success_message: Some("Selected provider models were added to /model.".to_string()),
            });
        });
    }

    pub(crate) fn on_codexos_provider_remove_snapshot_loaded(
        &mut self,
        provider_id: Option<String>,
        result: Result<CodexOsProviderRemoveSnapshot, String>,
    ) {
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        if snapshot.providers.is_empty() {
            self.add_info_message(
                "No providers are configured.".to_string(),
                Some("Run /provider add to connect one.".to_string()),
            );
            return;
        }
        if let Some(provider_id) = provider_id {
            if snapshot
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
            {
                self.open_codexos_provider_remove_action_menu(snapshot, provider_id);
            } else {
                self.add_error_message(format!("Provider not found: {provider_id}"));
            }
            return;
        }
        if snapshot.providers.len() == 1 {
            let provider_id = snapshot.providers[0].id.clone();
            self.open_codexos_provider_remove_action_menu(snapshot, provider_id);
            return;
        }
        self.open_codexos_provider_remove_provider_picker(snapshot);
    }

    pub(crate) fn open_codexos_provider_remove_provider_picker(
        &mut self,
        snapshot: CodexOsProviderRemoveSnapshot,
    ) {
        let items: Vec<SelectionItem> = snapshot
            .providers
            .iter()
            .map(|provider| {
                let provider_id = provider.id.clone();
                let provider_for_count = provider.id.clone();
                let model_count = snapshot
                    .models
                    .iter()
                    .filter(|model| model.provider == provider_for_count)
                    .count();
                let snapshot_for_action = snapshot.clone();
                SelectionItem {
                    name: provider.id.clone(),
                    description: Some(format!(
                        "{}  models={}  key={}",
                        provider.base_url, model_count, provider.key
                    )),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::CodexOsProviderRemoveProviderSelected {
                            snapshot: snapshot_for_action.clone(),
                            provider_id: provider_id.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Remove provider".to_string()),
            subtitle: Some("Choose the provider to modify.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_codexos_provider_remove_action_menu(
        &mut self,
        snapshot: CodexOsProviderRemoveSnapshot,
        provider_id: String,
    ) {
        let Some(provider) = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
        else {
            self.add_error_message(format!("Provider not found: {provider_id}"));
            return;
        };
        let model_count = snapshot
            .models
            .iter()
            .filter(|model| model.provider == provider_id)
            .count();
        let mut items = Vec::new();
        if model_count > 0 {
            let snapshot_for_action = snapshot.clone();
            let provider_for_action = provider_id.clone();
            items.push(SelectionItem {
                name: "Remove selected models".to_string(),
                description: Some(format!(
                    "Choose one or more of {model_count} model entries from this provider"
                )),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::CodexOsProviderRemoveModelsRequested {
                        snapshot: snapshot_for_action.clone(),
                        provider_id: provider_for_action.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        let provider_for_action = provider_id.clone();
        items.push(SelectionItem {
            name: "Remove entire provider".to_string(),
            description: Some(format!(
                "Delete provider config and all {model_count} model entries"
            )),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::CodexOsProviderRemoveEntire {
                    provider_id: provider_for_action.clone(),
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        items.push(SelectionItem {
            name: "Cancel".to_string(),
            description: Some("Leave provider settings unchanged".to_string()),
            dismiss_on_select: true,
            ..Default::default()
        });

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(format!("Remove {}", provider.id)),
            subtitle: Some(format!("{}  {}", provider.name, provider.base_url)),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_codexos_provider_remove_models_picker(
        &mut self,
        snapshot: CodexOsProviderRemoveSnapshot,
        provider_id: String,
    ) {
        let rows: Vec<CodexOsProviderModelRow> = snapshot
            .models
            .into_iter()
            .filter(|row| row.provider == provider_id)
            .collect();
        if rows.is_empty() {
            self.add_info_message(
                format!("Provider '{provider_id}' has no model entries to remove."),
                /*hint*/ None,
            );
            return;
        }
        let items: Vec<MultiSelectItem> = rows
            .iter()
            .map(|row| MultiSelectItem {
                id: row.id.clone(),
                name: row.id.clone(),
                description: Some(codexos_model_registry_description(row)),
                enabled: false,
                orderable: false,
                section_break_after: false,
            })
            .collect();
        let provider_for_event = provider_id.clone();
        let list_keymap = crate::keymap::RuntimeKeymap::from_config(&self.config.tui_keymap)
            .map(|keymap| keymap.list)
            .unwrap_or_else(|_| crate::keymap::RuntimeKeymap::defaults().list);
        let picker = MultiSelectPicker::builder(
            "Remove provider models".to_string(),
            Some(format!("{provider_id}: Enter selects models to remove.")),
            self.app_event_tx.clone(),
        )
        .items(items)
        .list_keymap(list_keymap)
        .confirm_label(|selected| match selected {
            0 => "Remove selected models (nothing selected yet)".to_string(),
            1 => "Remove 1 selected model".to_string(),
            n => format!("Remove {n} selected models"),
        })
        .cancel_label("Discard and close")
        .on_preview(|items| {
            let selected = items.iter().filter(|item| item.enabled).count();
            Some(Line::from(format!("{selected} selected for removal")))
        })
        .on_confirm(move |ids, tx| {
            tx.send(AppEvent::CodexOsProviderRemoveModelsSelected {
                provider_id: provider_for_event.clone(),
                model_ids: ids.to_vec(),
            });
        })
        .build();
        self.bottom_pane.show_view(Box::new(picker));
        self.request_redraw();
    }

    pub(crate) fn on_codexos_provider_remove_entire(&mut self, provider_id: String) {
        self.add_info_message(
            format!("Removing provider '{provider_id}'"),
            Some("This removes the provider and its model entries from /model.".to_string()),
        );
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let success_message = format!("Provider '{provider_id}' was removed from /model.");
            let result = remove_codexos_provider(provider_id).await;
            tx.send(AppEvent::CodexOsProviderConfigured {
                result,
                success_message: Some(success_message),
            });
        });
    }

    pub(crate) fn on_codexos_provider_remove_models_selected(
        &mut self,
        provider_id: String,
        model_ids: Vec<String>,
    ) {
        if model_ids.is_empty() {
            self.add_info_message(
                "No provider models selected for removal.".to_string(),
                /*hint*/ None,
            );
            return;
        }

        self.add_info_message(
            format!("Removing {} model(s) from {provider_id}", model_ids.len()),
            Some(
                "The provider stays configured; only the selected models are removed.".to_string(),
            ),
        );
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = remove_codexos_provider_models(provider_id, model_ids).await;
            tx.send(AppEvent::CodexOsProviderConfigured {
                result,
                success_message: Some(
                    "Selected provider models were removed from /model.".to_string(),
                ),
            });
        });
    }

    pub(crate) fn on_codexos_provider_configured(
        &mut self,
        result: Result<String, String>,
        success_message: Option<String>,
    ) {
        self.clear_model_probe_spinner();
        match result {
            Ok(text) => {
                let lines: Vec<Line<'static>> = if text.trim().is_empty() {
                    vec!["Provider models configured.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.add_plain_history_lines(lines);
                let message =
                    success_message.unwrap_or_else(|| "Provider registry updated.".to_string());
                self.add_info_message(
                    message,
                    Some("Run /model to choose an available model.".to_string()),
                );
            }
            Err(message) => {
                self.add_error_message(message);
            }
        }
    }

    fn apply_plan_slash_command(&mut self) -> bool {
        if !self.collaboration_modes_enabled() {
            self.add_info_message(
                "Collaboration modes are disabled.".to_string(),
                Some("Enable collaboration modes to use /plan.".to_string()),
            );
            return false;
        }
        if let Some(mask) = collaboration_modes::plan_mask(self.model_catalog.as_ref()) {
            self.set_collaboration_mask_from_user_action(mask);
            true
        } else {
            self.add_info_message(
                "Plan mode unavailable right now.".to_string(),
                /*hint*/ None,
            );
            false
        }
    }

    fn request_side_conversation(
        &mut self,
        parent_thread_id: ThreadId,
        user_message: Option<UserMessage>,
    ) {
        self.set_side_conversation_context_label(Some(SIDE_STARTING_CONTEXT_LABEL.to_string()));
        self.request_redraw();
        self.app_event_tx.send(AppEvent::StartSide {
            parent_thread_id,
            user_message,
        });
    }

    fn request_empty_side_conversation(&mut self, cmd: SlashCommand) {
        let Some(parent_thread_id) = self.thread_id else {
            let command = cmd.command();
            self.add_error_message(format!(
                "'/{command}' is unavailable before the session starts."
            ));
            return;
        };

        self.request_side_conversation(parent_thread_id, /*user_message*/ None);
    }

    fn emit_raw_output_mode_changed(&self, enabled: bool) {
        self.app_event_tx
            .send(AppEvent::RawOutputModeChanged { enabled });
    }

    /// `/model` with inline args: `probe` opens a picker of all registry
    /// models; `probe <model>` / `reprobe [model]` re-runs the live capability
    /// probe directly. No args opens the model picker.
    pub(crate) fn handle_model_command_args(&mut self, args: String) {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            self.open_model_popup();
            return;
        }
        let mut parts = trimmed.split_whitespace();
        let subcommand = parts
            .next()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !matches!(subcommand.as_str(), "reprobe" | "probe" | "add") {
            self.add_error_message("Usage: /model [probe [model] | add]".to_string());
            return;
        }
        let model_arg = parts.next().map(str::to_string);
        if parts.next().is_some() {
            self.add_error_message("Usage: /model [probe [model] | add]".to_string());
            return;
        }
        match (subcommand.as_str(), model_arg) {
            ("add", None) => self.open_model_add_picker(),
            ("add", Some(_)) => {
                self.add_error_message(
                    "/model add takes no arguments; pick models from the list.".to_string(),
                );
            }
            // Bare `/model probe` opens the picker of everything probe-able.
            ("probe", None) => self.open_model_probe_picker(),
            (_, model_arg) => {
                let model_id = model_arg.unwrap_or_else(|| self.current_model().to_string());
                self.start_model_reprobe(model_id);
            }
        }
    }

    /// `/delete` — enumerate saved conversations, then show a picker.
    /// Unified `/delete` entry: choose what to delete — conversations, or
    /// models/providers — then route to the matching picker.
    fn open_delete_target_picker(&mut self) {
        let items = vec![
            crate::bottom_pane::SelectionItem {
                name: "Conversations".to_string(),
                description: Some("Delete a saved conversation, or all of them.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::DeleteTargetConversations);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            crate::bottom_pane::SelectionItem {
                name: "Models or a provider".to_string(),
                description: Some(
                    "Remove selected models from a provider, or remove a whole provider."
                        .to_string(),
                ),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::DeleteTargetProviders);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];
        self.bottom_pane
            .show_selection_view(crate::bottom_pane::SelectionViewParams {
                title: Some("Delete".to_string()),
                subtitle: Some("What do you want to delete?".to_string()),
                footer_hint: Some(standard_popup_hint_line()),
                items,
                ..Default::default()
            });
    }

    pub(crate) fn open_delete_conversations_picker(&mut self) {
        let codex_home = self.config.codex_home.as_path().to_path_buf();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let rows = list_saved_conversations(codex_home).await;
            tx.send(AppEvent::DeleteConversationsListLoaded { rows });
        });
    }

    pub(crate) fn on_delete_conversations_list_loaded(
        &mut self,
        rows: Vec<crate::app_event::DeleteConversationRow>,
    ) {
        if rows.is_empty() {
            self.add_info_message("No saved conversations to delete.".to_string(), None);
            return;
        }
        let count = rows.len();
        let mut items: Vec<crate::bottom_pane::SelectionItem> = Vec::new();
        // Top: delete everything.
        items.push(crate::bottom_pane::SelectionItem {
            name: format!("all — delete all {count} conversations"),
            description: Some("Permanently removes every saved conversation.".to_string()),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::DeleteConversations {
                    target: crate::app_event::DeleteConversationTarget::All,
                    confirmed: false,
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        for row in rows {
            let path = row.path.clone();
            items.push(crate::bottom_pane::SelectionItem {
                name: row.label,
                description: None,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::DeleteConversations {
                        target: crate::app_event::DeleteConversationTarget::One(path.clone()),
                        confirmed: false,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        self.bottom_pane
            .show_selection_view(crate::bottom_pane::SelectionViewParams {
                title: Some("Delete a conversation".to_string()),
                subtitle: Some("Pick one to delete, or \"all\".".to_string()),
                footer_hint: Some(standard_popup_hint_line()),
                items,
                ..Default::default()
            });
    }

    pub(crate) fn on_delete_conversations(
        &mut self,
        target: crate::app_event::DeleteConversationTarget,
        confirmed: bool,
    ) {
        use crate::app_event::DeleteConversationTarget;
        if !confirmed {
            let what = match &target {
                DeleteConversationTarget::All => "ALL saved conversations".to_string(),
                DeleteConversationTarget::One(path) => format!(
                    "this conversation ({})",
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("conversation")
                ),
            };
            let confirm_target = target.clone();
            self.bottom_pane
                .show_selection_view(crate::bottom_pane::SelectionViewParams {
                    title: Some("Confirm delete".to_string()),
                    subtitle: Some(format!("Permanently delete {what}? This cannot be undone.")),
                    footer_hint: Some(standard_popup_hint_line()),
                    items: vec![
                        crate::bottom_pane::SelectionItem {
                            name: "Yes, delete".to_string(),
                            description: None,
                            actions: vec![Box::new(move |tx| {
                                tx.send(AppEvent::DeleteConversations {
                                    target: confirm_target.clone(),
                                    confirmed: true,
                                });
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        crate::bottom_pane::SelectionItem {
                            name: "Cancel".to_string(),
                            description: None,
                            actions: vec![],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
            return;
        }
        // Confirmed — perform the deletion.
        let codex_home = self.config.codex_home.as_path().to_path_buf();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let removed = delete_conversations(codex_home, target).await;
            tx.send(AppEvent::DeleteConversationsDone { removed });
        });
    }

    pub(crate) fn on_delete_conversations_done(&mut self, removed: Result<usize, String>) {
        match removed {
            Ok(n) => self.add_info_message(
                format!("Deleted {n} conversation{}.", if n == 1 { "" } else { "s" }),
                None,
            ),
            Err(message) => self.add_error_message(message),
        }
    }

    /// Loads all models for the `/model add` multi-select: re-fetches each
    /// provider's current catalog first (so newly-available upstream models
    /// show up), then lists everything with enabled models preselected.
    pub(crate) fn open_model_add_picker(&mut self) {
        self.add_info_message(
            "Refreshing models from your providers…".to_string(),
            /*hint*/ None,
        );
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = refresh_and_list_all_models().await;
            tx.send(AppEvent::CodexOsModelAddListLoaded { result });
        });
    }

    pub(crate) fn on_codexos_model_add_list_loaded(
        &mut self,
        result: Result<Vec<CodexOsProviderModelRow>, String>,
    ) {
        let rows = match result {
            Ok(rows) => rows,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        if rows.is_empty() {
            self.add_info_message(
                "No models are registered yet — connect a provider to add one.".to_string(),
                Some("Enter its API base URL, then its API key.".to_string()),
            );
            self.open_codexos_provider_url_prompt(String::new());
            return;
        }
        // First entry: connect a brand-new provider without leaving /model add.
        let mut items: Vec<MultiSelectItem> = vec![MultiSelectItem {
            id: ADD_CUSTOM_MODEL_SENTINEL.to_string(),
            name: "➕ Add a custom model (connect a new provider)".to_string(),
            description: Some(
                "Press Enter to connect a new provider: enter its URL and API key, then pick models".to_string(),
            ),
            enabled: false,
            orderable: false,
            section_break_after: true,
        }];
        items.extend(rows.iter().map(|row| MultiSelectItem {
            id: row.id.clone(),
            name: row.id.clone(),
            description: Some(format!(
                "{}; {}",
                row.provider,
                codexos_model_registry_description(row)
            )),
            enabled: row.enabled,
            orderable: false,
            section_break_after: false,
        }));
        let rows_for_event = rows.clone();
        let list_keymap = crate::keymap::RuntimeKeymap::from_config(&self.config.tui_keymap)
            .map(|keymap| keymap.list)
            .unwrap_or_else(|_| crate::keymap::RuntimeKeymap::defaults().list);
        let picker = MultiSelectPicker::builder(
            "Add models".to_string(),
            Some(format!(
                "{} model(s) across all providers. Enter toggles; checked models appear in /model.",
                rows.len()
            )),
            self.app_event_tx.clone(),
        )
        .items(items)
        .list_keymap(list_keymap)
        .default_focus_confirm()
        .confirm_label(|enabled| format!("Apply selection ({enabled} enabled)"))
        .cancel_label("Discard changes")
        .on_change(|items, tx| {
            // The "add a custom model" sentinel acts the moment it is checked:
            // the provider-connect prompt replaces this picker.
            if items
                .iter()
                .any(|item| item.id == ADD_CUSTOM_MODEL_SENTINEL && item.enabled)
            {
                tx.send(AppEvent::CodexOsOpenProviderPrompt);
            }
        })
        .on_preview(|items| {
            let selected = items
                .iter()
                .filter(|item| item.enabled && item.id != ADD_CUSTOM_MODEL_SENTINEL)
                .count();
            Some(Line::from(format!("{selected} enabled")))
        })
        .on_confirm(move |ids, tx| {
            // The sentinel opens the provider prompt from on_change; strip it
            // here so it can never hijack or miscount an apply.
            let model_ids: Vec<String> = ids
                .iter()
                .filter(|id| id.as_str() != ADD_CUSTOM_MODEL_SENTINEL)
                .cloned()
                .collect();
            tx.send(AppEvent::CodexOsModelAddSelectionConfirmed {
                rows: rows_for_event.clone(),
                model_ids,
            });
        })
        .build();
        self.bottom_pane.show_view(Box::new(picker));
        self.request_redraw();
    }

    pub(crate) fn on_codexos_model_add_selection_confirmed(
        &mut self,
        rows: Vec<CodexOsProviderModelRow>,
        model_ids: Vec<String>,
    ) {
        self.add_info_message(
            "Applying model selection\u{2026}".to_string(),
            Some("Newly added models are probed live before they are enabled.".to_string()),
        );
        self.begin_probe_status("Probing model capabilities".to_string());
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = apply_model_add_selection(rows, model_ids, tx.clone()).await;
            tx.send(AppEvent::CodexOsProviderConfigured {
                result,
                success_message: Some("Model list updated; run /model to switch.".to_string()),
            });
        });
    }

    /// Loads all registry models and shows the `/model probe` selection popup.
    fn open_model_probe_picker(&mut self) {
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = list_codexos_all_models().await;
            tx.send(AppEvent::CodexOsModelProbeListLoaded { result });
        });
    }

    pub(crate) fn on_codexos_model_probe_list_loaded(
        &mut self,
        result: Result<Vec<CodexOsProviderModelRow>, String>,
    ) {
        let rows = match result {
            Ok(rows) => rows,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        if rows.is_empty() {
            self.add_info_message(
                "No models are registered yet.".to_string(),
                Some("Run /provider to connect a provider first.".to_string()),
            );
            return;
        }
        let current_model = self.current_model().to_string();
        let items: Vec<crate::bottom_pane::SelectionItem> = rows
            .into_iter()
            .map(|row| {
                let description = Some(format!(
                    "{}; context {}; thinking {}; last method {}",
                    row.provider,
                    row.context
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    if row.thinking_levels.is_empty() {
                        "unprobed".to_string()
                    } else {
                        row.thinking_levels.join("/")
                    },
                    row.thinking_method.unwrap_or_else(|| "none".to_string()),
                ));
                let model_id = row.id.clone();
                let actions: Vec<crate::bottom_pane::SelectionAction> = vec![Box::new(move |tx| {
                    tx.send(AppEvent::CodexOsReprobeModel {
                        model_id: model_id.clone(),
                    });
                })];
                crate::bottom_pane::SelectionItem {
                    name: row.id.clone(),
                    description,
                    is_current: row.id == current_model,
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        self.bottom_pane
            .show_selection_view(crate::bottom_pane::SelectionViewParams {
                title: Some("Probe a model".to_string()),
                subtitle: Some(
                    "Live-detects thinking formats, effort levels, context window, cache, and streaming.".to_string(),
                ),
                footer_hint: Some(standard_popup_hint_line()),
                items,
                ..Default::default()
            });
    }

    /// Kicks off a live capability re-probe with a busy indicator until done.
    pub(crate) fn start_model_reprobe(&mut self, model_id: String) {
        self.add_info_message(
            format!("Probing {model_id}…"),
            Some(
                "This sends live requests to detect thinking formats, effort levels, context window, cache, and streaming. It can take a minute."
                    .to_string(),
            ),
        );
        self.begin_probe_status(format!("Probing {model_id}"));
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let success_message =
                format!("{model_id} probed; /model reflects the new capabilities.");
            let result = reprobe_codexos_model(model_id, tx.clone()).await;
            tx.send(AppEvent::CodexOsProviderConfigured {
                result,
                success_message: Some(success_message),
            });
        });
    }

    /// Clears the `/model probe` busy indicator once the probe completes.
    pub(crate) fn clear_model_probe_spinner(&mut self) {
        if self.model_probe_spinner_active {
            self.model_probe_spinner_active = false;
            // `bottom_pane.is_task_running()` reflects the spinner we set, so
            // consult the real turn/queue state before releasing it.
            if !self.turn_lifecycle.agent_turn_running && !self.input_queue.user_turn_pending_start
            {
                self.bottom_pane.set_task_running(/*running*/ false);
            }
        }
    }

    /// Path of the state file that persists `/thinking` across sessions.
    fn thinking_display_state_path(codex_home: &std::path::Path) -> std::path::PathBuf {
        codex_home.join("thinking_display")
    }

    /// Loads the persisted `/thinking` selection, if any.
    pub(crate) fn load_thinking_display_override(codex_home: &std::path::Path) -> Option<bool> {
        match std::fs::read_to_string(Self::thinking_display_state_path(codex_home))
            .ok()?
            .trim()
        {
            "show" => Some(true),
            "hide" => Some(false),
            _ => None,
        }
    }

    fn persist_thinking_display(&self, value: &str) {
        let path = Self::thinking_display_state_path(&self.config.codex_home);
        if let Err(err) = std::fs::write(&path, value) {
            tracing::warn!(error = %err, "failed to persist /thinking state to {}", path.display());
        }
    }

    /// `/thinking show|hide|auto`: controls whether the model's thinking block
    /// is rendered in the main transcript. `hide` still records it in the
    /// Ctrl-T transcript overlay; `auto` restores the header-based default.
    /// The selection persists across sessions via a state file in CODEX_HOME.
    pub(crate) fn handle_thinking_command_args(&mut self, args: String) {
        match args.trim().to_ascii_lowercase().as_str() {
            "show" | "on" => {
                self.thinking_display_override = Some(true);
                self.persist_thinking_display("show");
                self.add_info_message(
                    "Thinking blocks will be shown in the transcript.".to_string(),
                    /*hint*/ None,
                );
            }
            "hide" | "off" => {
                self.thinking_display_override = Some(false);
                self.persist_thinking_display("hide");
                self.add_info_message(
                    "Thinking blocks are hidden.".to_string(),
                    Some(
                        "They are still recorded; press Ctrl-T to view the full transcript."
                            .to_string(),
                    ),
                );
            }
            "auto" | "default" => {
                self.thinking_display_override = None;
                self.persist_thinking_display("auto");
                self.add_info_message(
                    "Thinking display restored to the default heuristic.".to_string(),
                    /*hint*/ None,
                );
            }
            "" => {
                let current = match self.thinking_display_override {
                    Some(true) => "show",
                    Some(false) => "hide",
                    None => "auto",
                };
                self.add_info_message(
                    format!("Thinking display is currently `{current}`."),
                    Some("Usage: /thinking show|hide|auto".to_string()),
                );
            }
            _ => {
                self.add_error_message("Usage: /thinking show|hide|auto".to_string());
            }
        }
    }

    pub(super) fn dispatch_command(&mut self, cmd: SlashCommand) {
        if !self.ensure_slash_command_allowed_in_side_conversation(cmd) {
            return;
        }
        if !self.ensure_side_command_allowed_outside_review(cmd) {
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.bottom_pane.drain_pending_submission_state();
            self.request_redraw();
            return;
        }

        match cmd {
            SlashCommand::Feedback => {
                if !self.config.feedback_enabled {
                    let params = crate::bottom_pane::feedback_disabled_params();
                    self.bottom_pane.show_selection_view(params);
                    self.request_redraw();
                    return;
                }
                // Step 1: pick a category (UI built in feedback_view)
                let params =
                    crate::bottom_pane::feedback_selection_params(self.app_event_tx.clone());
                self.bottom_pane.show_selection_view(params);
                self.request_redraw();
            }
            SlashCommand::New => {
                self.app_event_tx.send(AppEvent::NewSession);
            }
            SlashCommand::Archive => {
                self.bottom_pane.show_selection_view(SelectionViewParams {
                    title: Some("Archive this session?".to_string()),
                    subtitle: Some(
                        "Are you sure? This will archive the current session and exit Retrace"
                            .to_string(),
                    ),
                    footer_hint: Some(standard_popup_hint_line()),
                    items: vec![
                        SelectionItem {
                            name: "No, don't archive".to_string(),
                            description: Some("Return to the current session".to_string()),
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: "Yes, archive and exit".to_string(),
                            description: Some("Archive this session now".to_string()),
                            actions: vec![Box::new(|tx| {
                                tx.send(AppEvent::ArchiveCurrentThread);
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
                self.request_redraw();
            }
            SlashCommand::Clear => {
                self.app_event_tx.send(AppEvent::ClearUi);
            }
            SlashCommand::Resume => {
                self.app_event_tx.send(AppEvent::OpenResumePicker);
            }
            SlashCommand::Delete => {
                self.open_delete_target_picker();
            }
            SlashCommand::Fork => {
                self.app_event_tx.send(AppEvent::ForkCurrentSession);
            }
            SlashCommand::App => {
                let Some(thread_id) = self.thread_id else {
                    self.add_error_message(
                        "Session is still starting; try /app again in a moment.".to_string(),
                    );
                    return;
                };
                self.app_event_tx
                    .send(AppEvent::OpenDesktopThread { thread_id });
            }
            SlashCommand::Init => {
                let init_target = self.config.cwd.join(DEFAULT_AGENTS_MD_FILENAME);
                if init_target.exists() {
                    let message = format!(
                        "{DEFAULT_AGENTS_MD_FILENAME} already exists here. Skipping /init to avoid overwriting it."
                    );
                    self.add_info_message(message, /*hint*/ None);
                    return;
                }
                const INIT_PROMPT: &str = include_str!("../../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                self.clear_token_usage();
                if !self.bottom_pane.is_task_running() {
                    self.bottom_pane.set_task_running(/*running*/ true);
                }
                self.app_event_tx.compact();
            }
            SlashCommand::Review => {
                self.open_review_popup();
            }
            SlashCommand::Rename => {
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                self.show_rename_prompt();
            }
            SlashCommand::Model => {
                self.open_model_popup();
            }
            SlashCommand::ModelAdd => {
                self.handle_model_command_args("add".to_string());
            }
            SlashCommand::Provider => {
                self.open_codexos_provider_url_prompt(String::new());
            }
            SlashCommand::Thinking => {
                self.handle_thinking_command_args(String::new());
            }
            SlashCommand::AgentCheck => {
                self.handle_agentcheck_command_args(String::new());
            }
            SlashCommand::Realtime => {
                if !self.realtime_conversation_enabled() {
                    return;
                }
                if self.realtime_conversation.is_live() {
                    self.stop_realtime_conversation_from_ui();
                } else {
                    self.start_realtime_conversation();
                }
            }
            SlashCommand::Settings => {
                if !self.realtime_audio_device_selection_enabled() {
                    return;
                }
                self.open_realtime_audio_popup();
            }
            SlashCommand::Personality => {
                self.open_personality_popup();
            }
            SlashCommand::Plan => {
                self.apply_plan_slash_command();
            }
            SlashCommand::Goal => {
                if !self.config.features.enabled(Feature::Goals) {
                    return;
                }
                if let Some(thread_id) = self.thread_id {
                    self.app_event_tx
                        .send(AppEvent::OpenThreadGoalMenu { thread_id });
                    self.append_message_history_entry("/goal".to_string());
                } else {
                    self.add_info_message(
                        GOAL_USAGE.to_string(),
                        Some(GOAL_USAGE_HINT.to_string()),
                    );
                }
            }
            SlashCommand::Loop => {
                self.handle_loop_command_args(String::new());
            }
            SlashCommand::RalphLoop => {
                self.handle_ralphloop_command_args(String::new());
            }
            SlashCommand::Side | SlashCommand::Btw => {
                self.request_empty_side_conversation(cmd);
            }
            SlashCommand::Agent | SlashCommand::MultiAgents => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::Permissions => {
                self.open_permissions_popup();
            }
            SlashCommand::Vim => {
                self.toggle_vim_mode_and_notify();
            }
            SlashCommand::Keymap => {
                self.open_keymap_picker();
            }
            SlashCommand::ElevateSandbox => {
                #[cfg(target_os = "windows")]
                {
                    let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
                    let windows_degraded_sandbox_enabled =
                        matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
                    if !windows_degraded_sandbox_enabled
                        || !crate::legacy_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    {
                        // This command should not be visible/recognized outside degraded mode,
                        // but guard anyway in case something dispatches it directly.
                        return;
                    }

                    let Some(preset) = builtin_approval_presets()
                        .into_iter()
                        .find(|preset| preset.id == "auto")
                    else {
                        // Avoid panicking in interactive UI; treat this as a recoverable
                        // internal error.
                        self.add_error_message(
                            "Internal error: missing the 'auto' approval preset.".to_string(),
                        );
                        return;
                    };

                    if let Err(err) = self
                        .config
                        .permissions
                        .approval_policy
                        .can_set(&preset.approval)
                    {
                        self.add_error_message(err.to_string());
                        return;
                    }

                    self.session_telemetry.counter(
                        "codex.windows_sandbox.setup_elevated_sandbox_command",
                        /*inc*/ 1,
                        &[],
                    );
                    self.app_event_tx
                        .send(AppEvent::BeginWindowsSandboxElevatedSetup {
                            preset,
                            profile_selection: None,
                        });
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = &self.session_telemetry;
                    // Not supported; on non-Windows this command should never be reachable.
                }
            }
            SlashCommand::SandboxReadRoot => {
                self.add_error_message(
                    "Usage: /sandbox-add-read-dir <absolute-directory-path>".to_string(),
                );
            }
            SlashCommand::Experimental => {
                self.open_experimental_popup();
            }
            SlashCommand::AutoReview => {
                self.open_auto_review_denials_popup();
            }
            SlashCommand::Memories => {
                self.open_memories_popup();
            }
            SlashCommand::Quit | SlashCommand::Exit => {
                self.request_quit_without_confirmation();
            }
            SlashCommand::Logout => {
                self.app_event_tx.send(AppEvent::Logout);
            }
            SlashCommand::Copy => {
                self.copy_last_agent_markdown();
            }
            SlashCommand::Raw => {
                let enabled = self.toggle_raw_output_mode_and_notify();
                self.emit_raw_output_mode_changed(enabled);
            }
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let runner = self.workspace_command_runner.clone();
                let cwd = self
                    .current_cwd
                    .clone()
                    .unwrap_or_else(|| self.config.cwd.to_path_buf());
                tokio::spawn(async move {
                    let text = match runner {
                        Some(runner) => match get_git_diff(runner.as_ref(), &cwd).await {
                            Ok((is_git_repo, diff_text)) => {
                                if is_git_repo {
                                    diff_text
                                } else {
                                    "`/diff` — _not inside a git repository_".to_string()
                                }
                            }
                            Err(e) => format!("Failed to compute diff: {e}"),
                        },
                        None => "Failed to compute diff: workspace command runner unavailable"
                            .to_string(),
                    };
                    tx.send(AppEvent::DiffResult(text));
                });
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::Skills => {
                self.open_skills_menu();
            }
            SlashCommand::Hooks => {
                self.add_hooks_output();
            }
            SlashCommand::Status => {
                if self.should_prefetch_rate_limits() {
                    let request_id = self.next_status_refresh_request_id;
                    self.next_status_refresh_request_id =
                        self.next_status_refresh_request_id.wrapping_add(1);
                    self.add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
                    self.app_event_tx.send(AppEvent::RefreshRateLimits {
                        origin: RateLimitRefreshOrigin::StatusCommand { request_id },
                    });
                } else {
                    self.add_status_output(
                        /*refreshing_rate_limits*/ false, /*request_id*/ None,
                    );
                }
            }
            SlashCommand::Ide => {
                self.handle_ide_command();
            }
            SlashCommand::DebugConfig => {
                self.add_debug_config_output();
            }
            SlashCommand::Title => {
                self.open_terminal_title_setup();
            }
            SlashCommand::Statusline => {
                self.open_status_line_setup();
            }
            SlashCommand::Theme => {
                self.open_theme_picker();
            }
            SlashCommand::Pets => {
                self.open_pets_picker();
            }
            SlashCommand::Ps => {
                self.add_ps_output();
            }
            SlashCommand::Stop => {
                self.clean_background_terminals();
            }
            SlashCommand::MemoryDrop => {
                self.add_app_server_stub_message("Memory maintenance");
            }
            SlashCommand::MemoryUpdate => {
                self.add_app_server_stub_message("Memory maintenance");
            }
            SlashCommand::Mcp => {
                self.add_mcp_output(McpServerStatusDetail::ToolsAndAuthOnly);
            }
            SlashCommand::Apps => {
                self.add_connectors_output();
            }
            SlashCommand::Plugins => {
                self.add_plugins_output();
            }
            SlashCommand::Rollout => {
                if let Some(path) = self.rollout_path() {
                    self.add_info_message(
                        format!("Current rollout path: {}", path.display()),
                        /*hint*/ None,
                    );
                } else {
                    self.add_info_message(
                        "Rollout path is not available yet.".to_string(),
                        /*hint*/ None,
                    );
                }
            }
            SlashCommand::TestApproval => {
                use std::collections::HashMap;

                use crate::approval_events::ApplyPatchApprovalRequestEvent;
                use crate::diff_model::FileChange;

                self.on_apply_patch_approval_request(
                    "1".to_string(),
                    ApplyPatchApprovalRequestEvent {
                        call_id: "1".to_string(),
                        turn_id: "turn-1".to_string(),
                        changes: HashMap::from([
                            (
                                PathBuf::from("/tmp/test.txt"),
                                FileChange::Add {
                                    content: "test".to_string(),
                                },
                            ),
                            (
                                PathBuf::from("/tmp/test2.txt"),
                                FileChange::Update {
                                    unified_diff: "+test\n-test2".to_string(),
                                    move_path: None,
                                },
                            ),
                        ]),
                        reason: None,
                        grant_root: Some(PathBuf::from("/tmp")),
                    },
                );
            }
        }
    }

    /// Run an inline slash command.
    ///
    /// Branches that prepare arguments should pass `record_history: false` to the composer because
    /// the staged slash-command entry is the recall record; using the normal submission-history
    /// path as well would make a single command appear twice during Up-arrow navigation.
    pub(super) fn dispatch_command_with_args(
        &mut self,
        cmd: SlashCommand,
        args: String,
        text_elements: Vec<TextElement>,
    ) {
        if !self.ensure_slash_command_allowed_in_side_conversation(cmd) {
            return;
        }
        if !self.ensure_side_command_allowed_outside_review(cmd) {
            return;
        }
        if !cmd.supports_inline_args() {
            self.dispatch_command(cmd);
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }

        let trimmed = args.trim();
        if trimmed.is_empty() {
            self.dispatch_command(cmd);
            return;
        }

        if cmd == SlashCommand::Goal
            && !self.goal_objective_with_pending_pastes_is_allowed(&args, &text_elements)
        {
            return;
        }

        let Some((prepared_args, prepared_elements)) =
            self.prepare_live_inline_args(args, text_elements)
        else {
            return;
        };
        self.dispatch_prepared_command_with_args(
            cmd,
            PreparedSlashCommandArgs {
                args: prepared_args,
                text_elements: prepared_elements,
                local_images: Vec::new(),
                remote_image_urls: Vec::new(),
                mention_bindings: Vec::new(),
                source: SlashCommandDispatchSource::Live,
            },
        );
    }

    fn prepare_live_inline_args(
        &mut self,
        args: String,
        text_elements: Vec<TextElement>,
    ) -> Option<(String, Vec<TextElement>)> {
        if self.bottom_pane.composer_text().is_empty() {
            Some((args, text_elements))
        } else {
            self.bottom_pane
                .prepare_inline_args_submission(/*record_history*/ false)
        }
    }

    fn prepared_inline_user_message(
        &mut self,
        args: String,
        text_elements: Vec<TextElement>,
        mut local_images: Vec<LocalImageAttachment>,
        mut remote_image_urls: Vec<String>,
        mut mention_bindings: Vec<MentionBinding>,
        source: SlashCommandDispatchSource,
    ) -> UserMessage {
        if source == SlashCommandDispatchSource::Live {
            local_images = self
                .bottom_pane
                .take_recent_submission_images_with_placeholders();
            remote_image_urls = self.take_remote_image_urls();
            mention_bindings = self.bottom_pane.take_recent_submission_mention_bindings();
        }
        UserMessage {
            text: args,
            local_images,
            remote_image_urls,
            text_elements,
            mention_bindings,
        }
    }

    fn dispatch_prepared_command_with_args(
        &mut self,
        cmd: SlashCommand,
        prepared: PreparedSlashCommandArgs,
    ) {
        let PreparedSlashCommandArgs {
            args,
            text_elements,
            local_images,
            remote_image_urls,
            mention_bindings,
            source,
        } = prepared;
        let trimmed = args.trim();
        match cmd {
            SlashCommand::Ide => {
                self.handle_ide_command_args(trimmed);
            }
            SlashCommand::Mcp => match trimmed.to_ascii_lowercase().as_str() {
                "verbose" => self.add_mcp_output(McpServerStatusDetail::Full),
                _ => self.add_error_message("Usage: /mcp [verbose]".to_string()),
            },
            SlashCommand::Model => {
                self.handle_model_command_args(args);
            }
            SlashCommand::Provider => {
                self.handle_provider_command_args(args);
            }
            SlashCommand::Thinking => {
                self.handle_thinking_command_args(args);
            }
            SlashCommand::AgentCheck => {
                self.handle_agentcheck_command_args(args);
            }
            SlashCommand::Loop => {
                self.handle_loop_command_args(args);
            }
            SlashCommand::RalphLoop => {
                self.handle_ralphloop_command_args(args);
            }
            SlashCommand::Keymap => match trimmed.to_ascii_lowercase().as_str() {
                "" => self.open_keymap_picker(),
                "debug" => {
                    match crate::keymap::RuntimeKeymap::from_config(&self.config.tui_keymap) {
                        Ok(runtime_keymap) => self.open_keymap_debug(&runtime_keymap),
                        Err(err) => {
                            self.add_error_message(format!(
                                "Invalid `tui.keymap` configuration: {err}"
                            ));
                        }
                    }
                }
                _ => self.add_error_message("Usage: /keymap [debug]".to_string()),
            },
            SlashCommand::Raw => match trimmed.to_ascii_lowercase().as_str() {
                "on" => {
                    self.set_raw_output_mode_and_notify(/*enabled*/ true);
                    self.emit_raw_output_mode_changed(/*enabled*/ true);
                }
                "off" => {
                    self.set_raw_output_mode_and_notify(/*enabled*/ false);
                    self.emit_raw_output_mode_changed(/*enabled*/ false);
                }
                _ => self.add_error_message(RAW_USAGE.to_string()),
            },
            SlashCommand::Rename if !trimmed.is_empty() => {
                if !self.ensure_thread_rename_allowed() {
                    return;
                }
                self.session_telemetry
                    .counter("codex.thread.rename", /*inc*/ 1, &[]);
                let Some(name) = crate::legacy_core::util::normalize_thread_name(&args) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return;
                };
                self.app_event_tx.set_thread_name(name);
            }
            SlashCommand::Plan if !trimmed.is_empty() => {
                if !self.apply_plan_slash_command() {
                    return;
                }
                let user_message = self.prepared_inline_user_message(
                    args,
                    text_elements,
                    local_images,
                    remote_image_urls,
                    mention_bindings,
                    source,
                );
                if self.is_session_configured() {
                    self.reasoning_buffer.clear();
                    self.full_reasoning_buffer.clear();
                    self.set_status_header(String::from("Working"));
                    self.submit_user_message(user_message);
                } else {
                    self.queue_user_message(user_message);
                }
            }
            SlashCommand::Goal if !trimmed.is_empty() => {
                if !self.config.features.enabled(Feature::Goals) {
                    return;
                }
                enum GoalControlCommand {
                    Clear,
                    SetStatus(AppThreadGoalStatus),
                }
                let control_command = match trimmed.to_ascii_lowercase().as_str() {
                    "clear" => Some(GoalControlCommand::Clear),
                    "edit" => {
                        self.app_event_tx.send(AppEvent::OpenThreadGoalEditor {
                            thread_id: self.thread_id,
                        });
                        if source == SlashCommandDispatchSource::Live {
                            self.bottom_pane.drain_pending_submission_state();
                        }
                        return;
                    }
                    "pause" => Some(GoalControlCommand::SetStatus(AppThreadGoalStatus::Paused)),
                    "resume" => Some(GoalControlCommand::SetStatus(AppThreadGoalStatus::Active)),
                    _ => None,
                };
                if let Some(command) = control_command {
                    let Some(thread_id) = self.thread_id else {
                        self.add_info_message(
                            GOAL_USAGE.to_string(),
                            Some(
                                "The session must start before you can change a goal.".to_string(),
                            ),
                        );
                        return;
                    };
                    match command {
                        GoalControlCommand::Clear => {
                            self.app_event_tx
                                .send(AppEvent::ClearThreadGoal { thread_id });
                        }
                        GoalControlCommand::SetStatus(status) => {
                            self.app_event_tx
                                .send(AppEvent::SetThreadGoalStatus { thread_id, status });
                        }
                    }
                    self.append_message_history_entry(format!("/goal {trimmed}"));
                    if source == SlashCommandDispatchSource::Live {
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    return;
                }
                let objective = args.trim();
                if objective.is_empty() {
                    self.add_error_message("Goal objective must not be empty.".to_string());
                    self.add_info_message(
                        GOAL_USAGE.to_string(),
                        Some(GOAL_USAGE_HINT.to_string()),
                    );
                    if source == SlashCommandDispatchSource::Live {
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    return;
                }
                let validation_source = match source {
                    SlashCommandDispatchSource::Live => GoalObjectiveValidationSource::Live,
                    SlashCommandDispatchSource::Queued => GoalObjectiveValidationSource::Queued,
                };
                if !self.goal_objective_is_allowed(objective, validation_source) {
                    return;
                }
                let Some(thread_id) = self.thread_id else {
                    if source == SlashCommandDispatchSource::Live {
                        self.queue_user_message_with_options(
                            UserMessage {
                                text: format!("/goal {args}"),
                                local_images: Vec::new(),
                                remote_image_urls: Vec::new(),
                                text_elements: Vec::new(),
                                mention_bindings: Vec::new(),
                            },
                            QueuedInputAction::ParseSlash,
                        );
                        self.bottom_pane.drain_pending_submission_state();
                    } else {
                        self.add_info_message(
                            GOAL_USAGE.to_string(),
                            Some("The session must start before you can set a goal.".to_string()),
                        );
                    }
                    return;
                };
                self.app_event_tx.send(AppEvent::SetThreadGoalObjective {
                    thread_id,
                    objective: objective.to_string(),
                    mode: ThreadGoalSetMode::ConfirmIfExists,
                });
                self.append_message_history_entry(format!("/goal {trimmed}"));
                if source == SlashCommandDispatchSource::Live {
                    self.bottom_pane.drain_pending_submission_state();
                }
            }
            SlashCommand::Side | SlashCommand::Btw if !trimmed.is_empty() => {
                let Some(parent_thread_id) = self.thread_id else {
                    let command = cmd.command();
                    self.add_error_message(format!(
                        "'/{command}' is unavailable before the session starts."
                    ));
                    return;
                };
                let user_message = self.prepared_inline_user_message(
                    args,
                    text_elements,
                    local_images,
                    remote_image_urls,
                    mention_bindings,
                    source,
                );
                self.request_side_conversation(parent_thread_id, Some(user_message));
            }
            SlashCommand::Review if !trimmed.is_empty() => {
                self.submit_op(AppCommand::review(ReviewTarget::Custom {
                    instructions: args,
                }));
            }
            SlashCommand::Resume if !trimmed.is_empty() => {
                self.app_event_tx
                    .send(AppEvent::ResumeSessionByIdOrName(args));
            }
            SlashCommand::SandboxReadRoot if !trimmed.is_empty() => {
                self.app_event_tx
                    .send(AppEvent::BeginWindowsSandboxGrantReadRoot { path: args });
            }
            SlashCommand::Pets
                if matches!(
                    args.trim().to_ascii_lowercase().as_str(),
                    "disable" | "disabled" | "hide" | "hidden" | "off" | "none"
                ) =>
            {
                self.app_event_tx.send(AppEvent::PetDisabled);
            }
            SlashCommand::Pets if !trimmed.is_empty() => {
                self.select_pet_by_id(args);
            }
            _ => self.dispatch_command(cmd),
        }
        if source == SlashCommandDispatchSource::Live && cmd != SlashCommand::Goal {
            self.bottom_pane.drain_pending_submission_state();
        }
    }

    pub(super) fn submit_queued_slash_prompt(&mut self, user_message: UserMessage) -> QueueDrain {
        let UserMessage {
            text,
            local_images,
            remote_image_urls,
            text_elements,
            mention_bindings,
        } = user_message;
        let Some((name, rest, rest_offset)) = parse_slash_name(&text) else {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        };

        if name.contains('/') {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        }

        let service_tier_commands = self.current_model_service_tier_commands();
        let Some(command) =
            find_slash_command(name, self.builtin_command_flags(), &service_tier_commands)
        else {
            self.add_info_message(
                format!(
                    r#"Unrecognized command '/{name}'. Type "/" for a list of supported commands."#
                ),
                /*hint*/ None,
            );
            return QueueDrain::Continue;
        };

        if rest.is_empty() {
            return match command {
                SlashCommandItem::Builtin(cmd) => {
                    if Self::queued_command_cancels_prompt_loop(cmd) {
                        self.cancel_prompt_loop_for_thread_change();
                    }
                    self.dispatch_command(cmd);
                    self.queued_command_drain_result(cmd)
                }
                SlashCommandItem::ServiceTier(command) => {
                    self.handle_service_tier_command_dispatch(command);
                    QueueDrain::Continue
                }
            };
        }

        if !command.supports_inline_args() {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        }
        let SlashCommandItem::Builtin(cmd) = command else {
            self.submit_user_message(UserMessage {
                text,
                local_images,
                remote_image_urls,
                text_elements,
                mention_bindings,
            });
            return QueueDrain::Stop;
        };

        let trimmed_start = rest.trim_start();
        let leading_trimmed = rest.len().saturating_sub(trimmed_start.len());
        let trimmed_rest = trimmed_start.trim_end();
        let args_elements = Self::slash_command_args_elements(
            trimmed_rest,
            rest_offset + leading_trimmed,
            &text_elements,
        );
        if cmd == SlashCommand::Goal
            && !self.goal_objective_is_allowed(trimmed_rest, GoalObjectiveValidationSource::Queued)
        {
            return QueueDrain::Continue;
        }
        if Self::queued_command_cancels_prompt_loop(cmd) {
            self.cancel_prompt_loop_for_thread_change();
        }
        self.dispatch_prepared_command_with_args(
            cmd,
            PreparedSlashCommandArgs {
                args: trimmed_rest.to_string(),
                text_elements: args_elements,
                local_images,
                remote_image_urls,
                mention_bindings,
                source: SlashCommandDispatchSource::Queued,
            },
        );
        self.queued_command_drain_result(cmd)
    }

    fn builtin_command_flags(&self) -> BuiltinCommandFlags {
        #[cfg(target_os = "windows")]
        let allow_elevate_sandbox = {
            let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken)
        };
        #[cfg(not(target_os = "windows"))]
        let allow_elevate_sandbox = false;

        BuiltinCommandFlags {
            collaboration_modes_enabled: self.collaboration_modes_enabled(),
            connectors_enabled: self.connectors_enabled(),
            plugins_command_enabled: self.config.features.enabled(Feature::Plugins),
            goal_command_enabled: self.config.features.enabled(Feature::Goals),
            service_tier_commands_enabled: self.fast_mode_enabled(),
            personality_command_enabled: self.config.features.enabled(Feature::Personality),
            realtime_conversation_enabled: self.realtime_conversation_enabled(),
            audio_device_selection_enabled: self.realtime_audio_device_selection_enabled(),
            allow_elevate_sandbox,
            side_conversation_active: self.active_side_conversation,
        }
    }

    fn queued_command_drain_result(&self, cmd: SlashCommand) -> QueueDrain {
        if self.is_user_turn_pending_or_running() || !self.bottom_pane.no_modal_or_popup_active() {
            return QueueDrain::Stop;
        }
        match cmd {
            SlashCommand::Ide
            | SlashCommand::Status
            | SlashCommand::DebugConfig
            | SlashCommand::Ps
            | SlashCommand::Stop
            | SlashCommand::MemoryDrop
            | SlashCommand::MemoryUpdate
            | SlashCommand::Mcp
            | SlashCommand::Apps
            | SlashCommand::Plugins
            | SlashCommand::Rollout
            | SlashCommand::Copy
            | SlashCommand::Raw
            | SlashCommand::Vim
            | SlashCommand::Diff
            | SlashCommand::App
            | SlashCommand::Rename
            | SlashCommand::AgentCheck
            | SlashCommand::Thinking
            | SlashCommand::Loop
            | SlashCommand::RalphLoop
            | SlashCommand::TestApproval => QueueDrain::Continue,
            SlashCommand::Feedback
            | SlashCommand::New
            | SlashCommand::Archive
            | SlashCommand::Clear
            | SlashCommand::Resume
            | SlashCommand::Delete
            | SlashCommand::Fork
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Review
            | SlashCommand::Model
            | SlashCommand::ModelAdd
            | SlashCommand::Provider
            | SlashCommand::Realtime
            | SlashCommand::Settings
            | SlashCommand::Personality
            | SlashCommand::Plan
            | SlashCommand::Goal
            | SlashCommand::Side
            | SlashCommand::Btw
            | SlashCommand::Keymap
            | SlashCommand::Agent
            | SlashCommand::MultiAgents
            | SlashCommand::Permissions
            | SlashCommand::ElevateSandbox
            | SlashCommand::SandboxReadRoot
            | SlashCommand::Experimental
            | SlashCommand::AutoReview
            | SlashCommand::Memories
            | SlashCommand::Quit
            | SlashCommand::Exit
            | SlashCommand::Logout
            | SlashCommand::Mention
            | SlashCommand::Skills
            | SlashCommand::Hooks
            | SlashCommand::Title
            | SlashCommand::Statusline
            | SlashCommand::Theme
            | SlashCommand::Pets => QueueDrain::Stop,
        }
    }

    pub(super) fn queued_command_cancels_prompt_loop(cmd: SlashCommand) -> bool {
        matches!(
            cmd,
            SlashCommand::New
                | SlashCommand::Archive
                | SlashCommand::Clear
                | SlashCommand::Resume
                | SlashCommand::Delete
                | SlashCommand::Fork
                | SlashCommand::Side
                | SlashCommand::Btw
                | SlashCommand::Quit
                | SlashCommand::Exit
                | SlashCommand::Logout
        )
    }

    fn slash_command_args_elements(
        rest: &str,
        rest_offset: usize,
        text_elements: &[TextElement],
    ) -> Vec<TextElement> {
        if rest.is_empty() || text_elements.is_empty() {
            return Vec::new();
        }
        text_elements
            .iter()
            .filter_map(|elem| {
                if elem.byte_range.end <= rest_offset {
                    return None;
                }
                let start = elem.byte_range.start.saturating_sub(rest_offset);
                let mut end = elem.byte_range.end.saturating_sub(rest_offset);
                if start >= rest.len() {
                    return None;
                }
                end = end.min(rest.len());
                (start < end).then_some(elem.map_range(|_| ByteRange { start, end }))
            })
            .collect()
    }

    fn ensure_slash_command_allowed_in_side_conversation(&mut self, cmd: SlashCommand) -> bool {
        if !self.active_side_conversation || cmd.available_in_side_conversation() {
            return true;
        }
        self.add_error_message(format!(
            "'/{}' is unavailable in side conversations. {SIDE_SLASH_COMMAND_UNAVAILABLE_HINT}",
            cmd.command()
        ));
        self.bottom_pane.drain_pending_submission_state();
        false
    }

    fn ensure_side_command_allowed_outside_review(&mut self, cmd: SlashCommand) -> bool {
        if !matches!(cmd, SlashCommand::Side | SlashCommand::Btw) || !self.review.is_review_mode {
            return true;
        }

        let command = cmd.command();
        self.add_error_message(format!(
            "'/{command}' is unavailable while code review is running."
        ));
        self.bottom_pane.drain_pending_submission_state();
        false
    }
}

#[cfg(test)]
mod token_count_tests {
    use super::parse_token_count;

    #[test]
    fn parses_plain_numbers_and_separators() {
        assert_eq!(parse_token_count("1000000"), Some(1_000_000));
        assert_eq!(parse_token_count("1,000,000"), Some(1_000_000));
        assert_eq!(parse_token_count("  131072 "), Some(131_072));
    }

    #[test]
    fn k_suffix_is_binary_but_m_suffix_is_round() {
        assert_eq!(parse_token_count("128k"), Some(131_072));
        // A 1M model means a round 1,000,000. Rounding up to 1048576 would
        // overstate the window and cause context-overflow rejections.
        assert_eq!(parse_token_count("1m"), Some(1_000_000));
        assert_ne!(parse_token_count("1m"), Some(1_048_576));
    }

    #[test]
    fn rejects_junk_and_implausibly_small_windows() {
        assert_eq!(parse_token_count("abc"), None);
        assert_eq!(parse_token_count(""), None);
        assert_eq!(parse_token_count("0"), None);
        // Below 1024 is almost certainly a typo; silently shrinking a context
        // window is worse than refusing the input.
        assert_eq!(parse_token_count("512"), None);
        assert_eq!(parse_token_count("-5"), None);
    }
}
