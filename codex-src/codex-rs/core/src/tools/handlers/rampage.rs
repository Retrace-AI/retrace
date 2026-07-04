use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::function_arguments;
use crate::tools::handlers::multi_agents_common::tool_output_code_mode_result;
use crate::tools::handlers::multi_agents_common::tool_output_json_text;
use crate::tools::handlers::multi_agents_common::tool_output_response_item;
use crate::tools::handlers::multi_agents_v2::spawn::SpawnAgentArgs;
use crate::tools::handlers::multi_agents_v2::spawn::spawn_agent_with_args;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SessionSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use uuid::Uuid;

const RAMPAGE_DIR: &str = "rampage";
const RAMPAGE_CONTROL_TOOL: &str = "rampage_control";
const RAMPAGE_BOARD_TOOL: &str = "rampage_board";
const RAMPAGE_COMPACT_TOOL: &str = "rampage_compact";
const RAMPAGE_SPAWN_TOOL: &str = "rampage_spawn";
const SUPPORT_AGENT_NEW_IDEAS: &str = "new_ideas";
const SUPPORT_AGENT_EFFICIENCY: &str = "efficiency";

pub(crate) fn tools_enabled_for_turn(mode: ModeKind, source: &SessionSource) -> bool {
    matches!(mode, ModeKind::AbsoluteRampage | ModeKind::ReadonlyResearch)
        && !matches!(source, SessionSource::SubAgent(_))
}

#[derive(Clone, Copy)]
pub(crate) struct RampageToolOptions {
    mode: ModeKind,
}

#[derive(Debug, Clone)]
pub(crate) struct ActiveRampageMissionStatus {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) phase: String,
    pub(crate) total_tasks: usize,
    pub(crate) active_tasks: usize,
    pub(crate) done_tasks: usize,
    pub(crate) blocked_tasks: usize,
    pub(crate) board_items: usize,
    pub(crate) verifier_status: Option<String>,
    pub(crate) verifier_pass_threshold: f64,
    pub(crate) verifier_pass_percentage: Option<f64>,
    pub(crate) verifier_max_failures: Option<u64>,
    pub(crate) verifier_failure_count: u64,
    pub(crate) state_path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MissingSupportAgent {
    pub(crate) display_name: &'static str,
    pub(crate) task_name: &'static str,
    pub(crate) role: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) title: &'static str,
    pub(crate) instructions: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct SupportAgentSpawnGateStatus {
    pub(crate) mission_id: String,
    pub(crate) title: String,
    pub(crate) support_agents: String,
    pub(crate) missing_agents: Vec<MissingSupportAgent>,
    pub(crate) state_path: String,
}

pub(crate) async fn active_mission_status_for_thread(
    codex_home: &Path,
    root_thread_id: &str,
) -> Result<Option<ActiveRampageMissionStatus>, String> {
    let path = rampage_state_file_path(codex_home, root_thread_id);
    let contents = match fs::read_to_string(&path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read Rampage state at {}: {err}",
                path.display()
            ));
        }
    };
    let state = serde_json::from_str::<RampageState>(&contents)
        .map_err(|err| format!("failed to parse Rampage state at {}: {err}", path.display()))?;
    let Some(mission) = state.active_running_mission() else {
        return Ok(None);
    };
    let mission_tasks = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .collect::<Vec<_>>();
    let active_tasks = mission_tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "queued" | "running"))
        .count();
    let done_tasks = mission_tasks
        .iter()
        .filter(|task| task.status == "done")
        .count();
    let blocked_tasks = mission_tasks
        .iter()
        .filter(|task| task.status == "blocked")
        .count();
    let board_items = state
        .board_items
        .iter()
        .filter(|item| item.mission_id == mission.id && item.active)
        .count();

    Ok(Some(ActiveRampageMissionStatus {
        id: mission.id.clone(),
        title: mission.title.clone(),
        status: mission.status.clone(),
        phase: mission.phase.clone(),
        total_tasks: mission_tasks.len(),
        active_tasks,
        done_tasks,
        blocked_tasks,
        board_items,
        verifier_status: mission.verifier_status.clone(),
        verifier_pass_threshold: mission.verifier_pass_threshold,
        verifier_pass_percentage: mission.verifier_pass_percentage,
        verifier_max_failures: mission.verifier_max_failures,
        verifier_failure_count: mission.verifier_failure_count,
        state_path: path.display().to_string(),
    }))
}

pub(crate) async fn support_agent_spawn_gate_status_for_thread(
    codex_home: &Path,
    root_thread_id: &str,
) -> Result<Option<SupportAgentSpawnGateStatus>, String> {
    let path = rampage_state_file_path(codex_home, root_thread_id);
    let contents = match fs::read_to_string(&path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed to read Rampage state at {}: {err}",
                path.display()
            ));
        }
    };
    let state = serde_json::from_str::<RampageState>(&contents)
        .map_err(|err| format!("failed to parse Rampage state at {}: {err}", path.display()))?;
    let Some(mission) = state.active_running_mission() else {
        return Ok(None);
    };

    let missing_agents = required_support_agents(&mission.support_agents)
        .into_iter()
        .filter(|support_agent| !support_agent_has_spawned(&state, mission, support_agent))
        .map(missing_support_agent)
        .collect::<Vec<_>>();
    if missing_agents.is_empty() {
        return Ok(None);
    }

    Ok(Some(SupportAgentSpawnGateStatus {
        mission_id: mission.id.clone(),
        title: mission.title.clone(),
        support_agents: mission.support_agents.clone(),
        missing_agents,
        state_path: path.display().to_string(),
    }))
}

impl RampageToolOptions {
    pub(crate) fn new(mode: ModeKind) -> Self {
        Self { mode }
    }

    fn readonly(self) -> bool {
        matches!(self.mode, ModeKind::ReadonlyResearch)
    }

    fn controller_agent(self) -> &'static str {
        if self.readonly() {
            "readonly-research"
        } else {
            "absolute-rampage"
        }
    }

    fn display_name(self) -> &'static str {
        self.mode.display_name()
    }
}

#[derive(Clone)]
pub(crate) struct RampageControlHandler {
    options: RampageToolOptions,
}

impl RampageControlHandler {
    pub(crate) fn new(options: RampageToolOptions) -> Self {
        Self { options }
    }
}

#[derive(Clone)]
pub(crate) struct RampageBoardHandler {
    options: RampageToolOptions,
}

impl RampageBoardHandler {
    pub(crate) fn new(options: RampageToolOptions) -> Self {
        Self { options }
    }
}

#[derive(Clone)]
pub(crate) struct RampageCompactHandler {
    options: RampageToolOptions,
}

impl RampageCompactHandler {
    pub(crate) fn new(options: RampageToolOptions) -> Self {
        Self { options }
    }
}

#[derive(Clone)]
pub(crate) struct RampageSpawnHandler {
    options: RampageToolOptions,
}

impl RampageSpawnHandler {
    pub(crate) fn new(options: RampageToolOptions) -> Self {
        Self { options }
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RampageControlHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RAMPAGE_CONTROL_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        create_rampage_control_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let result = handle_rampage_control(invocation, self.options).await?;
        Ok(boxed_tool_output(RampageOutput::new(
            RAMPAGE_CONTROL_TOOL,
            result,
        )))
    }
}

impl CoreToolRuntime for RampageControlHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RampageBoardHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RAMPAGE_BOARD_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        create_rampage_board_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let result = handle_rampage_board(invocation).await?;
        Ok(boxed_tool_output(RampageOutput::new(
            RAMPAGE_BOARD_TOOL,
            result,
        )))
    }
}

impl CoreToolRuntime for RampageBoardHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RampageCompactHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RAMPAGE_COMPACT_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        create_rampage_compact_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let result = handle_rampage_compact(invocation).await?;
        Ok(boxed_tool_output(RampageOutput::new(
            RAMPAGE_COMPACT_TOOL,
            result,
        )))
    }
}

impl CoreToolRuntime for RampageCompactHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RampageSpawnHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RAMPAGE_SPAWN_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        create_rampage_spawn_tool(self.options)
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let result = handle_rampage_spawn(invocation, self.options).await?;
        Ok(boxed_tool_output(RampageOutput::new(
            RAMPAGE_SPAWN_TOOL,
            result,
        )))
    }
}

impl CoreToolRuntime for RampageSpawnHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageState {
    schema_version: u32,
    root_thread_id: String,
    active_mission_id: Option<String>,
    missions: Vec<RampageMission>,
    tasks: Vec<RampageTask>,
    board_items: Vec<RampageBoardItem>,
    briefs: Vec<RampageBrief>,
    events: Vec<RampageEvent>,
}

impl RampageState {
    fn new(root_thread_id: String) -> Self {
        Self {
            schema_version: 1,
            root_thread_id,
            active_mission_id: None,
            missions: Vec::new(),
            tasks: Vec::new(),
            board_items: Vec::new(),
            briefs: Vec::new(),
            events: Vec::new(),
        }
    }

    fn active_mission(&self) -> Option<&RampageMission> {
        let mission_id = self.active_mission_id.as_deref()?;
        self.missions
            .iter()
            .find(|mission| mission.id == mission_id)
    }

    fn active_mission_mut(&mut self) -> Option<&mut RampageMission> {
        let mission_id = self.active_mission_id.clone()?;
        self.missions
            .iter_mut()
            .find(|mission| mission.id == mission_id)
    }

    fn active_running_mission(&self) -> Option<&RampageMission> {
        self.active_mission().filter(|mission| {
            matches!(mission.status.as_str(), "running" | "blocked" | "verifying")
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageMission {
    id: String,
    root_thread_id: String,
    status: String,
    title: String,
    goal: String,
    success_criteria: String,
    phase: String,
    controller_agent: String,
    support_agents: String,
    verifier_status: Option<String>,
    verifier_notes: Option<String>,
    /// Percentage of success criteria that must be met for the verifier to pass (0-100).
    #[serde(default = "default_verifier_pass_threshold")]
    verifier_pass_threshold: f64,
    /// Maximum failed verification rounds before escalating to the user.
    /// `None` means infinite: the verifier keeps looping until it passes.
    #[serde(default)]
    verifier_max_failures: Option<u64>,
    /// Running count of failed verification rounds recorded so far.
    #[serde(default)]
    verifier_failure_count: u64,
    /// Last pass percentage the verifier reported (0-100).
    #[serde(default)]
    verifier_pass_percentage: Option<f64>,
    latest_brief_id: Option<String>,
    time_created: i64,
    time_updated: i64,
    time_completed: Option<i64>,
}

fn default_verifier_pass_threshold() -> f64 {
    100.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageTask {
    id: String,
    mission_id: String,
    parent_task_id: Option<String>,
    worker_session_id: Option<String>,
    status: String,
    kind: String,
    role: String,
    title: String,
    instructions: String,
    dependencies: Option<String>,
    model: Option<String>,
    result: Option<String>,
    confidence: Option<f64>,
    error: Option<String>,
    time_created: i64,
    time_started: Option<i64>,
    time_finished: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageBoardItem {
    id: String,
    mission_id: String,
    task_id: Option<String>,
    kind: String,
    title: String,
    body: String,
    source_role: String,
    artifact_path: Option<String>,
    confidence: Option<f64>,
    active: bool,
    time_created: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageBrief {
    id: String,
    mission_id: String,
    summary: String,
    open_tasks: String,
    completed_tasks: String,
    blockers: String,
    artifacts: String,
    next_actions: String,
    token_estimate: Option<u64>,
    time_created: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageEvent {
    id: String,
    mission_id: Option<String>,
    task_id: Option<String>,
    event: String,
    body: String,
    time_created: i64,
}

#[derive(Debug, Serialize)]
struct RampageResult {
    ok: bool,
    message: String,
    state_path: String,
    mission: Option<RampageMission>,
    tasks: Vec<RampageTask>,
    board_items: Vec<RampageBoardItem>,
    briefs: Vec<RampageBrief>,
    events: Vec<RampageEvent>,
}

#[derive(Debug, Serialize)]
struct RampageOutput {
    tool_name: &'static str,
    result: RampageResult,
}

impl RampageOutput {
    fn new(tool_name: &'static str, result: RampageResult) -> Self {
        Self { tool_name, result }
    }
}

impl ToolOutput for RampageOutput {
    fn log_preview(&self) -> String {
        tool_output_json_text(&self.result, self.tool_name)
    }

    fn success_for_logging(&self) -> bool {
        self.result.ok
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            &self.result,
            Some(self.result.ok),
            self.tool_name,
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(&self.result, self.tool_name)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RampageControlArgs {
    action: String,
    title: Option<String>,
    goal: Option<String>,
    success_criteria: Option<String>,
    phase: Option<String>,
    status: Option<String>,
    support_agents: Option<String>,
    verifier_status: Option<String>,
    verifier_notes: Option<String>,
    /// Startup: percentage of success criteria that count as a pass (0-100).
    verifier_pass_threshold: Option<f64>,
    /// Startup: max failed verification rounds before escalating to the user.
    /// Accepts a non-negative integer or the string `infinite` / `unlimited`.
    verifier_max_failures: Option<JsonValue>,
    /// verify_result: percentage of success criteria the verifier found met (0-100).
    pass_percentage: Option<f64>,
    /// verify_result: id of the durable verify task that produced this result.
    verify_task_id: Option<String>,
    task_id: Option<String>,
    task_status: Option<String>,
    task_result: Option<String>,
    task_confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RampageBoardArgs {
    action: String,
    kind: Option<String>,
    title: Option<String>,
    body: Option<String>,
    task_id: Option<String>,
    source_role: Option<String>,
    artifact_path: Option<String>,
    confidence: Option<f64>,
    active: Option<bool>,
    active_only: Option<bool>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RampageCompactArgs {
    summary: String,
    open_tasks: String,
    completed_tasks: String,
    blockers: String,
    artifacts: String,
    next_actions: String,
    token_estimate: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RampageSpawnArgs {
    task_name: String,
    title: String,
    instructions: String,
    kind: Option<String>,
    role: Option<String>,
    parent_task_id: Option<String>,
    dependencies: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    fork_turns: Option<String>,
}

async fn handle_rampage_control(
    invocation: ToolInvocation,
    options: RampageToolOptions,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageControlArgs =
        parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let action = normalize_token(&args.action);
    match action.as_str() {
        "start" => {
            handle_control_start(
                &mut state,
                &args,
                options,
                invocation.session.thread_id.to_string(),
            )?;
        }
        "status" => {}
        "update" => {
            handle_control_update(&mut state, &args)?;
        }
        "complete" => {
            handle_control_complete(&mut state, &args)?;
        }
        "task_result" => {
            handle_control_task_result(&mut state, &args)?;
        }
        "verify_result" => {
            handle_control_verify_result(&mut state, &args)?;
        }
        _ => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported rampage_control action `{}`; use start, status, update, task_result, verify_result, or complete",
                args.action
            )));
        }
    }

    if action != "status" {
        save_state(&path, &state).await?;
    }
    Ok(result_from_state(
        true,
        format!("rampage_control {action} recorded"),
        &path,
        &state,
    ))
}

fn handle_control_start(
    state: &mut RampageState,
    args: &RampageControlArgs,
    options: RampageToolOptions,
    root_thread_id: String,
) -> Result<(), FunctionCallError> {
    if let Some(existing) = state.active_running_mission() {
        return Err(FunctionCallError::RespondToModel(format!(
            "active Rampage mission `{}` already exists with status `{}`; call rampage_control status or update it instead of starting a second mission",
            existing.id, existing.status
        )));
    }

    let support_agents = args
        .support_agents
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "rampage_control start requires support_agents from the startup question; call request_user_input first with Both support agents, New Ideas only, Efficiency only, or No support agents".to_string(),
            )
        })
        .map(normalize_token)?;
    validate_support_agents(&support_agents)?;

    let verifier_pass_threshold = args.verifier_pass_threshold.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "rampage_control start requires verifier_pass_threshold (0-100) from the mandatory verifier-config question; ask the user what percentage of success criteria counts as a pass".to_string(),
        )
    })?;
    if !(0.0..=100.0).contains(&verifier_pass_threshold) {
        return Err(FunctionCallError::RespondToModel(
            "verifier_pass_threshold must be between 0 and 100".to_string(),
        ));
    }
    let verifier_max_failures = parse_verifier_max_failures(args.verifier_max_failures.as_ref())?;

    let now = now_unix_timestamp_ms();
    let mission_id = format!("mission-{}", Uuid::new_v4());
    let mission = RampageMission {
        id: mission_id.clone(),
        root_thread_id,
        status: "running".to_string(),
        title: required_string(args.title.as_deref(), "title")?,
        goal: required_string(args.goal.as_deref(), "goal")?,
        success_criteria: required_string(args.success_criteria.as_deref(), "success_criteria")?,
        phase: args
            .phase
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("startup")
            .to_string(),
        controller_agent: options.controller_agent().to_string(),
        support_agents: support_agents.clone(),
        verifier_status: None,
        verifier_notes: None,
        verifier_pass_threshold,
        verifier_max_failures,
        verifier_failure_count: 0,
        verifier_pass_percentage: None,
        latest_brief_id: None,
        time_created: now,
        time_updated: now,
        time_completed: None,
    };
    state.active_mission_id = Some(mission_id.clone());
    state.missions.push(mission);
    state.board_items.push(RampageBoardItem {
        id: format!("board-{}", Uuid::new_v4()),
        mission_id: mission_id.clone(),
        task_id: None,
        kind: "decision".to_string(),
        title: "Support-agent choice".to_string(),
        body: support_agents.clone(),
        source_role: "Mission Control".to_string(),
        artifact_path: None,
        confidence: None,
        active: true,
        time_created: now,
    });
    push_event(
        state,
        Some(mission_id),
        None,
        "mission_created",
        format!(
            "{} mission started with support_agents={support_agents}, verifier_pass_threshold={verifier_pass_threshold:.0}%, verifier_max_failures={}",
            options.display_name(),
            verifier_max_failures
                .map(|max| max.to_string())
                .unwrap_or_else(|| "infinite".to_string()),
        ),
    );
    Ok(())
}

fn handle_control_update(
    state: &mut RampageState,
    args: &RampageControlArgs,
) -> Result<(), FunctionCallError> {
    let mission_id = {
        let mission = required_active_mission_mut(state)?;
        if let Some(status) = args.status.as_deref().map(normalize_token) {
            validate_mission_status(&status)?;
            mission.status = status;
        }
        if let Some(phase) = nonempty(args.phase.as_deref()) {
            mission.phase = phase.to_string();
        }
        if let Some(verifier_status) = nonempty(args.verifier_status.as_deref()) {
            mission.verifier_status = Some(normalize_token(verifier_status));
        }
        if let Some(verifier_notes) = nonempty(args.verifier_notes.as_deref()) {
            mission.verifier_notes = Some(verifier_notes.to_string());
        }
        mission.time_updated = now_unix_timestamp_ms();
        mission.id.clone()
    };
    push_event(
        state,
        Some(mission_id),
        None,
        "mission_updated",
        "mission fields updated",
    );
    Ok(())
}

fn handle_control_complete(
    state: &mut RampageState,
    args: &RampageControlArgs,
) -> Result<(), FunctionCallError> {
    let verifier_status = args
        .verifier_status
        .as_deref()
        .map(normalize_token)
        .or_else(|| {
            state
                .active_mission()
                .and_then(|mission| mission.verifier_status.clone())
        })
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "rampage_control complete requires verifier_status=passed and verifier_notes"
                    .to_string(),
            )
        })?;
    if !matches!(verifier_status.as_str(), "passed" | "complete" | "verified") {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control complete refused: verifier_status must be passed, complete, or verified".to_string(),
        ));
    }
    let verifier_notes = args
        .verifier_notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .or_else(|| {
            state
                .active_mission()
                .and_then(|mission| mission.verifier_notes.as_deref())
        })
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "rampage_control complete refused: verifier_notes are required".to_string(),
            )
        })?
        .to_string();

    let mission = required_active_mission_record(state)?.clone();
    validate_selected_support_agents_completed(state, &mission)?;

    // The verifier is mandatory and threshold-gated: a real verify worker must have
    // run, and its recorded pass percentage must meet the mission's threshold.
    ensure_verify_agent_ran(state, &mission.id)?;
    match mission.verifier_pass_percentage {
        Some(pass_percentage) if pass_percentage >= mission.verifier_pass_threshold => {}
        Some(pass_percentage) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "rampage_control complete refused: verifier pass {pass_percentage:.0}% is below the required {:.0}% threshold. Continue the mission or escalate to the user.",
                mission.verifier_pass_threshold
            )));
        }
        None => {
            return Err(FunctionCallError::RespondToModel(
                "rampage_control complete refused: no verifier pass percentage recorded. Run the verify worker and record it with `rampage_control action=verify_result` first.".to_string(),
            ));
        }
    }

    let now = now_unix_timestamp_ms();
    let mission_id = {
        let mission = required_active_mission_mut(state)?;
        mission.status = "completed".to_string();
        mission.phase = "completed".to_string();
        mission.verifier_status = Some(verifier_status);
        mission.verifier_notes = Some(verifier_notes.clone());
        mission.time_updated = now;
        mission.time_completed = Some(now);
        mission.id.clone()
    };
    push_event(
        state,
        Some(mission_id),
        None,
        "mission_completed",
        verifier_notes,
    );
    Ok(())
}

fn handle_control_task_result(
    state: &mut RampageState,
    args: &RampageControlArgs,
) -> Result<(), FunctionCallError> {
    let task_id = required_string(args.task_id.as_deref(), "task_id")?;
    let result = required_string(args.task_result.as_deref(), "task_result")?;
    let status = args
        .task_status
        .as_deref()
        .map(normalize_token)
        .unwrap_or_else(|| "done".to_string());
    validate_task_status(&status)?;
    let now = now_unix_timestamp_ms();
    let task = state
        .tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!("unknown rampage task `{task_id}`"))
        })?;
    task.status = status.clone();
    task.result = Some(result.clone());
    task.confidence = args.task_confidence;
    task.time_finished = Some(now);
    let mission_id = task.mission_id.clone();
    let task_id = task.id.clone();
    let task_title = task.title.clone();
    let task_role = task.role.clone();
    state.board_items.push(RampageBoardItem {
        id: format!("board-{}", Uuid::new_v4()),
        mission_id: mission_id.clone(),
        task_id: Some(task_id.clone()),
        kind: if status == "blocked" {
            "blocker".to_string()
        } else {
            "finding".to_string()
        },
        title: format!("{task_title} result"),
        body: result.clone(),
        source_role: task_role,
        artifact_path: None,
        confidence: args.task_confidence,
        active: true,
        time_created: now,
    });
    push_event(
        state,
        Some(mission_id),
        Some(task_id),
        "task_updated",
        "task result recorded",
    );
    Ok(())
}

/// Records the outcome of a bounded verifier round.
///
/// The verify agent reports `pass_percentage` (fraction of the mission's success
/// criteria it found met). Mission Control routes that here:
/// - `pass_percentage >= verifier_pass_threshold` marks the verifier passed, unblocking
///   `rampage_control action=complete`.
/// - otherwise the round counts as a failure. When the failure count reaches the
///   configured `verifier_max_failures`, the mission is set to `blocked` so Mission
///   Control escalates to the user. `verifier_max_failures = infinite` never escalates.
fn handle_control_verify_result(
    state: &mut RampageState,
    args: &RampageControlArgs,
) -> Result<(), FunctionCallError> {
    let pass_percentage = args.pass_percentage.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "rampage_control verify_result requires pass_percentage (0-100), the fraction of success criteria the verifier found met".to_string(),
        )
    })?;
    if !(0.0..=100.0).contains(&pass_percentage) {
        return Err(FunctionCallError::RespondToModel(
            "pass_percentage must be between 0 and 100".to_string(),
        ));
    }
    let verifier_notes = required_string(args.verifier_notes.as_deref(), "verifier_notes")?;

    // A verify_result must be backed by a durable, spawned verify task with a result.
    let mission_id = required_active_mission_record(state)?.id.clone();
    ensure_verify_agent_ran(state, &mission_id)?;

    let now = now_unix_timestamp_ms();
    let escalate;
    {
        let mission = required_active_mission_mut(state)?;
        mission.verifier_pass_percentage = Some(pass_percentage);
        mission.verifier_notes = Some(verifier_notes.clone());
        mission.time_updated = now;
        if pass_percentage >= mission.verifier_pass_threshold {
            mission.verifier_status = Some("passed".to_string());
            mission.verifier_failure_count = 0;
            escalate = false;
        } else {
            mission.verifier_status = Some("failed".to_string());
            mission.verifier_failure_count = mission.verifier_failure_count.saturating_add(1);
            escalate = mission
                .verifier_max_failures
                .is_some_and(|max| mission.verifier_failure_count >= max);
            if escalate {
                mission.status = "blocked".to_string();
            }
        }
    }

    let (kind, title, body) = if pass_percentage
        >= required_active_mission_record(state)?.verifier_pass_threshold
    {
        (
            "finding",
            "Verifier passed".to_string(),
            format!("Verifier reported {pass_percentage:.0}% of success criteria met. {verifier_notes}"),
        )
    } else if escalate {
        let failures = required_active_mission_record(state)?.verifier_failure_count;
        (
            "blocker",
            "Verifier failed - escalating to user".to_string(),
            format!(
                "Verifier reported {pass_percentage:.0}% (below threshold) after {failures} failed rounds. Mission blocked; ask the user how to proceed. {verifier_notes}"
            ),
        )
    } else {
        (
            "blocker",
            "Verifier failed - continuing".to_string(),
            format!("Verifier reported {pass_percentage:.0}% (below threshold). Write missing work and continue. {verifier_notes}"),
        )
    };
    state.board_items.push(RampageBoardItem {
        id: format!("board-{}", Uuid::new_v4()),
        mission_id: mission_id.clone(),
        task_id: args.verify_task_id.clone(),
        kind: kind.to_string(),
        title,
        body,
        source_role: "Verifier".to_string(),
        artifact_path: None,
        confidence: None,
        active: true,
        time_created: now,
    });
    push_event(
        state,
        Some(mission_id),
        args.verify_task_id.clone(),
        "verifier_updated",
        format!("verifier pass_percentage={pass_percentage:.0}, escalate={escalate}"),
    );
    Ok(())
}

async fn handle_rampage_board(
    invocation: ToolInvocation,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageBoardArgs = parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let action = normalize_token(&args.action);
    match action.as_str() {
        "add" => {
            let mission_id = required_active_mission(&state)?.id.clone();
            let kind = args
                .kind
                .as_deref()
                .map(normalize_token)
                .unwrap_or_else(|| "finding".to_string());
            validate_board_kind(&kind)?;
            let item = RampageBoardItem {
                id: format!("board-{}", Uuid::new_v4()),
                mission_id: mission_id.clone(),
                task_id: args.task_id.clone(),
                kind,
                title: required_string(args.title.as_deref(), "title")?,
                body: required_string(args.body.as_deref(), "body")?,
                source_role: args
                    .source_role
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("Mission Control")
                    .to_string(),
                artifact_path: args.artifact_path.clone(),
                confidence: args.confidence,
                active: args.active.unwrap_or(true),
                time_created: now_unix_timestamp_ms(),
            };
            let item_id = item.id.clone();
            state.board_items.push(item);
            push_event(
                &mut state,
                Some(mission_id),
                args.task_id.clone(),
                "board_item_added",
                item_id,
            );
            save_state(&path, &state).await?;
        }
        "list" => {}
        _ => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported rampage_board action `{}`; use add or list",
                args.action
            )));
        }
    }
    let mut result = result_from_state(
        true,
        format!("rampage_board {action} recorded"),
        &path,
        &state,
    );
    result.board_items = filtered_board_items(&state, &args);
    Ok(result)
}

async fn handle_rampage_compact(
    invocation: ToolInvocation,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageCompactArgs =
        parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let mission_id = required_active_mission(&state)?.id.clone();
    let brief_id = format!("brief-{}", Uuid::new_v4());
    let brief = RampageBrief {
        id: brief_id.clone(),
        mission_id: mission_id.clone(),
        summary: args.summary,
        open_tasks: args.open_tasks,
        completed_tasks: args.completed_tasks,
        blockers: args.blockers,
        artifacts: args.artifacts,
        next_actions: args.next_actions,
        token_estimate: args.token_estimate,
        time_created: now_unix_timestamp_ms(),
    };
    state.briefs.push(brief);
    if let Some(mission) = state.active_mission_mut() {
        mission.latest_brief_id = Some(brief_id.clone());
        mission.time_updated = now_unix_timestamp_ms();
    }
    push_event(
        &mut state,
        Some(mission_id),
        None,
        "brief_created",
        brief_id,
    );
    save_state(&path, &state).await?;
    Ok(result_from_state(
        true,
        "rampage_compact created durable mission brief",
        &path,
        &state,
    ))
}

async fn handle_rampage_spawn(
    invocation: ToolInvocation,
    options: RampageToolOptions,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageSpawnArgs = parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let mission = required_active_mission(&state)?.clone();
    let task_id = format!("task-{}", Uuid::new_v4());
    let now = now_unix_timestamp_ms();
    let task_kind = args
        .kind
        .as_deref()
        .map(normalize_token)
        .unwrap_or_else(|| {
            if options.readonly() {
                "research".to_string()
            } else {
                "work".to_string()
            }
        });
    validate_task_kind(&task_kind)?;
    let role = args
        .role
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .unwrap_or_else(|| {
            if options.readonly() {
                "readonly-research-worker"
            } else {
                "rampage-worker"
            }
        })
        .to_string();
    let task = RampageTask {
        id: task_id.clone(),
        mission_id: mission.id.clone(),
        parent_task_id: args.parent_task_id.clone(),
        worker_session_id: None,
        status: "queued".to_string(),
        kind: task_kind.clone(),
        role: role.clone(),
        title: args.title.clone(),
        instructions: args.instructions.clone(),
        dependencies: args.dependencies.clone(),
        model: args.model.clone(),
        result: None,
        confidence: None,
        error: None,
        time_created: now,
        time_started: None,
        time_finished: None,
    };
    state.tasks.push(task);
    push_event(
        &mut state,
        Some(mission.id.clone()),
        Some(task_id.clone()),
        "task_created",
        args.title.clone(),
    );
    save_state(&path, &state).await?;

    let message = worker_brief(&state, &mission, &task_id, &args, &role, options);
    mark_task_running(&mut state, &task_id);
    save_state(&path, &state).await?;

    let spawn_args = SpawnAgentArgs {
        message,
        task_name: args.task_name.clone(),
        agent_type: None,
        model: args.model,
        reasoning_effort: args.reasoning_effort,
        service_tier: args.service_tier,
        fork_turns: args.fork_turns.or_else(|| Some("none".to_string())),
        fork_context: None,
    };
    match spawn_agent_with_args(invocation, spawn_args).await {
        Ok(spawn_result) => {
            let worker_session_id = spawn_result.task_name().to_string();
            mark_task_spawned(&mut state, &task_id, worker_session_id.clone());
            state.board_items.push(RampageBoardItem {
                id: format!("board-{}", Uuid::new_v4()),
                mission_id: mission.id.clone(),
                task_id: Some(task_id.clone()),
                kind: "next_action".to_string(),
                title: format!("Worker spawned: {}", args.title),
                body: format!(
                    "Worker `{worker_session_id}` is running. Wait for the worker, then record useful findings with rampage_control action=task_result and rampage_board add."
                ),
                source_role: "Mission Control".to_string(),
                artifact_path: None,
                confidence: None,
                active: true,
                time_created: now_unix_timestamp_ms(),
            });
            push_event(
                &mut state,
                Some(mission.id),
                Some(task_id),
                "task_updated",
                format!("worker spawned: {worker_session_id}"),
            );
            save_state(&path, &state).await?;
            Ok(result_from_state(
                true,
                format!("rampage_spawn created worker `{worker_session_id}`"),
                &path,
                &state,
            ))
        }
        Err(err) => {
            mark_task_failed(&mut state, &task_id, err.to_string());
            save_state(&path, &state).await?;
            Err(err)
        }
    }
}

fn create_rampage_control_tool(options: RampageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "action".to_string(),
        JsonSchema::string_enum(
            strings(&[
                "start",
                "status",
                "update",
                "task_result",
                "verify_result",
                "complete",
            ]),
            Some("Mission lifecycle action.".to_string()),
        ),
    );
    properties.insert(
        "title".to_string(),
        JsonSchema::string(Some("Mission title for action=start.".to_string())),
    );
    properties.insert(
        "goal".to_string(),
        JsonSchema::string(Some("Original user goal for action=start.".to_string())),
    );
    properties.insert(
        "success_criteria".to_string(),
        JsonSchema::string(Some(
            "Concrete success criteria for action=start.".to_string(),
        )),
    );
    properties.insert(
        "phase".to_string(),
        JsonSchema::string(Some("Current mission phase.".to_string())),
    );
    properties.insert(
        "status".to_string(),
        JsonSchema::string_enum(
            strings(&[
                "running",
                "paused",
                "blocked",
                "verifying",
                "completed",
                "stopped",
            ]),
            Some("Mission status for action=update.".to_string()),
        ),
    );
    properties.insert(
        "support_agents".to_string(),
        JsonSchema::string_enum(
            strings(&["both", "new_ideas_only", "efficiency_only", "none"]),
            Some("Startup support-agent choice gathered from request_user_input.".to_string()),
        ),
    );
    properties.insert(
        "verifier_status".to_string(),
        JsonSchema::string(Some(
            "Verifier status; completion requires passed, complete, or verified.".to_string(),
        )),
    );
    properties.insert(
        "verifier_pass_threshold".to_string(),
        JsonSchema::number(Some(
            "Startup (mandatory): percentage of success criteria that counts as a verifier pass (0-100). Gather from the mandatory verifier-config question.".to_string(),
        )),
    );
    properties.insert(
        "verifier_max_failures".to_string(),
        JsonSchema::string(Some(
            "Startup (mandatory): max failed verification rounds before escalating to the user. Use a non-negative integer or `infinite` to loop until it passes.".to_string(),
        )),
    );
    properties.insert(
        "pass_percentage".to_string(),
        JsonSchema::number(Some(
            "verify_result: percentage of the mission's success criteria the verifier found met (0-100).".to_string(),
        )),
    );
    properties.insert(
        "verify_task_id".to_string(),
        JsonSchema::string(Some(
            "verify_result: id of the durable kind=verify task that produced this result.".to_string(),
        )),
    );
    properties.insert(
        "verifier_notes".to_string(),
        JsonSchema::string(Some(
            "Bounded verifier notes proving completion or missing work.".to_string(),
        )),
    );
    properties.insert(
        "task_id".to_string(),
        JsonSchema::string(Some("Task id for action=task_result.".to_string())),
    );
    properties.insert(
        "task_status".to_string(),
        JsonSchema::string_enum(
            strings(&["done", "blocked", "failed", "cancelled"]),
            Some("Task terminal status for action=task_result.".to_string()),
        ),
    );
    properties.insert(
        "task_result".to_string(),
        JsonSchema::string(Some(
            "Structured worker result to write to the task and Questboard.".to_string(),
        )),
    );
    properties.insert(
        "task_confidence".to_string(),
        JsonSchema::number(Some("Optional 0-1 confidence score.".to_string())),
    );
    function_tool(
        RAMPAGE_CONTROL_TOOL,
        &format!(
            "Manage the durable {} mission lifecycle. start/status/update/task_result/verify_result/complete Mission Control state. Completion is gated on a mandatory verify agent meeting the pass-percentage threshold.",
            options.display_name()
        ),
        properties,
        vec!["action".to_string()],
    )
}

fn create_rampage_board_tool(_options: RampageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "action".to_string(),
        JsonSchema::string_enum(
            strings(&["add", "list"]),
            Some("Questboard action.".to_string()),
        ),
    );
    properties.insert(
        "kind".to_string(),
        JsonSchema::string_enum(
            strings(&[
                "finding",
                "decision",
                "blocker",
                "artifact",
                "assumption",
                "next_action",
            ]),
            Some("Questboard item kind for action=add or filter for action=list.".to_string()),
        ),
    );
    properties.insert(
        "title".to_string(),
        JsonSchema::string(Some("Short Questboard item title.".to_string())),
    );
    properties.insert(
        "body".to_string(),
        JsonSchema::string(Some(
            "Evidence, decision, blocker, artifact, or action detail.".to_string(),
        )),
    );
    properties.insert(
        "task_id".to_string(),
        JsonSchema::string(Some("Optional related rampage_task id.".to_string())),
    );
    properties.insert(
        "source_role".to_string(),
        JsonSchema::string(Some(
            "Source role, such as Mission Control or worker role.".to_string(),
        )),
    );
    properties.insert(
        "artifact_path".to_string(),
        JsonSchema::string(Some("Optional local artifact path.".to_string())),
    );
    properties.insert(
        "confidence".to_string(),
        JsonSchema::number(Some("Optional 0-1 confidence score.".to_string())),
    );
    properties.insert(
        "active".to_string(),
        JsonSchema::boolean(Some("Whether the board item remains active.".to_string())),
    );
    properties.insert(
        "active_only".to_string(),
        JsonSchema::boolean(Some("For list, omit inactive items when true.".to_string())),
    );
    properties.insert(
        "limit".to_string(),
        JsonSchema::integer(Some("For list, maximum items to return.".to_string())),
    );
    function_tool(
        RAMPAGE_BOARD_TOOL,
        "Read or write the durable Questboard. Workers do not coordinate with each other; Mission Control records structured evidence here.",
        properties,
        vec!["action".to_string()],
    )
}

fn create_rampage_compact_tool(_options: RampageToolOptions) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "summary".to_string(),
            JsonSchema::string(Some("Compacted mission summary.".to_string())),
        ),
        (
            "open_tasks".to_string(),
            JsonSchema::string(Some("Open tasks that still matter.".to_string())),
        ),
        (
            "completed_tasks".to_string(),
            JsonSchema::string(Some("Completed task summary.".to_string())),
        ),
        (
            "blockers".to_string(),
            JsonSchema::string(Some("Active blockers and unlock plans.".to_string())),
        ),
        (
            "artifacts".to_string(),
            JsonSchema::string(Some(
                "Known artifacts, paths, results, or links.".to_string(),
            )),
        ),
        (
            "next_actions".to_string(),
            JsonSchema::string(Some("Next concrete controller actions.".to_string())),
        ),
        (
            "token_estimate".to_string(),
            JsonSchema::integer(Some("Optional token estimate for the brief.".to_string())),
        ),
    ]);
    function_tool(
        RAMPAGE_COMPACT_TOOL,
        "Create a durable mission compaction brief and link it from the active mission.",
        properties,
        vec![
            "summary".to_string(),
            "open_tasks".to_string(),
            "completed_tasks".to_string(),
            "blockers".to_string(),
            "artifacts".to_string(),
            "next_actions".to_string(),
        ],
    )
}

fn create_rampage_spawn_tool(options: RampageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::new();
    properties.insert(
        "task_name".to_string(),
        JsonSchema::string(Some(
            "Canonical worker task name, for example code_scan or verifier.".to_string(),
        )),
    );
    properties.insert(
        "title".to_string(),
        JsonSchema::string(Some("Durable rampage_task title.".to_string())),
    );
    properties.insert(
        "instructions".to_string(),
        JsonSchema::string(Some(
            "Focused worker brief. Workers must return structured evidence.".to_string(),
        )),
    );
    properties.insert(
        "kind".to_string(),
        JsonSchema::string_enum(
            strings(&["research", "work", "review", "verify", "compact"]),
            Some("Task kind.".to_string()),
        ),
    );
    properties.insert(
        "role".to_string(),
        JsonSchema::string(Some("Worker role label.".to_string())),
    );
    properties.insert(
        "parent_task_id".to_string(),
        JsonSchema::string(Some("Optional parent task id.".to_string())),
    );
    properties.insert(
        "dependencies".to_string(),
        JsonSchema::string(Some("Optional dependency notes or task ids.".to_string())),
    );
    properties.insert(
        "model".to_string(),
        JsonSchema::string(Some("Optional child model override.".to_string())),
    );
    properties.insert(
        "reasoning_effort".to_string(),
        JsonSchema::string(Some(
            "Optional child reasoning effort override.".to_string(),
        )),
    );
    properties.insert(
        "service_tier".to_string(),
        JsonSchema::string(Some("Optional child service tier override.".to_string())),
    );
    properties.insert(
        "fork_turns".to_string(),
        JsonSchema::string(Some(
            "none, all, or a positive integer string. Defaults to none so the durable mission brief controls worker context.".to_string(),
        )),
    );
    let description = if options.readonly() {
        "Create a durable read-only research worker task, inject mission/Questboard/brief context, and spawn the worker. Workers cannot coordinate freely and must report evidence back to Mission Control."
    } else {
        "Create a durable Rampage worker task, inject mission/Questboard/brief context, and spawn the worker. This is the only delegation primitive for Absolute Rampage."
    };
    function_tool(
        RAMPAGE_SPAWN_TOOL,
        description,
        properties,
        vec![
            "task_name".to_string(),
            "title".to_string(),
            "instructions".to_string(),
        ],
    )
}

fn function_tool(
    name: &str,
    description: &str,
    properties: BTreeMap<String, JsonSchema>,
    required: Vec<String>,
) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(required), Some(false.into())),
        output_schema: None,
    })
}

fn strings(values: &[&str]) -> Vec<JsonValue> {
    values.iter().map(|value| json!(value)).collect()
}

fn rampage_state_path(invocation: &ToolInvocation) -> PathBuf {
    rampage_state_file_path(
        invocation.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
    )
}

fn rampage_state_file_path(codex_home: &Path, root_thread_id: &str) -> PathBuf {
    codex_home
        .join(RAMPAGE_DIR)
        .join(format!("{root_thread_id}.json"))
}

async fn load_state(
    path: &Path,
    root_thread_id: String,
) -> Result<RampageState, FunctionCallError> {
    match fs::read_to_string(path).await {
        Ok(contents) => serde_json::from_str::<RampageState>(&contents).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse Rampage state at {}: {err}",
                path.display()
            ))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(RampageState::new(root_thread_id))
        }
        Err(err) => Err(FunctionCallError::RespondToModel(format!(
            "failed to read Rampage state at {}: {err}",
            path.display()
        ))),
    }
}

async fn save_state(path: &Path, state: &RampageState) -> Result<(), FunctionCallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to create Rampage state directory {}: {err}",
                parent.display()
            ))
        })?;
    }
    let contents = serde_json::to_string_pretty(state).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize Rampage state: {err}"))
    })?;
    fs::write(path, contents).await.map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to write Rampage state at {}: {err}",
            path.display()
        ))
    })
}

fn result_from_state(
    ok: bool,
    message: impl Into<String>,
    path: &Path,
    state: &RampageState,
) -> RampageResult {
    RampageResult {
        ok,
        message: message.into(),
        state_path: path.display().to_string(),
        mission: state.active_mission().cloned(),
        tasks: state.tasks.clone(),
        board_items: state.board_items.clone(),
        briefs: state.briefs.clone(),
        events: state.events.clone(),
    }
}

fn required_active_mission(state: &RampageState) -> Result<&RampageMission, FunctionCallError> {
    state.active_running_mission().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "no active Rampage mission exists; call rampage_control action=start first".to_string(),
        )
    })
}

fn required_active_mission_record(
    state: &RampageState,
) -> Result<&RampageMission, FunctionCallError> {
    state.active_mission().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "no active Rampage mission exists; call rampage_control action=start first".to_string(),
        )
    })
}

fn required_active_mission_mut(
    state: &mut RampageState,
) -> Result<&mut RampageMission, FunctionCallError> {
    state.active_mission_mut().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "no active Rampage mission exists; call rampage_control action=start first".to_string(),
        )
    })
}

fn required_string(value: Option<&str>, field_name: &str) -> Result<String, FunctionCallError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!("missing required `{field_name}`"))
        })
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalize_token(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

fn validate_mission_status(status: &str) -> Result<(), FunctionCallError> {
    if matches!(
        status,
        "running" | "paused" | "blocked" | "verifying" | "completed" | "stopped"
    ) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "invalid Rampage mission status `{status}`"
        )))
    }
}

/// Parses the startup `verifier_max_failures` value.
///
/// Accepts a non-negative integer (number or numeric string) or the strings
/// `infinite` / `unlimited` / `none`, which map to `None` (never escalate).
fn parse_verifier_max_failures(
    value: Option<&JsonValue>,
) -> Result<Option<u64>, FunctionCallError> {
    let Some(value) = value else {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control start requires verifier_max_failures from the mandatory verifier-config question; use a non-negative integer or `infinite`".to_string(),
        ));
    };
    match value {
        JsonValue::Number(number) => {
            let failures = number.as_u64().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "verifier_max_failures must be a non-negative integer or `infinite`".to_string(),
                )
            })?;
            Ok(Some(failures))
        }
        JsonValue::String(text) => {
            let normalized = normalize_token(text);
            if matches!(normalized.as_str(), "infinite" | "unlimited" | "none") {
                return Ok(None);
            }
            let failures = normalized.parse::<u64>().map_err(|_| {
                FunctionCallError::RespondToModel(format!(
                    "verifier_max_failures `{text}` is invalid; use a non-negative integer or `infinite`"
                ))
            })?;
            Ok(Some(failures))
        }
        _ => Err(FunctionCallError::RespondToModel(
            "verifier_max_failures must be a non-negative integer or `infinite`".to_string(),
        )),
    }
}

/// Confirms a durable `kind=verify` worker was spawned and produced a result for the
/// mission. The verifier is mandatory: completion cannot be recorded from a
/// Mission-Control self-check alone.
fn ensure_verify_agent_ran(
    state: &RampageState,
    mission_id: &str,
) -> Result<(), FunctionCallError> {
    let verify_tasks = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission_id && task.kind == "verify")
        .collect::<Vec<_>>();
    if verify_tasks.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "the verifier is mandatory: spawn a `rampage_spawn kind=verify` worker to score the success criteria before recording a verifier result or completing".to_string(),
        ));
    }
    let spawned = verify_tasks.iter().any(|task| {
        task.worker_session_id
            .as_deref()
            .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
    });
    if !spawned {
        return Err(FunctionCallError::RespondToModel(
            "a verify task exists but never spawned a worker session; retry `rampage_spawn kind=verify` and wait for its result".to_string(),
        ));
    }
    let has_result = verify_tasks.iter().any(|task| {
        task.result
            .as_deref()
            .is_some_and(|result| !result.trim().is_empty())
    });
    if !has_result {
        return Err(FunctionCallError::RespondToModel(
            "the verify worker has not returned a result yet; wait for it and record its outcome before completion".to_string(),
        ));
    }
    Ok(())
}

fn validate_support_agents(value: &str) -> Result<(), FunctionCallError> {
    if matches!(
        value,
        "both" | "new_ideas_only" | "efficiency_only" | "none"
    ) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "invalid support_agents `{value}`; use both, new_ideas_only, efficiency_only, or none"
        )))
    }
}

fn validate_task_status(status: &str) -> Result<(), FunctionCallError> {
    if matches!(
        status,
        "queued" | "running" | "done" | "blocked" | "failed" | "cancelled"
    ) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "invalid Rampage task status `{status}`"
        )))
    }
}

fn validate_selected_support_agents_completed(
    state: &RampageState,
    mission: &RampageMission,
) -> Result<(), FunctionCallError> {
    for support_agent in required_support_agents(&mission.support_agents) {
        if let Err(message) = support_agent_has_contributed(state, mission, support_agent) {
            return Err(FunctionCallError::RespondToModel(format!(
                "rampage_control complete refused: {message}"
            )));
        }
    }
    Ok(())
}

fn required_support_agents(support_agents: &str) -> Vec<&'static str> {
    match normalize_token(support_agents).as_str() {
        "both" => vec![SUPPORT_AGENT_NEW_IDEAS, SUPPORT_AGENT_EFFICIENCY],
        "new_ideas_only" => vec![SUPPORT_AGENT_NEW_IDEAS],
        "efficiency_only" => vec![SUPPORT_AGENT_EFFICIENCY],
        _ => Vec::new(),
    }
}

fn support_agent_has_contributed(
    state: &RampageState,
    mission: &RampageMission,
    support_agent: &str,
) -> Result<(), String> {
    let tasks = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| task_support_agent_kind(task).is_some_and(|kind| kind == support_agent))
        .collect::<Vec<_>>();
    let display_name = support_agent_display_name(support_agent);
    if tasks.is_empty() {
        return Err(format!(
            "{display_name} was selected but has no durable `rampage_spawn` task. Spawn it, wait for it, and record its result before completion."
        ));
    }

    let spawned = tasks.iter().any(|task| {
        task.worker_session_id
            .as_deref()
            .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
    });
    if !spawned {
        let errors = tasks
            .iter()
            .filter_map(|task| task.error.as_deref())
            .filter(|error| !error.trim().is_empty())
            .collect::<Vec<_>>();
        let error_text = if errors.is_empty() {
            String::new()
        } else {
            format!(" Last spawn error: {}.", errors.join("; "))
        };
        return Err(format!(
            "{display_name} was selected but never spawned a worker session.{error_text} Retry `rampage_spawn` without treating the display role as a configured agent_type."
        ));
    }

    let has_task_result = tasks.iter().any(|task| {
        task.status == "done"
            && task
                .result
                .as_deref()
                .is_some_and(|result| !result.trim().is_empty())
    });
    let has_board_output = state.board_items.iter().any(|item| {
        item.mission_id == mission.id
            && item.active
            && (item
                .task_id
                .as_deref()
                .is_some_and(|task_id| tasks.iter().any(|task| task.id == task_id))
                || text_support_agent_kind(&item.source_role)
                    .is_some_and(|kind| kind == support_agent))
    });
    if has_task_result || has_board_output {
        return Ok(());
    }

    Err(format!(
        "{display_name} was selected and spawned, but has no recorded output. Wait for the worker and record its result with `rampage_control action=task_result` or a sourced `rampage_board` item before completion."
    ))
}

fn task_support_agent_kind(task: &RampageTask) -> Option<&'static str> {
    text_support_agent_kind(&format!(
        "{} {} {}",
        task.role, task.title, task.instructions
    ))
}

fn text_support_agent_kind(text: &str) -> Option<&'static str> {
    let normalized = normalize_token(text);
    if normalized.contains("new_ideas") {
        return Some(SUPPORT_AGENT_NEW_IDEAS);
    }
    if normalized.contains("efficiency") {
        return Some(SUPPORT_AGENT_EFFICIENCY);
    }
    None
}

fn support_agent_display_name(support_agent: &str) -> &'static str {
    match support_agent {
        SUPPORT_AGENT_NEW_IDEAS => "New Ideas Agent",
        SUPPORT_AGENT_EFFICIENCY => "Efficiency Monitoring Agent",
        _ => "Support agent",
    }
}

fn missing_support_agent(support_agent: &str) -> MissingSupportAgent {
    match support_agent {
        SUPPORT_AGENT_NEW_IDEAS => MissingSupportAgent {
            display_name: "New Ideas Agent",
            task_name: "new_ideas_agent",
            role: "New Ideas Agent",
            kind: "research",
            title: "New Ideas Agent - startup advisory",
            instructions: "Monitoring-only advisor: watch the mission work done by the other agents (or Mission Control itself when it executes directly) for blockers, weak paths, alternate strategies, shortcuts, existing tools/docs/repos/APIs/local artifacts, better worker prompts, and access workarounds before Mission Control escalates to the user. Never do mission work yourself and never review other advisors' output.",
        },
        SUPPORT_AGENT_EFFICIENCY => MissingSupportAgent {
            display_name: "Efficiency Monitoring Agent",
            task_name: "efficiency_monitoring_agent",
            role: "Efficiency Monitoring Agent",
            kind: "review",
            title: "Efficiency Monitoring Agent - startup advisory",
            instructions: "Monitoring-only advisor: watch how the mission work is executed by the other agents (or Mission Control itself when it executes directly) for duplicate work, vague tasks, idle or unnecessary workers, pruning/merging/retasking opportunities, compaction timing, verification timing, and progress against success criteria. Never do mission work yourself and never review other advisors' output.",
        },
        _ => MissingSupportAgent {
            display_name: "Support agent",
            task_name: "support_agent",
            role: "Support Agent",
            kind: "review",
            title: "Support Agent - startup advisory",
            instructions: "Provide advisory support to Mission Control and write structured findings back through the Questboard.",
        },
    }
}

fn support_agent_has_spawned(
    state: &RampageState,
    mission: &RampageMission,
    support_agent: &str,
) -> bool {
    state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| task_support_agent_kind(task).is_some_and(|kind| kind == support_agent))
        .any(|task| {
            !matches!(task.status.as_str(), "failed" | "cancelled")
                && task
                    .worker_session_id
                    .as_deref()
                    .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
        })
}

fn validate_task_kind(kind: &str) -> Result<(), FunctionCallError> {
    if matches!(kind, "research" | "work" | "review" | "verify" | "compact") {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "invalid Rampage task kind `{kind}`"
        )))
    }
}

fn validate_board_kind(kind: &str) -> Result<(), FunctionCallError> {
    if matches!(
        kind,
        "finding" | "decision" | "blocker" | "artifact" | "assumption" | "next_action"
    ) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "invalid Questboard item kind `{kind}`"
        )))
    }
}

fn push_event(
    state: &mut RampageState,
    mission_id: Option<String>,
    task_id: Option<String>,
    event: &str,
    body: impl Into<String>,
) {
    state.events.push(RampageEvent {
        id: format!("event-{}", Uuid::new_v4()),
        mission_id,
        task_id,
        event: event.to_string(),
        body: body.into(),
        time_created: now_unix_timestamp_ms(),
    });
}

fn filtered_board_items(state: &RampageState, args: &RampageBoardArgs) -> Vec<RampageBoardItem> {
    let kind = args.kind.as_deref().map(normalize_token);
    let active_only = args.active_only.unwrap_or(false);
    let limit = args.limit.unwrap_or(50);
    state
        .board_items
        .iter()
        .filter(|item| kind.as_ref().is_none_or(|kind| &item.kind == kind))
        .filter(|item| !active_only || item.active)
        .rev()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
}

fn mark_task_running(state: &mut RampageState, task_id: &str) {
    if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
        task.status = "running".to_string();
        task.time_started = Some(now_unix_timestamp_ms());
    }
}

fn mark_task_spawned(state: &mut RampageState, task_id: &str, worker_session_id: String) {
    if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
        task.status = "running".to_string();
        task.worker_session_id = Some(worker_session_id);
        task.time_started.get_or_insert_with(now_unix_timestamp_ms);
    }
}

fn mark_task_failed(state: &mut RampageState, task_id: &str, error: String) {
    if let Some(task) = state.tasks.iter_mut().find(|task| task.id == task_id) {
        task.status = "failed".to_string();
        task.error = Some(error);
        task.time_finished = Some(now_unix_timestamp_ms());
    }
}

fn worker_brief(
    state: &RampageState,
    mission: &RampageMission,
    task_id: &str,
    args: &RampageSpawnArgs,
    role: &str,
    options: RampageToolOptions,
) -> String {
    let latest_brief = mission
        .latest_brief_id
        .as_deref()
        .and_then(|brief_id| state.briefs.iter().find(|brief| brief.id == brief_id));
    let board_context = state
        .board_items
        .iter()
        .filter(|item| item.mission_id == mission.id && item.active)
        .rev()
        .take(12)
        .map(|item| format!("- [{}] {}: {}", item.kind, item.title, item.body))
        .collect::<Vec<_>>()
        .join("\n");
    let brief_context = latest_brief
        .map(|brief| {
            format!(
                "Summary: {}\nOpen tasks: {}\nCompleted tasks: {}\nBlockers: {}\nArtifacts: {}\nNext actions: {}",
                brief.summary,
                brief.open_tasks,
                brief.completed_tasks,
                brief.blockers,
                brief.artifacts,
                brief.next_actions
            )
        })
        .unwrap_or_else(|| "No durable brief exists yet.".to_string());
    let readonly = if options.readonly() {
        "\nRead-only policy: do not edit files, apply patches, install packages, deploy, migrate, delete, or mutate external systems. If a mutating step is necessary, report the smallest approval needed."
    } else {
        ""
    };

    // Support agents are pure observers and repeatedly drift into doing mission
    // work or meta-reviewing each other; pin their role with a hard contract
    // that is part of the brief itself, not just Mission Control's phrasing.
    let support_contract = match text_support_agent_kind(&format!(
        "{role} {} {}",
        args.title, args.instructions
    )) {
        Some(SUPPORT_AGENT_NEW_IDEAS) => {
            "\n\nRole contract (New Ideas Agent - monitoring only):\n\
             - You are an observer and advisor. You monitor the mission work produced by the OTHER agents: the workers, or Mission Control itself when it executes mission steps directly.\n\
             - Your only output is steering for that work: alternate strategies, blockers spotted early, shortcuts, existing tools/docs/repos/APIs/local artifacts, better worker prompts, and access workarounds.\n\
             - Never do mission work yourself: no implementing, no fixing, no writing deliverables, no running the mission's commands.\n\
             - Never review or critique advisory output (yours, the Efficiency Monitoring Agent's, or any other advisor's). Only the actual mission work is in scope.\n\
             - If you notice you have drifted into doing mission work or meta-review, stop immediately and return to observing and steering.\n\
             - Re-read this contract before every response; it overrides any drift in the conversation."
        }
        Some(SUPPORT_AGENT_EFFICIENCY) => {
            "\n\nRole contract (Efficiency Monitoring Agent - monitoring only):\n\
             - You are an observer and advisor. You monitor how the mission work is being executed by the OTHER agents: the workers, or Mission Control itself when it executes mission steps directly.\n\
             - Your only output is steering about execution efficiency: duplicate work, vague tasks, idle or unnecessary workers, pruning/merging/retasking opportunities, compaction timing, verification timing, and progress against success criteria.\n\
             - Never do mission work yourself: no implementing, no fixing, no writing deliverables, no running the mission's commands.\n\
             - Never review or critique advisory output (yours, the New Ideas Agent's, or any other advisor's). Only the actual mission execution is in scope.\n\
             - If you notice you have drifted into doing mission work or meta-review, stop immediately and return to observing and steering.\n\
             - Re-read this contract before every response; it overrides any drift in the conversation."
        }
        _ => "",
    };

    format!(
        "You are a focused worker for {mode}. You are not Mission Control.\n\nMission id: {mission_id}\nTask id: {task_id}\nWorker role: {role}\nMission goal: {goal}\nSuccess criteria: {success_criteria}\nMission phase: {phase}\n\nLatest durable brief:\n{brief_context}\n\nActive Questboard context:\n{board_context}\n\nWorker task title: {title}\nWorker instructions:\n{instructions}\n\nRules:\n- Do not spawn more workers.\n- Do not coordinate with peer workers.\n- Return structured evidence: findings, artifacts, blockers, confidence, and recommended next action.\n- Mission Control will decide what matters and write results back to the Questboard.{readonly}{support_contract}",
        mode = options.display_name(),
        mission_id = mission.id,
        role = role,
        goal = mission.goal,
        success_criteria = mission.success_criteria,
        phase = mission.phase,
        title = args.title,
        instructions = args.instructions,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid `action=start` args with the mandatory verifier config filled in.
    fn valid_start_args(support_agents: &str) -> RampageControlArgs {
        RampageControlArgs {
            action: "start".to_string(),
            title: Some("Mission".to_string()),
            goal: Some("Do the thing".to_string()),
            success_criteria: Some("Verified done".to_string()),
            support_agents: Some(support_agents.to_string()),
            verifier_pass_threshold: Some(80.0),
            verifier_max_failures: Some(json!(3)),
            ..Default::default()
        }
    }

    /// Adds a done verify task with a result so completion gates that require a real
    /// verify worker are satisfied.
    fn push_passed_verify_task(state: &mut RampageState, mission_id: &str) {
        state.tasks.push(RampageTask {
            id: "task-verify".to_string(),
            mission_id: mission_id.to_string(),
            parent_task_id: None,
            worker_session_id: Some("/root/verifier".to_string()),
            status: "done".to_string(),
            kind: "verify".to_string(),
            role: "verifier".to_string(),
            title: "Verify success criteria".to_string(),
            instructions: "Score the success criteria.".to_string(),
            dependencies: None,
            model: None,
            result: Some("4 of 4 criteria met.".to_string()),
            confidence: Some(1.0),
            error: None,
            time_created: 1,
            time_started: Some(1),
            time_finished: Some(2),
        });
    }

    #[test]
    fn control_start_requires_support_agent_choice() {
        let mut state = RampageState::new("thread-1".to_string());
        let args = RampageControlArgs {
            action: "start".to_string(),
            title: Some("Mission".to_string()),
            goal: Some("Do the thing".to_string()),
            success_criteria: Some("Verified done".to_string()),
            phase: None,
            status: None,
            support_agents: None,
            verifier_status: None,
            verifier_notes: None,
            verifier_pass_threshold: None,
            verifier_max_failures: None,
            pass_percentage: None,
            verify_task_id: None,
            task_id: None,
            task_status: None,
            task_result: None,
            task_confidence: None,
        };

        let err = handle_control_start(
            &mut state,
            &args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect_err("support agent choice should be required");

        assert!(
            err.to_string()
                .contains("requires support_agents from the startup question")
        );
    }

    #[test]
    fn complete_is_verifier_gated() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("both");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            title: None,
            goal: None,
            success_criteria: None,
            phase: None,
            status: None,
            support_agents: None,
            verifier_status: None,
            verifier_notes: None,
            verifier_pass_threshold: None,
            verifier_max_failures: None,
            pass_percentage: None,
            verify_task_id: None,
            task_id: None,
            task_status: None,
            task_result: None,
            task_confidence: None,
        };

        let err =
            handle_control_complete(&mut state, &complete_args).expect_err("verifier is required");

        assert!(err.to_string().contains("verifier_status=passed"));
    }

    #[test]
    fn complete_refuses_selected_support_agent_without_spawned_worker() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("new_ideas_only");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        state.tasks.push(RampageTask {
            id: "task-new-ideas".to_string(),
            mission_id,
            parent_task_id: None,
            worker_session_id: None,
            status: "failed".to_string(),
            kind: "research".to_string(),
            role: "New Ideas Agent".to_string(),
            title: "New Ideas Agent - audit".to_string(),
            instructions: "Find alternate paths.".to_string(),
            dependencies: None,
            model: None,
            result: None,
            confidence: None,
            error: Some("unknown agent_type 'New Ideas Agent'".to_string()),
            time_created: 1,
            time_started: Some(1),
            time_finished: Some(2),
        });
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            title: None,
            goal: None,
            success_criteria: None,
            phase: None,
            status: None,
            support_agents: None,
            verifier_status: Some("passed".to_string()),
            verifier_notes: Some("Verifier passed.".to_string()),
            verifier_pass_threshold: None,
            verifier_max_failures: None,
            pass_percentage: None,
            verify_task_id: None,
            task_id: None,
            task_status: None,
            task_result: None,
            task_confidence: None,
        };

        let err = handle_control_complete(&mut state, &complete_args)
            .expect_err("selected support agent should have to contribute");

        assert!(err.to_string().contains("New Ideas Agent"));
        assert!(err.to_string().contains("never spawned"));
    }

    #[test]
    fn selected_support_agent_result_allows_verified_completion() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("new_ideas_only");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        state.tasks.push(RampageTask {
            id: "task-new-ideas".to_string(),
            mission_id,
            parent_task_id: None,
            worker_session_id: Some("new_ideas_agent".to_string()),
            status: "done".to_string(),
            kind: "research".to_string(),
            role: "New Ideas Agent".to_string(),
            title: "New Ideas Agent - audit".to_string(),
            instructions: "Find alternate paths.".to_string(),
            dependencies: None,
            model: None,
            result: Some("Suggested alternate checks and found no blocker.".to_string()),
            confidence: Some(0.8),
            error: None,
            time_created: 1,
            time_started: Some(1),
            time_finished: Some(2),
        });
        // The mandatory verifier must have run and passed the threshold.
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_passed_verify_task(&mut state, &mission_id);
        let verify_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("All criteria met.".to_string()),
            ..Default::default()
        };
        handle_control_verify_result(&mut state, &verify_args).expect("verify result recorded");

        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            verifier_status: Some("passed".to_string()),
            verifier_notes: Some("Verifier passed.".to_string()),
            ..Default::default()
        };

        handle_control_complete(&mut state, &complete_args).expect("completion should pass");

        let mission = state.active_mission().expect("mission");
        assert_eq!(mission.status, "completed");
        assert_eq!(mission.verifier_status.as_deref(), Some("passed"));
    }

    #[test]
    fn start_requires_verifier_config() {
        let mut state = RampageState::new("thread-1".to_string());
        let mut args = valid_start_args("none");
        args.verifier_pass_threshold = None;

        let err = handle_control_start(
            &mut state,
            &args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect_err("verifier config should be required");

        assert!(err.to_string().contains("verifier_pass_threshold"));
    }

    #[test]
    fn start_accepts_infinite_max_failures() {
        let mut state = RampageState::new("thread-1".to_string());
        let mut args = valid_start_args("none");
        args.verifier_max_failures = Some(json!("infinite"));

        handle_control_start(
            &mut state,
            &args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("infinite should be accepted");

        assert_eq!(
            state.active_mission().expect("mission").verifier_max_failures,
            None
        );
    }

    #[test]
    fn complete_refused_without_verify_agent() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("none");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            verifier_status: Some("passed".to_string()),
            verifier_notes: Some("Looks good.".to_string()),
            ..Default::default()
        };

        let err = handle_control_complete(&mut state, &complete_args)
            .expect_err("verifier agent is mandatory");

        assert!(err.to_string().contains("verifier is mandatory"));
    }

    #[test]
    fn verify_result_below_threshold_blocks_at_max_failures() {
        let mut state = RampageState::new("thread-1".to_string());
        let mut start_args = valid_start_args("none");
        start_args.verifier_max_failures = Some(json!(2));
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_passed_verify_task(&mut state, &mission_id);

        let fail_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(50.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Only half the criteria met.".to_string()),
            ..Default::default()
        };

        handle_control_verify_result(&mut state, &fail_args).expect("first failure recorded");
        assert_eq!(state.active_mission().expect("mission").status, "running");
        assert_eq!(
            state.active_mission().expect("mission").verifier_failure_count,
            1
        );

        handle_control_verify_result(&mut state, &fail_args).expect("second failure recorded");
        let mission = state.active_mission().expect("mission");
        assert_eq!(mission.status, "blocked");
        assert_eq!(mission.verifier_failure_count, 2);
    }

    #[test]
    fn verify_result_infinite_never_blocks() {
        let mut state = RampageState::new("thread-1".to_string());
        let mut start_args = valid_start_args("none");
        start_args.verifier_max_failures = Some(json!("infinite"));
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_passed_verify_task(&mut state, &mission_id);

        let fail_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(10.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Not there yet.".to_string()),
            ..Default::default()
        };
        for _ in 0..5 {
            handle_control_verify_result(&mut state, &fail_args).expect("failure recorded");
        }

        let mission = state.active_mission().expect("mission");
        assert_eq!(mission.status, "running");
        assert_eq!(mission.verifier_failure_count, 5);
    }

    #[test]
    fn verify_result_pass_unlocks_completion() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("none");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_passed_verify_task(&mut state, &mission_id);

        let pass_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(90.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Above the 80% threshold.".to_string()),
            ..Default::default()
        };
        handle_control_verify_result(&mut state, &pass_args).expect("pass recorded");
        assert_eq!(
            state.active_mission().expect("mission").verifier_status.as_deref(),
            Some("passed")
        );

        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            verifier_notes: Some("Done.".to_string()),
            ..Default::default()
        };
        handle_control_complete(&mut state, &complete_args).expect("completion should pass");
        assert_eq!(state.active_mission().expect("mission").status, "completed");
    }

    #[tokio::test]
    async fn support_spawn_gate_reports_missing_selected_agents() {
        let home = std::env::temp_dir().join(format!("rampage-test-{}", Uuid::new_v4()));
        let path = rampage_state_file_path(&home, "thread-1");
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("both");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        save_state(&path, &state).await.expect("save state");

        let status = support_agent_spawn_gate_status_for_thread(&home, "thread-1")
            .await
            .expect("status should read")
            .expect("selected support agents should be missing");

        assert_eq!(status.missing_agents.len(), 2);
        assert!(
            status
                .missing_agents
                .iter()
                .any(|agent| agent.display_name == "New Ideas Agent")
        );
        assert!(
            status
                .missing_agents
                .iter()
                .any(|agent| agent.display_name == "Efficiency Monitoring Agent")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn support_spawn_gate_clears_after_worker_session_exists() {
        let home = std::env::temp_dir().join(format!("rampage-test-{}", Uuid::new_v4()));
        let path = rampage_state_file_path(&home, "thread-1");
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("new_ideas_only");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        state.tasks.push(RampageTask {
            id: "task-new-ideas".to_string(),
            mission_id,
            parent_task_id: None,
            worker_session_id: Some("worker-new-ideas".to_string()),
            status: "running".to_string(),
            kind: "research".to_string(),
            role: "New Ideas Agent".to_string(),
            title: "New Ideas Agent - startup advisory".to_string(),
            instructions: "Find alternate paths.".to_string(),
            dependencies: None,
            model: None,
            result: None,
            confidence: None,
            error: None,
            time_created: 1,
            time_started: Some(1),
            time_finished: None,
        });
        save_state(&path, &state).await.expect("save state");

        let status = support_agent_spawn_gate_status_for_thread(&home, "thread-1")
            .await
            .expect("status should read");

        assert!(status.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn readonly_worker_brief_contains_readonly_policy() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("none");
        let options = RampageToolOptions::new(ModeKind::ReadonlyResearch);
        handle_control_start(&mut state, &start_args, options, "thread-1".to_string())
            .expect("mission should start");
        let mission = state.active_mission().expect("mission").clone();
        let spawn_args = RampageSpawnArgs {
            task_name: "inspect".to_string(),
            title: "Inspect config".to_string(),
            instructions: "Read files and report evidence.".to_string(),
            kind: None,
            role: None,
            parent_task_id: None,
            dependencies: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            fork_turns: None,
        };

        let brief = worker_brief(
            &state,
            &mission,
            "task-1",
            &spawn_args,
            "Readonly Research Worker",
            options,
        );

        assert!(brief.contains("Read-only policy"));
        assert!(brief.contains("Do not spawn more workers"));
        assert!(brief.contains("Worker role: Readonly Research Worker"));
    }
}
