use crate::compact::collect_user_messages;
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
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;
use tracing::warn;
use uuid::Uuid;

const RAMPAGE_DIR: &str = "rampage";
const RAMPAGE_CONTROL_TOOL: &str = "rampage_control";
const RAMPAGE_BOARD_TOOL: &str = "rampage_board";
const RAMPAGE_COMPACT_TOOL: &str = "rampage_compact";
const RAMPAGE_SPAWN_TOOL: &str = "rampage_spawn";
const RAMPAGE_CHECKPOINT_TOOL: &str = "rampage_checkpoint";
const SUPPORT_AGENT_NEW_IDEAS: &str = "new_ideas";
const SUPPORT_AGENT_EFFICIENCY: &str = "efficiency";
const RESULT_TASK_LIMIT: usize = 8;
const RESULT_BOARD_LIMIT: usize = 8;
const RESULT_BRIEF_LIMIT: usize = 1;
const RESULT_EVENT_LIMIT: usize = 8;
const BOARD_LIST_LIMIT_DEFAULT: usize = 8;
const BOARD_LIST_LIMIT_MAX: usize = 12;
const OUTPUT_MISSION_TEXT_LIMIT: usize = 2_000;
const OUTPUT_TASK_TEXT_LIMIT: usize = 500;
const OUTPUT_BOARD_TEXT_LIMIT: usize = 500;
const OUTPUT_BRIEF_TEXT_LIMIT: usize = 500;
const OUTPUT_EVENT_TEXT_LIMIT: usize = 300;
const STORED_TASK_RESULT_LIMIT: usize = 12_000;
const ADVISOR_ACTIVE_TASK_LIMIT: usize = 8;
const ADVISOR_TERMINAL_TASK_LIMIT: usize = 6;
const VERIFIER_EVIDENCE_TASK_LIMIT: usize = 12;
const REVIEWED_LEGACY_EVIDENCE_ID_LIMIT: usize = 64;
const CHECKPOINT_TEXT_LIMIT: usize = 2_000;
const CHECKPOINT_DETAIL_LIMIT: usize = 1_000;
const ORPHAN_RECONCILE_GRACE_MS: i64 = 60_000;
const ATTESTATION_WRITE_ATTEMPTS: usize = 3;

type RampageStateTransactionMutex = AsyncMutex<()>;
type RampageStateTransactionRegistry = BTreeMap<PathBuf, Weak<RampageStateTransactionMutex>>;

static RAMPAGE_STATE_TRANSACTION_MUTEXES: OnceLock<StdMutex<RampageStateTransactionRegistry>> =
    OnceLock::new();

fn rampage_state_transaction_mutex(path: &Path) -> Arc<RampageStateTransactionMutex> {
    let registry = RAMPAGE_STATE_TRANSACTION_MUTEXES.get_or_init(|| StdMutex::new(BTreeMap::new()));
    let mut registry = match registry.lock() {
        Ok(registry) => registry,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(transaction_mutex) = registry.get(path).and_then(Weak::upgrade) {
        return transaction_mutex;
    }

    registry.retain(|_, transaction_mutex| transaction_mutex.strong_count() > 0);
    let transaction_mutex = Arc::new(AsyncMutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&transaction_mutex));
    transaction_mutex
}

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

pub(crate) async fn incomplete_mission_status_for_thread(
    codex_home: &Path,
    root_thread_id: &str,
) -> Result<Option<ActiveRampageMissionStatus>, String> {
    mission_status_for_thread(codex_home, root_thread_id, true).await
}

pub(crate) async fn current_mission_id_for_thread(
    codex_home: &Path,
    root_thread_id: &str,
) -> Result<Option<String>, String> {
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
    Ok(state.active_mission().map(|mission| mission.id.clone()))
}

async fn mission_status_for_thread(
    codex_home: &Path,
    root_thread_id: &str,
    include_inactive_incomplete: bool,
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
    let mission = if include_inactive_incomplete {
        state.active_incomplete_mission()
    } else {
        state.active_running_mission()
    };
    let Some(mission) = mission else {
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

#[derive(Clone, Default)]
pub(crate) struct RampageCheckpointHandler;

impl RampageCheckpointHandler {
    pub(crate) fn new() -> Self {
        Self
    }
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

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
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

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
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

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
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

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for RampageCheckpointHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RAMPAGE_CHECKPOINT_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        create_rampage_checkpoint_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let result = handle_rampage_checkpoint(invocation).await?;
        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for RampageCheckpointHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
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
            !mission.requires_user_resume
                && matches!(mission.status.as_str(), "running" | "blocked" | "verifying")
        })
    }

    fn active_incomplete_mission(&self) -> Option<&RampageMission> {
        self.active_mission()
            .filter(|mission| !matches!(mission.status.as_str(), "completed" | "stopped"))
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
    /// Durable verify task whose output produced the recorded verifier result.
    #[serde(default)]
    verifier_task_id: Option<String>,
    /// Verifier task ids already consumed as bounded verification rounds.
    #[serde(default)]
    consumed_verifier_task_ids: Vec<String>,
    /// Last authenticated verifier round, used to carry bounded continuity into
    /// the next evidence window.
    #[serde(default)]
    verifier_continuity: Option<RampageVerifierContinuity>,
    /// A goal or criteria revision after a reviewed round requires newer
    /// substantive worker evidence before another verifier can be accepted.
    #[serde(default)]
    fresh_worker_evidence_required_after_revision: Option<u64>,
    /// Monotonic revision of worker/advisory evidence and mission criteria.
    #[serde(default)]
    evidence_revision: u64,
    /// Evidence revision injected into each durable worker when it was spawned.
    #[serde(default)]
    task_input_revisions: BTreeMap<String, u64>,
    /// Evidence revision assigned when each authenticated task result was recorded.
    #[serde(default)]
    task_result_revisions: BTreeMap<String, u64>,
    /// Exact spawned thread UUID for each task, independent of its transient agent path.
    #[serde(default)]
    worker_thread_ids: BTreeMap<String, String>,
    /// Latest authenticated progress checkpoint for each substantive worker task.
    #[serde(default)]
    worker_checkpoints: BTreeMap<String, RampageWorkerCheckpoint>,
    /// A verifier-limit escalation cannot be bypassed until a newer user message resumes it.
    #[serde(default)]
    requires_user_resume: bool,
    #[serde(default)]
    user_message_at_resume_block: Option<String>,
    latest_brief_id: Option<String>,
    time_created: i64,
    time_updated: i64,
    time_completed: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageVerifierContinuity {
    verify_task_id: String,
    reviewed_through_revision: u64,
    pass_percentage: f64,
    notes: String,
    #[serde(default)]
    reviewed_evidence_task_ids: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageWorkerAttestation {
    schema_version: u32,
    mission_id: String,
    task_id: String,
    worker_thread_id: String,
    terminal_status: String,
    output: String,
    time_completed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RampageWorkerCheckpoint {
    revision: u64,
    attempt: u64,
    checkpoint: String,
    blocker: Option<String>,
    next_action: String,
    time_updated: i64,
}

#[derive(Debug, Deserialize, PartialEq)]
struct VerifierWorkerVerdict {
    pass_percentage: f64,
    notes: String,
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

#[derive(Debug, Serialize)]
struct RampageCheckpointAck {
    ok: bool,
}

impl ToolOutput for RampageCheckpointAck {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, RAMPAGE_CHECKPOINT_TOOL)
    }

    fn success_for_logging(&self) -> bool {
        self.ok
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            self,
            Some(self.ok),
            RAMPAGE_CHECKPOINT_TOOL,
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, RAMPAGE_CHECKPOINT_TOOL)
    }
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
    stop_reason: Option<String>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RampageCheckpointArgs {
    attempt: u64,
    checkpoint: String,
    blocker: Option<String>,
    next_action: String,
}

async fn handle_rampage_control(
    invocation: ToolInvocation,
    options: RampageToolOptions,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageControlArgs =
        parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let _state_transaction = rampage_state_transaction_mutex(&path).lock_owned().await;
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let action = normalize_token(&args.action);
    let latest_user_message = if matches!(action.as_str(), "stop" | "resume" | "verify_result") {
        let history = invocation.session.clone_history().await;
        collect_user_messages(history.raw_items())
            .into_iter()
            .next_back()
    } else {
        None
    };
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
        "resume" => {
            handle_control_resume(&mut state, latest_user_message.as_deref())?;
        }
        "stop" => {
            handle_control_stop(&mut state, &args, latest_user_message.as_deref())?;
        }
        "complete" => {
            handle_control_complete(&mut state, &args)?;
        }
        "task_result" => {
            let worker_result = verified_worker_task_result(&state, &args, &invocation).await?;
            handle_control_task_result(&mut state, &args, worker_result)?;
        }
        "verify_result" => {
            handle_control_verify_result(&mut state, &args, latest_user_message.as_deref())?;
        }
        _ => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unsupported rampage_control action `{}`; use start, status, update, resume, stop, task_result, verify_result, or complete",
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
    if let Some(existing) = state.active_incomplete_mission() {
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
        verifier_task_id: None,
        consumed_verifier_task_ids: Vec::new(),
        verifier_continuity: None,
        fresh_worker_evidence_required_after_revision: None,
        evidence_revision: 0,
        task_input_revisions: BTreeMap::new(),
        task_result_revisions: BTreeMap::new(),
        worker_thread_ids: BTreeMap::new(),
        worker_checkpoints: BTreeMap::new(),
        requires_user_resume: false,
        user_message_at_resume_block: None,
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
    let current_status = required_active_mission_record(state)?.status.as_str();
    if matches!(current_status, "completed" | "stopped") {
        return Err(FunctionCallError::RespondToModel(
            "terminal Rampage missions are immutable; start a new mission instead of reopening one"
                .to_string(),
        ));
    }
    if args.verifier_status.is_some() || args.verifier_notes.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control update cannot author verifier fields; record the named verifier worker with action=verify_result"
                .to_string(),
        ));
    }
    let mission_id = {
        let mission = required_active_mission_mut(state)?;
        if mission.requires_user_resume {
            return Err(FunctionCallError::RespondToModel(
                "rampage_control update refused: the verifier failure limit was reached and this mission requires a newer explicit user resume message followed by action=resume"
                    .to_string(),
            ));
        }
        let mut criteria_changed = false;
        if let Some(status) = args.status.as_deref().map(normalize_token) {
            validate_mission_status(&status)?;
            if matches!(status.as_str(), "completed" | "stopped") {
                return Err(FunctionCallError::RespondToModel(
                    "rampage_control update cannot set a terminal mission status; only action=complete may finish a mission after the worker, advisory, and verifier gates pass"
                        .to_string(),
                ));
            }
            mission.status = status;
        }
        if let Some(goal) = nonempty(args.goal.as_deref())
            && goal != mission.goal
        {
            mission.goal = goal.to_string();
            criteria_changed = true;
        }
        if let Some(success_criteria) = nonempty(args.success_criteria.as_deref())
            && success_criteria != mission.success_criteria
        {
            mission.success_criteria = success_criteria.to_string();
            criteria_changed = true;
        }
        if criteria_changed {
            mission.evidence_revision = mission.evidence_revision.saturating_add(1);
            if mission.verifier_continuity.is_some() {
                mission.fresh_worker_evidence_required_after_revision =
                    Some(mission.evidence_revision);
            }
            mission.verifier_status = None;
            mission.verifier_task_id = None;
        }
        if let Some(phase) = nonempty(args.phase.as_deref()) {
            mission.phase = phase.to_string();
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

fn handle_control_resume(
    state: &mut RampageState,
    latest_user_message: Option<&str>,
) -> Result<(), FunctionCallError> {
    let mission = required_active_mission_record(state)?;
    if !mission.requires_user_resume {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control resume refused: this mission is not waiting for user authorization"
                .to_string(),
        ));
    }
    let latest_user_message = latest_user_message
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .filter(|message| {
            mission.user_message_at_resume_block.as_deref() != Some(*message)
                && explicit_user_resume_request(message)
        })
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "rampage_control resume refused: wait for a newer real user message that explicitly says resume, continue, retry, proceed, or keep going"
                    .to_string(),
            )
        })?;
    let now = now_unix_timestamp_ms();
    let mission_id = {
        let mission = required_active_mission_mut(state)?;
        mission.requires_user_resume = false;
        mission.user_message_at_resume_block = None;
        mission.status = "running".to_string();
        mission.phase = "resumed_by_user".to_string();
        mission.verifier_status = None;
        mission.verifier_failure_count = 0;
        mission.time_updated = now;
        mission.id.clone()
    };
    push_event(
        state,
        Some(mission_id),
        None,
        "mission_resumed_by_user",
        latest_user_message,
    );
    Ok(())
}

fn handle_control_stop(
    state: &mut RampageState,
    args: &RampageControlArgs,
    latest_user_message: Option<&str>,
) -> Result<(), FunctionCallError> {
    let stop_reason = required_string(args.stop_reason.as_deref(), "stop_reason")?;
    let latest_user_message =
        latest_user_message.filter(|message| explicit_user_stop_request(message));
    if latest_user_message.is_none() {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control stop refused: the latest real user message is not an explicit stop request. Never stop a mission from controller-authored text, task difficulty, or verifier failure."
                .to_string(),
        ));
    }
    let current_status = required_active_mission_record(state)?.status.as_str();
    if matches!(current_status, "completed" | "stopped") {
        return Err(FunctionCallError::RespondToModel(
            "terminal Rampage missions are immutable; there is no active mission to stop"
                .to_string(),
        ));
    }
    let mission_id = required_active_mission_record(state)?.id.clone();
    ensure_no_active_mission_tasks(state, &mission_id).map_err(|_| {
        FunctionCallError::RespondToModel(
            "rampage_control stop refused while durable workers are queued or running. Interrupt each live worker, record its terminal task_result as cancelled/failed/blocked, then retry stop while the same explicit user stop request remains the latest user message."
                .to_string(),
        )
    })?;
    let now = now_unix_timestamp_ms();
    let mission_id = {
        let mission = required_active_mission_mut(state)?;
        mission.status = "stopped".to_string();
        mission.phase = "stopped_by_user".to_string();
        mission.time_updated = now;
        mission.time_completed = Some(now);
        mission.id.clone()
    };
    push_event(
        state,
        Some(mission_id),
        None,
        "mission_stopped_by_user",
        stop_reason,
    );
    Ok(())
}

fn handle_control_complete(
    state: &mut RampageState,
    args: &RampageControlArgs,
) -> Result<(), FunctionCallError> {
    let mission = required_active_mission_record(state)?.clone();
    ensure_completed_mission_worker_ran(state, &mission.id)?;
    validate_selected_support_agents_completed(state, &mission)?;
    ensure_no_active_mission_tasks(state, &mission.id)?;

    let verifier_status = mission.verifier_status.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "rampage_control complete refused: the verifier is mandatory and no recorded verifier result exists; call action=verify_result first"
                .to_string(),
        )
    })?;
    if !matches!(verifier_status.as_str(), "passed" | "complete" | "verified") {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control complete refused: verifier_status must be passed, complete, or verified".to_string(),
        ));
    }
    if let Some(requested_status) = args.verifier_status.as_deref().map(normalize_token)
        && requested_status != verifier_status
    {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control complete refused: verifier_status does not match the authoritative recorded verifier result"
                .to_string(),
        ));
    }
    let verifier_notes = mission
        .verifier_notes
        .clone()
        .filter(|notes| !notes.trim().is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "rampage_control complete refused: the authoritative verifier returned no notes"
                    .to_string(),
            )
        })?;
    if let Some(requested_notes) = args
        .verifier_notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        && requested_notes != verifier_notes
    {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control complete refused: verifier_notes do not match the authoritative verifier output"
                .to_string(),
        ));
    }

    // The verifier is mandatory and threshold-gated: a real verify worker must have
    // run, and its recorded pass percentage must meet the mission's threshold.
    let verify_task_id = mission.verifier_task_id.as_deref().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "rampage_control complete refused: the verifier is mandatory and no durable verify task is tied to the recorded verifier result; call action=verify_result with verify_task_id"
                .to_string(),
        )
    })?;
    let verify_task = ensure_verify_agent_ran(state, &mission, verify_task_id)?;
    let verdict = parse_verifier_worker_verdict(verify_task)?;
    if mission.verifier_pass_percentage != Some(verdict.pass_percentage)
        || mission.verifier_notes.as_deref() != Some(verdict.notes.as_str())
    {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control complete refused: recorded verifier fields do not match the authoritative verifier task output; record action=verify_result again"
                .to_string(),
        ));
    }
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
    worker_result: String,
) -> Result<(), FunctionCallError> {
    let task_id = required_string(args.task_id.as_deref(), "task_id")?;
    required_string(args.task_result.as_deref(), "task_result")?;
    let active_mission_id = required_active_mission_record(state)?.id.clone();
    let status = args
        .task_status
        .as_deref()
        .map(normalize_token)
        .unwrap_or_else(|| "done".to_string());
    validate_task_status(&status)?;
    let worker_result = bounded_storage_text(&worker_result, STORED_TASK_RESULT_LIMIT);
    let now = now_unix_timestamp_ms();
    let (mission_id, task_id, task_title, task_role, task_kind) = {
        let task = state
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!("unknown rampage task `{task_id}`"))
            })?;
        if task.mission_id != active_mission_id {
            return Err(FunctionCallError::RespondToModel(format!(
                "rampage task `{task_id}` belongs to an older mission and is immutable"
            )));
        }
        if task.time_finished.is_some()
            || matches!(
                task.status.as_str(),
                "done" | "blocked" | "failed" | "cancelled"
            )
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "rampage task `{task_id}` already has a terminal result and cannot be rewritten; spawn a fresh durable task for another round"
            )));
        }
        task.status = status.clone();
        if matches!(status.as_str(), "failed" | "cancelled") {
            task.result = None;
            task.error = Some(worker_result.clone());
        } else {
            task.result = Some(worker_result.clone());
            task.error = None;
        }
        task.confidence = args.task_confidence;
        task.time_finished = Some(now);
        (
            task.mission_id.clone(),
            task.id.clone(),
            task.title.clone(),
            task.role.clone(),
            task.kind.clone(),
        )
    };
    if task_kind != "verify" {
        let mission = required_active_mission_mut(state)?;
        mission.evidence_revision = mission.evidence_revision.saturating_add(1);
        let result_revision = mission.evidence_revision;
        mission
            .task_result_revisions
            .insert(task_id.clone(), result_revision);
        mission.verifier_status = None;
        mission.verifier_task_id = None;
        mission.time_updated = now;
    }
    state.board_items.push(RampageBoardItem {
        id: format!("board-{}", Uuid::new_v4()),
        mission_id: mission_id.clone(),
        task_id: Some(task_id.clone()),
        kind: if matches!(status.as_str(), "blocked" | "failed" | "cancelled") {
            "blocker".to_string()
        } else {
            "finding".to_string()
        },
        title: format!("{task_title} result"),
        body: format!(
            "Authenticated task output for `{task_id}` (full bounded output is stored on the durable task): {}",
            text_for_worker_brief(&worker_result, OUTPUT_BOARD_TEXT_LIMIT)
        ),
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

async fn verified_worker_task_result(
    state: &RampageState,
    args: &RampageControlArgs,
    invocation: &ToolInvocation,
) -> Result<String, FunctionCallError> {
    let task_id = required_string(args.task_id.as_deref(), "task_id")?;
    let active_mission_id = required_active_mission_record(state)?.id.as_str();
    let requested_status = args
        .task_status
        .as_deref()
        .map(normalize_token)
        .unwrap_or_else(|| "done".to_string());
    validate_task_status(&requested_status)?;
    if matches!(requested_status.as_str(), "queued" | "running") {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control task_result only accepts a terminal task status".to_string(),
        ));
    }
    let task = state
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!("unknown rampage task `{task_id}`"))
        })?;
    if task.mission_id != active_mission_id {
        return Err(FunctionCallError::RespondToModel(format!(
            "rampage task `{task_id}` does not belong to the current mutable mission"
        )));
    }
    if task.time_finished.is_some()
        || matches!(
            task.status.as_str(),
            "done" | "blocked" | "failed" | "cancelled"
        )
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "rampage task `{task_id}` already has a terminal result and cannot be rewritten"
        )));
    }
    let worker_reference = task
        .worker_session_id
        .as_deref()
        .filter(|worker_session_id| !worker_session_id.trim().is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "rampage task `{task_id}` has no spawned worker session"
            ))
        })?;
    let expected_worker_thread_id = required_active_mission_record(state)?
        .worker_thread_ids
        .get(&task_id)
        .cloned();
    let worker_id = invocation
        .session
        .services
        .agent_control
        .resolve_agent_reference(
            invocation.session.thread_id,
            &invocation.turn.session_source,
            worker_reference,
        )
        .await;
    if let Ok(worker_id) = worker_id {
        if let Some(expected) = expected_worker_thread_id.as_deref()
            && worker_id.to_string() != expected
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "resolved worker UUID `{worker_id}` does not match durable Rampage binding `{expected}` for task `{task_id}`"
            )));
        }
        let worker_status = invocation
            .session
            .services
            .agent_control
            .get_status(worker_id)
            .await;
        if !matches!(&worker_status, AgentStatus::NotFound) {
            return authoritative_worker_result(&worker_status, &requested_status).map_err(
                |message| {
                    FunctionCallError::RespondToModel(format!(
                        "rampage task `{task_id}` cannot record a result yet: {message}"
                    ))
                },
            );
        }
    }

    let expected_worker_thread_id = expected_worker_thread_id.ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "rampage task `{task_id}` has no durable worker UUID and its live agent path is unavailable"
        ))
    })?;
    let attestation_path =
        rampage_attestation_file_path(&rampage_state_path(invocation), &task.mission_id, &task_id);
    let attestation = load_worker_attestation(&attestation_path)
        .await
        .map_err(|message| {
            FunctionCallError::RespondToModel(format!(
                "rampage task `{task_id}` has no live worker and no valid terminal attestation: {message}"
            ))
        })?;
    if attestation.mission_id != task.mission_id
        || attestation.task_id != task_id
        || attestation.worker_thread_id != expected_worker_thread_id
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "worker attestation binding mismatch for Rampage task `{task_id}`"
        )));
    }
    authoritative_attested_worker_result(&attestation, &requested_status).map_err(|message| {
        FunctionCallError::RespondToModel(format!(
            "rampage task `{task_id}` cannot record its attested result: {message}"
        ))
    })
}

fn authoritative_worker_result(
    worker_status: &AgentStatus,
    requested_status: &str,
) -> Result<String, String> {
    match (worker_status, requested_status) {
        (AgentStatus::Completed(Some(result)), "done" | "blocked") if !result.trim().is_empty() => {
            Ok(result.trim().to_string())
        }
        (AgentStatus::Errored(error), "failed") if !error.trim().is_empty() => {
            Ok(error.trim().to_string())
        }
        (AgentStatus::Interrupted | AgentStatus::Shutdown, "cancelled") => {
            Ok(format!("worker ended with status {worker_status:?}"))
        }
        (AgentStatus::Completed(None), "done" | "blocked")
        | (AgentStatus::Completed(Some(_)), "done" | "blocked") => {
            Err("the worker completed without a non-empty final result".to_string())
        }
        (AgentStatus::PendingInit | AgentStatus::Running, _) => Err(format!(
            "the worker is still {worker_status:?}; wait for its final status"
        )),
        (AgentStatus::NotFound, _) => {
            Err("the spawned worker is not present in the live agent registry".to_string())
        }
        _ => Err(format!(
            "worker status {worker_status:?} does not match requested task status `{requested_status}`"
        )),
    }
}

fn authoritative_attested_worker_result(
    attestation: &RampageWorkerAttestation,
    requested_status: &str,
) -> Result<String, String> {
    match (attestation.terminal_status.as_str(), requested_status) {
        ("completed", "done" | "blocked") if !attestation.output.trim().is_empty() => {
            Ok(attestation.output.trim().to_string())
        }
        ("failed", "failed") if !attestation.output.trim().is_empty() => {
            Ok(attestation.output.trim().to_string())
        }
        ("lost", "failed") if !attestation.output.trim().is_empty() => {
            Ok(attestation.output.trim().to_string())
        }
        ("interrupted" | "shutdown", "cancelled") => Ok(attestation.output.trim().to_string()),
        (terminal, requested) => Err(format!(
            "attested worker status `{terminal}` does not match requested task status `{requested}`"
        )),
    }
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
    latest_user_message: Option<&str>,
) -> Result<(), FunctionCallError> {
    let reported_pass_percentage = args.pass_percentage.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "rampage_control verify_result requires pass_percentage (0-100), the fraction of success criteria the verifier found met".to_string(),
        )
    })?;
    if !(0.0..=100.0).contains(&reported_pass_percentage) {
        return Err(FunctionCallError::RespondToModel(
            "pass_percentage must be between 0 and 100".to_string(),
        ));
    }
    required_string(args.verifier_notes.as_deref(), "verifier_notes")?;

    // Verification is downstream of real worker execution and fresh advisory review.
    let mission = required_active_mission_record(state)?.clone();
    if mission.requires_user_resume {
        return Err(FunctionCallError::RespondToModel(
            "rampage_control verify_result refused: the verifier failure limit was reached; wait for a newer explicit user resume message and call action=resume"
                .to_string(),
        ));
    }
    ensure_completed_mission_worker_ran(state, &mission.id)?;
    validate_selected_support_agents_completed(state, &mission)?;
    ensure_no_active_mission_tasks(state, &mission.id)?;
    let verify_task_id = required_string(args.verify_task_id.as_deref(), "verify_task_id")?;
    if mission
        .consumed_verifier_task_ids
        .iter()
        .any(|task_id| task_id == &verify_task_id)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{verify_task_id}` was already consumed as a verification round; spawn a fresh kind=verify task"
        )));
    }
    let verify_task = ensure_verify_agent_ran(state, &mission, &verify_task_id)?;
    let verdict = parse_verifier_worker_verdict(verify_task)?;
    if (reported_pass_percentage - verdict.pass_percentage).abs() > f64::EPSILON {
        return Err(FunctionCallError::RespondToModel(format!(
            "reported pass_percentage {reported_pass_percentage:.0}% does not match authoritative verifier task `{verify_task_id}` output {:.0}%",
            verdict.pass_percentage
        )));
    }
    let pass_percentage = verdict.pass_percentage;
    let verifier_notes = verdict.notes;
    let reviewed_through_revision = mission
        .task_input_revisions
        .get(&verify_task_id)
        .copied()
        .unwrap_or(mission.evidence_revision);
    let mut reviewed_evidence_task_ids = mission
        .verifier_continuity
        .as_ref()
        .map(|continuity| {
            continuity
                .reviewed_evidence_task_ids
                .iter()
                .filter(|task_id| !mission.task_result_revisions.contains_key(*task_id))
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    reviewed_evidence_task_ids.extend(
        verifier_evidence_tasks(state, &mission, Some(&verify_task_id))
            .into_iter()
            .filter(|task| !mission.task_result_revisions.contains_key(&task.id))
            .map(|task| task.id.clone()),
    );
    let reviewed_evidence_task_ids = reviewed_evidence_task_ids
        .into_iter()
        .take(REVIEWED_LEGACY_EVIDENCE_ID_LIMIT)
        .collect::<Vec<_>>();
    let mission_id = mission.id;

    let now = now_unix_timestamp_ms();
    let escalate;
    {
        let mission = required_active_mission_mut(state)?;
        mission.verifier_pass_percentage = Some(pass_percentage);
        mission.verifier_notes = Some(verifier_notes.clone());
        mission.verifier_task_id = Some(verify_task_id.clone());
        mission
            .consumed_verifier_task_ids
            .push(verify_task_id.clone());
        mission.verifier_continuity = Some(RampageVerifierContinuity {
            verify_task_id: verify_task_id.clone(),
            reviewed_through_revision,
            pass_percentage,
            notes: bounded_storage_text(&verifier_notes, STORED_TASK_RESULT_LIMIT),
            reviewed_evidence_task_ids,
        });
        mission.fresh_worker_evidence_required_after_revision = None;
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
                mission.phase = "awaiting_user_resume".to_string();
                mission.requires_user_resume = true;
                mission.user_message_at_resume_block = latest_user_message.map(str::to_string);
            }
        }
    }

    let (kind, title, body) = if pass_percentage
        >= required_active_mission_record(state)?.verifier_pass_threshold
    {
        (
            "finding",
            "Verifier passed".to_string(),
            format!(
                "Verifier reported {pass_percentage:.0}% of success criteria met. {verifier_notes}"
            ),
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
            format!(
                "Verifier reported {pass_percentage:.0}% (below threshold). Write missing work and continue. {verifier_notes}"
            ),
        )
    };
    state.board_items.push(RampageBoardItem {
        id: format!("board-{}", Uuid::new_v4()),
        mission_id: mission_id.clone(),
        task_id: Some(verify_task_id.clone()),
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
        Some(verify_task_id),
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
    let _state_transaction = rampage_state_transaction_mutex(&path).lock_owned().await;
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let action = normalize_token(&args.action);
    match action.as_str() {
        "add" => {
            let mission_id = required_active_mission(&state)?.id.clone();
            if let Some(source_role) = nonempty(args.source_role.as_deref())
                && normalize_token(source_role) != "mission_control"
            {
                return Err(FunctionCallError::RespondToModel(
                    "rampage_board add cannot impersonate a worker or verifier; controller-authored rows always use source_role=Mission Control, while authenticated worker rows come from rampage_control task_result"
                        .to_string(),
                ));
            }
            if let Some(task_id) = nonempty(args.task_id.as_deref()) {
                let valid_task = state
                    .tasks
                    .iter()
                    .any(|task| task.id == task_id && task.mission_id == mission_id);
                if !valid_task {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "rampage_board task_id `{task_id}` does not belong to the active mission"
                    )));
                }
            }
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
                body: bounded_storage_text(
                    &required_string(args.body.as_deref(), "body")?,
                    STORED_TASK_RESULT_LIMIT,
                ),
                source_role: "Mission Control".to_string(),
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
    if action == "list" {
        result.tasks.clear();
        result.briefs.clear();
        result.events.clear();
    }
    Ok(result)
}

async fn handle_rampage_compact(
    invocation: ToolInvocation,
) -> Result<RampageResult, FunctionCallError> {
    let args: RampageCompactArgs =
        parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    let path = rampage_state_path(&invocation);
    let _state_transaction = rampage_state_transaction_mutex(&path).lock_owned().await;
    let mut state = load_state(&path, invocation.session.thread_id.to_string()).await?;
    let mission_id = required_active_mission(&state)?.id.clone();
    let brief_id = format!("brief-{}", Uuid::new_v4());
    let brief = RampageBrief {
        id: brief_id.clone(),
        mission_id: mission_id.clone(),
        summary: bounded_storage_text(&args.summary, STORED_TASK_RESULT_LIMIT),
        open_tasks: bounded_storage_text(&args.open_tasks, STORED_TASK_RESULT_LIMIT),
        completed_tasks: bounded_storage_text(&args.completed_tasks, STORED_TASK_RESULT_LIMIT),
        blockers: bounded_storage_text(&args.blockers, STORED_TASK_RESULT_LIMIT),
        artifacts: bounded_storage_text(&args.artifacts, STORED_TASK_RESULT_LIMIT),
        next_actions: bounded_storage_text(&args.next_actions, STORED_TASK_RESULT_LIMIT),
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
    let _state_transaction = rampage_state_transaction_mutex(&path).lock_owned().await;
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
    let requested_support_agent = spawn_args_support_agent_kind(&args);
    if let Some(support_agent) = requested_support_agent
        && !required_support_agents(&mission.support_agents).contains(&support_agent)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{} was not enabled for this mission's support_agents selection",
            support_agent_display_name(support_agent)
        )));
    }
    validate_spawn_contract(&args, &task_kind, requested_support_agent)?;
    if let Some(support_agent) = requested_support_agent {
        ensure_support_agent_spawn_has_worker_evidence(&state, &mission, support_agent)?;
    }
    ensure_evidence_window_capacity_for_spawn(
        &state,
        &mission,
        &task_kind,
        requested_support_agent,
    )?;
    if task_kind == "verify" {
        ensure_completed_mission_worker_ran(&state, &mission.id)?;
        validate_selected_support_agents_completed(&state, &mission)?;
        ensure_no_active_mission_tasks(&state, &mission.id)?;
        ensure_required_new_substantive_evidence(&state, &mission)?;
        ensure_verifier_evidence_coverage(&state, &mission, &task_id)?;
    }
    if let Some(parent_task_id) = nonempty(args.parent_task_id.as_deref()) {
        let valid_parent = state.tasks.iter().any(|task| {
            task.id == parent_task_id && task.mission_id == mission.id && task.kind != "verify"
        });
        if !valid_parent {
            return Err(FunctionCallError::RespondToModel(format!(
                "parent_task_id `{parent_task_id}` is not a non-verifier task in the current mission"
            )));
        }
    }
    let role = if task_kind == "verify" {
        "Verifier".to_string()
    } else if let Some(support_agent) = requested_support_agent {
        support_agent_display_name(support_agent).to_string()
    } else {
        args.role
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
            .to_string()
    };
    let task = RampageTask {
        id: task_id.clone(),
        mission_id: mission.id.clone(),
        parent_task_id: args.parent_task_id.clone(),
        worker_session_id: None,
        status: "queued".to_string(),
        kind: task_kind.clone(),
        role: role.clone(),
        title: args.title.clone(),
        instructions: bounded_storage_text(&args.instructions, STORED_TASK_RESULT_LIMIT),
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
    if let Some(active_mission) = state.active_mission_mut() {
        active_mission
            .task_input_revisions
            .insert(task_id.clone(), mission.evidence_revision);
    }
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
    if let Err(err) = save_state(&path, &state).await {
        mark_task_failed(
            &mut state,
            &task_id,
            "worker was not spawned because running task state could not be saved".to_string(),
        );
        if let Err(compensation_err) = save_state_with_retry(&path, &state).await {
            warn!("failed to persist pre-spawn Rampage compensation: {compensation_err}");
        }
        return Err(err);
    }

    let task_name_suffix = task_id.rsplit('-').next().unwrap_or(task_id.as_str());
    let unique_task_name = format!("{}-{task_name_suffix}", args.task_name);
    let internal_agent_role = if requested_support_agent.is_some() {
        "rampage-advisor"
    } else if task_kind == "verify" {
        "rampage-verifier"
    } else if options.readonly() {
        "rampage-readonly-worker"
    } else {
        "rampage-worker"
    };
    let spawn_args = SpawnAgentArgs {
        message,
        task_name: unique_task_name,
        agent_type: None,
        model: args.model,
        reasoning_effort: args.reasoning_effort,
        service_tier: args.service_tier,
        fork_turns: Some("none".to_string()),
        fork_context: None,
        internal_agent_role: Some(internal_agent_role.to_string()),
        force_read_only: options.readonly()
            || requested_support_agent.is_some()
            || task_kind == "verify",
        workspace_write_denied_path: (!options.readonly()
            && requested_support_agent.is_none()
            && task_kind != "verify")
            .then(|| invocation.turn.config.codex_home.join(RAMPAGE_DIR)),
    };
    let agent_control = invocation.session.services.agent_control.clone();
    match spawn_agent_with_args(invocation, spawn_args).await {
        Ok(spawn_result) => {
            let worker_thread_id = spawn_result.thread_id();
            let worker_session_id = spawn_result.task_name().to_string();
            mark_task_spawned(&mut state, &task_id, worker_session_id.clone());
            if let Some(active_mission) = state.active_mission_mut() {
                active_mission
                    .worker_thread_ids
                    .insert(task_id.clone(), worker_thread_id.to_string());
            }
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
                Some(mission.id.clone()),
                Some(task_id.clone()),
                "task_updated",
                format!("worker spawned: {worker_session_id}"),
            );
            let status_rx = match agent_control.subscribe_status(worker_thread_id).await {
                Ok(status_rx) => status_rx,
                Err(err) => {
                    let _ = agent_control.interrupt_agent(worker_thread_id).await;
                    let attestation = lost_worker_attestation(
                        &mission.id,
                        &task_id,
                        worker_thread_id,
                        &format!(
                            "worker interrupted because terminal status subscription failed: {err}"
                        ),
                    );
                    let attestation_path =
                        rampage_attestation_file_path(&path, &mission.id, &task_id);
                    if let Err(attestation_err) =
                        save_worker_attestation_with_retry(&attestation_path, &attestation).await
                    {
                        warn!(
                            "failed to persist Rampage subscription-failure attestation: {attestation_err}"
                        );
                    }
                    mark_task_failed(
                        &mut state,
                        &task_id,
                        format!("failed to subscribe to authenticated worker status: {err}"),
                    );
                    save_state_with_retry(&path, &state).await?;
                    return Err(FunctionCallError::RespondToModel(format!(
                        "spawned worker `{worker_session_id}` was interrupted because Rampage could not establish terminal attestation: {err}"
                    )));
                }
            };
            if let Err(err) = save_state(&path, &state).await {
                let _ = agent_control.interrupt_agent(worker_thread_id).await;
                let attestation = lost_worker_attestation(
                    &mission.id,
                    &task_id,
                    worker_thread_id,
                    "worker interrupted because its durable UUID binding could not be saved",
                );
                let attestation_path = rampage_attestation_file_path(&path, &mission.id, &task_id);
                if let Err(attestation_err) =
                    save_worker_attestation_with_retry(&attestation_path, &attestation).await
                {
                    warn!(
                        "failed to persist Rampage state-save-failure attestation: {attestation_err}"
                    );
                }
                mark_task_failed(
                    &mut state,
                    &task_id,
                    "spawned worker was interrupted because its durable binding could not be saved"
                        .to_string(),
                );
                if let Err(compensation_err) = save_state_with_retry(&path, &state).await {
                    warn!("failed to persist Rampage spawn compensation: {compensation_err}");
                }
                return Err(err);
            }
            tokio::spawn(persist_worker_attestation_when_terminal(
                status_rx,
                rampage_attestation_file_path(&path, &mission.id, &task_id),
                mission.id.clone(),
                task_id.clone(),
                worker_thread_id,
            ));
            Ok(result_from_state(
                true,
                format!("rampage_spawn created worker `{worker_session_id}`"),
                &path,
                &state,
            ))
        }
        Err(err) => {
            mark_task_failed(&mut state, &task_id, err.to_string());
            save_state_with_retry(&path, &state).await?;
            Err(err)
        }
    }
}

async fn handle_rampage_checkpoint(
    invocation: ToolInvocation,
) -> Result<RampageCheckpointAck, FunctionCallError> {
    let args: RampageCheckpointArgs =
        parse_arguments(&function_arguments(invocation.payload.clone())?)?;
    if args.attempt == 0 {
        return Err(FunctionCallError::RespondToModel(
            "rampage_checkpoint attempt must be at least 1".to_string(),
        ));
    }
    let checkpoint = required_string(Some(&args.checkpoint), "checkpoint")?;
    let next_action = required_string(Some(&args.next_action), "next_action")?;
    let (root_thread_id, internal_role) =
        checkpoint_source_binding(&invocation.turn.session_source)?;
    let path = rampage_state_file_path(
        invocation.turn.config.codex_home.as_path(),
        &root_thread_id.to_string(),
    );
    let _state_transaction = rampage_state_transaction_mutex(&path).lock_owned().await;
    let mut state = load_state(&path, root_thread_id.to_string()).await?;
    if state.root_thread_id != root_thread_id.to_string() {
        return Err(FunctionCallError::RespondToModel(
            "Rampage checkpoint parent-thread binding does not match durable state".to_string(),
        ));
    }

    let mission = required_active_mission_record(&state)?.clone();
    let worker_thread_id = invocation.session.thread_id.to_string();
    let task_id =
        authenticated_checkpoint_task_id(&state, &mission, &worker_thread_id, internal_role)?;

    let blocker = nonempty(args.blocker.as_deref())
        .map(|value| bounded_storage_text(value, CHECKPOINT_DETAIL_LIMIT));
    let checkpoint = bounded_storage_text(&checkpoint, CHECKPOINT_TEXT_LIMIT);
    let next_action = bounded_storage_text(&next_action, CHECKPOINT_DETAIL_LIMIT);
    let duplicate = mission
        .worker_checkpoints
        .get(&task_id)
        .is_some_and(|existing| {
            existing.attempt == args.attempt
                && existing.checkpoint == checkpoint
                && existing.blocker == blocker
                && existing.next_action == next_action
        });
    if duplicate {
        return Ok(RampageCheckpointAck { ok: true });
    }
    if mission
        .worker_checkpoints
        .get(&task_id)
        .is_some_and(|existing| args.attempt < existing.attempt)
    {
        return Err(FunctionCallError::RespondToModel(
            "rampage_checkpoint attempt cannot move backwards".to_string(),
        ));
    }

    let now = now_unix_timestamp_ms();
    let revision = {
        let active_mission = required_active_mission_mut(&mut state)?;
        active_mission.evidence_revision = active_mission.evidence_revision.saturating_add(1);
        let revision = active_mission.evidence_revision;
        active_mission.worker_checkpoints.insert(
            task_id.clone(),
            RampageWorkerCheckpoint {
                revision,
                attempt: args.attempt,
                checkpoint,
                blocker,
                next_action,
                time_updated: now,
            },
        );
        active_mission.verifier_status = None;
        active_mission.verifier_task_id = None;
        active_mission.time_updated = now;
        revision
    };
    push_event(
        &mut state,
        Some(mission.id),
        Some(task_id),
        "worker_checkpoint",
        format!("authenticated worker checkpoint revision {revision}"),
    );
    save_state(&path, &state).await?;
    Ok(RampageCheckpointAck { ok: true })
}

fn authenticated_checkpoint_task_id(
    state: &RampageState,
    mission: &RampageMission,
    worker_thread_id: &str,
    internal_role: &str,
) -> Result<String, FunctionCallError> {
    let mut matching_tasks = mission
        .worker_thread_ids
        .iter()
        .filter(|(_, bound_thread_id)| bound_thread_id.as_str() == worker_thread_id)
        .map(|(task_id, _)| task_id.clone())
        .collect::<Vec<_>>();
    if matching_tasks.len() != 1 {
        return Err(FunctionCallError::RespondToModel(
            "Rampage checkpoint refused: this worker UUID is not bound to exactly one active mission task"
                .to_string(),
        ));
    }
    let Some(task_id) = matching_tasks.pop() else {
        return Err(FunctionCallError::RespondToModel(
            "Rampage checkpoint refused: the authenticated task binding disappeared".to_string(),
        ));
    };
    let task = state
        .tasks
        .iter()
        .find(|task| task.id == task_id && task.mission_id == mission.id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "Rampage checkpoint task binding is missing from durable state".to_string(),
            )
        })?;
    if !is_substantive_mission_worker(task) || task.status != "running" {
        return Err(FunctionCallError::RespondToModel(
            "Rampage checkpoints are accepted only from a running substantive worker".to_string(),
        ));
    }
    let expected_role = if mission.controller_agent == "readonly-research" {
        "rampage-readonly-worker"
    } else {
        "rampage-worker"
    };
    if internal_role != expected_role {
        return Err(FunctionCallError::RespondToModel(
            "Rampage checkpoint internal role does not match the mission worker role".to_string(),
        ));
    }
    Ok(task_id)
}

fn checkpoint_source_binding(
    source: &SessionSource,
) -> Result<(ThreadId, &str), FunctionCallError> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_role: Some(agent_role),
            ..
        }) if matches!(
            agent_role.as_str(),
            "rampage-worker" | "rampage-readonly-worker"
        ) =>
        {
            Ok((*parent_thread_id, agent_role.as_str()))
        }
        _ => Err(FunctionCallError::RespondToModel(
            "rampage_checkpoint is restricted to trusted substantive Rampage worker sessions"
                .to_string(),
        )),
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
                "resume",
                "stop",
                "task_result",
                "verify_result",
                "complete",
            ]),
            Some("Mission lifecycle action.".to_string()),
        ),
    );
    properties.insert(
        "stop_reason".to_string(),
        JsonSchema::string(Some(
            "Required for action=stop: quote or concise restatement of the user's explicit instruction to stop this mission. Never infer consent from task difficulty or a verifier failure."
                .to_string(),
        )),
    );
    properties.insert(
        "title".to_string(),
        JsonSchema::string(Some("Mission title for action=start.".to_string())),
    );
    properties.insert(
        "goal".to_string(),
        JsonSchema::string(Some(
            "User goal for action=start, or a revised goal for action=update after a new user constraint. Revisions invalidate older verification."
                .to_string(),
        )),
    );
    properties.insert(
        "success_criteria".to_string(),
        JsonSchema::string(Some(
            "Concrete success criteria for action=start, or revised criteria for action=update. Revisions invalidate older verification."
                .to_string(),
        )),
    );
    properties.insert(
        "phase".to_string(),
        JsonSchema::string(Some("Current mission phase.".to_string())),
    );
    properties.insert(
        "status".to_string(),
        JsonSchema::string_enum(
            strings(&["running", "paused", "blocked", "verifying"]),
            Some(
                "Non-terminal mission status for action=update. Only action=complete may finish a mission."
                    .to_string(),
            ),
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
            "verify_result: id of the durable kind=verify task that produced this result."
                .to_string(),
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
            "Controller note for action=task_result. The durable result is taken from the spawned worker's actual final output after its live status is terminal."
                .to_string(),
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
            "Optional value must be Mission Control. Worker and verifier provenance is created only from authenticated task results."
                .to_string(),
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
        JsonSchema::string(Some(
            "Worker role label. Reserve the exact roles New Ideas Agent and Efficiency Monitoring Agent for the selected monitoring advisors."
                .to_string(),
        )),
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
            "Must be none when provided. Rampage workers receive only the durable mission brief, never inherited controller/advisor context."
                .to_string(),
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

fn create_rampage_checkpoint_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "attempt".to_string(),
            JsonSchema::integer(Some(
                "Current worker attempt number, starting at 1 and never decreasing.".to_string(),
            )),
        ),
        (
            "checkpoint".to_string(),
            JsonSchema::string(Some(
                "Concise progress and evidence gathered so far.".to_string(),
            )),
        ),
        (
            "blocker".to_string(),
            JsonSchema::string(Some(
                "Current blocker, when one exists. Omit when unblocked.".to_string(),
            )),
        ),
        (
            "next_action".to_string(),
            JsonSchema::string(Some("Immediate next action.".to_string())),
        ),
    ]);
    function_tool(
        RAMPAGE_CHECKPOINT_TOOL,
        "Persist a bounded authenticated progress checkpoint for this worker's exact durable Rampage task. Returns only an acknowledgement.",
        properties,
        vec![
            "attempt".to_string(),
            "checkpoint".to_string(),
            "next_action".to_string(),
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

fn rampage_attestation_file_path(state_path: &Path, mission_id: &str, task_id: &str) -> PathBuf {
    state_path
        .parent()
        .unwrap_or(state_path)
        .join("attestations")
        .join(mission_id)
        .join(format!("{task_id}.json"))
}

fn terminal_attestation_from_status(
    mission_id: &str,
    task_id: &str,
    worker_thread_id: ThreadId,
    status: &AgentStatus,
) -> Option<RampageWorkerAttestation> {
    let (terminal_status, output) = match status {
        AgentStatus::Completed(result) => (
            "completed",
            result
                .as_deref()
                .unwrap_or("worker completed without output"),
        ),
        AgentStatus::Errored(error) => ("failed", error.as_str()),
        AgentStatus::Interrupted => ("interrupted", "worker was interrupted"),
        AgentStatus::Shutdown => ("shutdown", "worker was shut down"),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::NotFound => return None,
    };
    Some(RampageWorkerAttestation {
        schema_version: 1,
        mission_id: mission_id.to_string(),
        task_id: task_id.to_string(),
        worker_thread_id: worker_thread_id.to_string(),
        terminal_status: terminal_status.to_string(),
        output: bounded_storage_text(output, STORED_TASK_RESULT_LIMIT),
        time_completed: now_unix_timestamp_ms(),
    })
}

async fn persist_worker_attestation_when_terminal(
    mut status_rx: watch::Receiver<AgentStatus>,
    path: PathBuf,
    mission_id: String,
    task_id: String,
    worker_thread_id: ThreadId,
) {
    loop {
        let status = { status_rx.borrow().clone() };
        if let Some(attestation) =
            terminal_attestation_from_status(&mission_id, &task_id, worker_thread_id, &status)
        {
            if let Err(err) = save_worker_attestation_with_retry(&path, &attestation).await {
                warn!(
                    "failed to persist Rampage worker attestation at {}: {err}",
                    path.display()
                );
            }
            return;
        }
        if status_rx.changed().await.is_err() {
            let attestation = lost_worker_attestation(
                &mission_id,
                &task_id,
                worker_thread_id,
                "worker status channel closed before a terminal status was observed",
            );
            if let Err(err) = save_worker_attestation_with_retry(&path, &attestation).await {
                warn!(
                    "failed to persist lost Rampage worker attestation at {}: {err}",
                    path.display()
                );
            }
            return;
        }
    }
}

fn lost_worker_attestation(
    mission_id: &str,
    task_id: &str,
    worker_thread_id: ThreadId,
    reason: &str,
) -> RampageWorkerAttestation {
    RampageWorkerAttestation {
        schema_version: 1,
        mission_id: mission_id.to_string(),
        task_id: task_id.to_string(),
        worker_thread_id: worker_thread_id.to_string(),
        terminal_status: "lost".to_string(),
        output: bounded_storage_text(reason, STORED_TASK_RESULT_LIMIT),
        time_completed: now_unix_timestamp_ms(),
    }
}

async fn save_worker_attestation_with_retry(
    path: &Path,
    attestation: &RampageWorkerAttestation,
) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..ATTESTATION_WRITE_ATTEMPTS {
        match save_worker_attestation(path, attestation).await {
            Ok(()) => return Ok(()),
            Err(err) => last_error = Some(err),
        }
        if attempt + 1 < ATTESTATION_WRITE_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(25 * (1_u64 << attempt))).await;
        }
    }
    Err(last_error.unwrap_or_else(|| "attestation write did not run".to_string()))
}

async fn save_worker_attestation(
    path: &Path,
    attestation: &RampageWorkerAttestation,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("failed to create attestation directory: {err}"))?;
    }
    let contents = serde_json::to_string_pretty(attestation)
        .map_err(|err| format!("failed to serialize attestation: {err}"))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::path_utils::write_atomically(&path, &contents))
        .await
        .map_err(|err| format!("attestation writer failed: {err}"))?
        .map_err(|err| format!("failed to atomically persist attestation: {err}"))
}

async fn load_worker_attestation(path: &Path) -> Result<RampageWorkerAttestation, String> {
    let contents = fs::read_to_string(path)
        .await
        .map_err(|err| format!("failed to read worker attestation: {err}"))?;
    serde_json::from_str(&contents).map_err(|err| format!("invalid worker attestation: {err}"))
}

async fn load_state(
    path: &Path,
    root_thread_id: String,
) -> Result<RampageState, FunctionCallError> {
    match fs::read_to_string(path).await {
        Ok(contents) => {
            let mut state = serde_json::from_str::<RampageState>(&contents).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to parse Rampage state at {}: {err}",
                    path.display()
                ))
            })?;
            if reconcile_unbound_active_tasks(&mut state, now_unix_timestamp_ms()) {
                save_state(path, &state).await?;
            }
            Ok(state)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(RampageState::new(root_thread_id))
        }
        Err(err) => Err(FunctionCallError::RespondToModel(format!(
            "failed to read Rampage state at {}: {err}",
            path.display()
        ))),
    }
}

fn reconcile_unbound_active_tasks(state: &mut RampageState, now: i64) -> bool {
    let Some(mission) = state.active_incomplete_mission().cloned() else {
        return false;
    };
    let mut reconciled = Vec::new();
    for task in state.tasks.iter_mut().filter(|task| {
        task.mission_id == mission.id && matches!(task.status.as_str(), "queued" | "running")
    }) {
        if mission.worker_thread_ids.contains_key(&task.id) {
            continue;
        }
        let aged_out = now.saturating_sub(task.time_started.unwrap_or(task.time_created))
            >= ORPHAN_RECONCILE_GRACE_MS;
        let partial_spawn = task
            .worker_session_id
            .as_deref()
            .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty());
        if !aged_out && !partial_spawn {
            continue;
        }
        task.status = "failed".to_string();
        task.error = Some(
            "Rampage reconciled an active task without a durable worker UUID binding".to_string(),
        );
        task.time_finished = Some(now);
        reconciled.push(task.id.clone());
    }
    if reconciled.is_empty() {
        return false;
    }
    if let Some(active_mission) = state.active_mission_mut() {
        active_mission.time_updated = now;
        active_mission.verifier_status = None;
        active_mission.verifier_task_id = None;
    }
    for task_id in reconciled {
        push_event(
            state,
            Some(mission.id.clone()),
            Some(task_id),
            "task_reconciled",
            "active task had no durable worker UUID and was marked failed",
        );
    }
    true
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
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::path_utils::write_atomically(&path, &contents))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "Rampage state writer failed before persistence: {err}"
            ))
        })?
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to atomically persist Rampage state: {err}"
            ))
        })
}

async fn save_state_with_retry(path: &Path, state: &RampageState) -> Result<(), FunctionCallError> {
    let mut last_error = None;
    for attempt in 0..ATTESTATION_WRITE_ATTEMPTS {
        match save_state(path, state).await {
            Ok(()) => return Ok(()),
            Err(err) => last_error = Some(err.to_string()),
        }
        if attempt + 1 < ATTESTATION_WRITE_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(25 * (1_u64 << attempt))).await;
        }
    }
    Err(FunctionCallError::RespondToModel(
        last_error.unwrap_or_else(|| "Rampage state retry did not run".to_string()),
    ))
}

fn result_from_state(
    ok: bool,
    message: impl Into<String>,
    path: &Path,
    state: &RampageState,
) -> RampageResult {
    let mut mission = state.active_mission().cloned();
    let mission_id = mission.as_ref().map(|mission| mission.id.as_str());
    let mut tasks = state
        .tasks
        .iter()
        .filter(|task| Some(task.mission_id.as_str()) == mission_id)
        .rev()
        .take(RESULT_TASK_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    tasks.reverse();
    let mut board_items = state
        .board_items
        .iter()
        .filter(|item| Some(item.mission_id.as_str()) == mission_id)
        .rev()
        .take(RESULT_BOARD_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    board_items.reverse();
    let mut briefs = state
        .briefs
        .iter()
        .filter(|brief| Some(brief.mission_id.as_str()) == mission_id)
        .rev()
        .take(RESULT_BRIEF_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    briefs.reverse();
    let mut events = state
        .events
        .iter()
        .filter(|event| event.mission_id.as_deref() == mission_id)
        .rev()
        .take(RESULT_EVENT_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    events.reverse();
    if let Some(mission) = mission.as_mut() {
        truncate_string_for_output(&mut mission.goal, OUTPUT_MISSION_TEXT_LIMIT);
        truncate_string_for_output(&mut mission.success_criteria, OUTPUT_MISSION_TEXT_LIMIT);
        if let Some(notes) = mission.verifier_notes.as_mut() {
            truncate_string_for_output(notes, OUTPUT_MISSION_TEXT_LIMIT);
        }
        if let Some(continuity) = mission.verifier_continuity.as_mut() {
            truncate_string_for_output(&mut continuity.notes, OUTPUT_MISSION_TEXT_LIMIT);
            continuity
                .reviewed_evidence_task_ids
                .truncate(REVIEWED_LEGACY_EVIDENCE_ID_LIMIT);
        }
    }
    for task in &mut tasks {
        truncate_string_for_output(&mut task.instructions, OUTPUT_TASK_TEXT_LIMIT);
        if let Some(result) = task.result.as_mut() {
            truncate_string_for_output(result, OUTPUT_TASK_TEXT_LIMIT);
        }
        if let Some(error) = task.error.as_mut() {
            truncate_string_for_output(error, OUTPUT_TASK_TEXT_LIMIT);
        }
    }
    for item in &mut board_items {
        truncate_board_item_for_output(item);
    }
    for brief in &mut briefs {
        for value in [
            &mut brief.summary,
            &mut brief.open_tasks,
            &mut brief.completed_tasks,
            &mut brief.blockers,
            &mut brief.artifacts,
            &mut brief.next_actions,
        ] {
            truncate_string_for_output(value, OUTPUT_BRIEF_TEXT_LIMIT);
        }
    }
    for event in &mut events {
        truncate_string_for_output(&mut event.body, OUTPUT_EVENT_TEXT_LIMIT);
    }
    RampageResult {
        ok,
        message: message.into(),
        state_path: path.display().to_string(),
        mission,
        tasks,
        board_items,
        briefs,
        events,
    }
}

fn truncate_board_item_for_output(item: &mut RampageBoardItem) {
    truncate_string_for_output(&mut item.body, OUTPUT_BOARD_TEXT_LIMIT);
}

fn truncate_string_for_output(value: &mut String, max_chars: usize) {
    if value.chars().count() <= max_chars {
        return;
    }
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push_str("... [truncated; full value is in the durable Rampage state file]");
    *value = truncated;
}

fn text_for_worker_brief(value: &str, max_chars: usize) -> String {
    let mut value = value.to_string();
    truncate_string_for_output(&mut value, max_chars);
    value
}

fn bounded_storage_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut bounded = value.chars().take(max_chars).collect::<String>();
    bounded.push_str("... [truncated at trusted Rampage result ingress]");
    bounded
}

fn escape_verdict_tags(value: &str) -> String {
    value
        .replace("<rampage_verdict>", "<quoted_rampage_verdict>")
        .replace("</rampage_verdict>", "</quoted_rampage_verdict>")
}

fn required_active_mission(state: &RampageState) -> Result<&RampageMission, FunctionCallError> {
    let mission = state.active_mission().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "no active Rampage mission exists; call rampage_control action=start first".to_string(),
        )
    })?;
    if mission.requires_user_resume {
        return Err(FunctionCallError::RespondToModel(
            "Rampage mission is blocked at the user-selected verifier failure limit. Do not spawn more work until a newer explicit user resume message is received and rampage_control action=resume succeeds."
                .to_string(),
        ));
    }
    if matches!(mission.status.as_str(), "running" | "blocked" | "verifying") {
        Ok(mission)
    } else {
        Err(FunctionCallError::RespondToModel(
            "no active Rampage mission exists; call rampage_control action=start first".to_string(),
        ))
    }
}

fn required_active_mission_record(
    state: &RampageState,
) -> Result<&RampageMission, FunctionCallError> {
    state.active_incomplete_mission().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "no mutable Rampage mission exists; terminal missions are immutable, so start a new mission instead"
                .to_string(),
        )
    })
}

fn required_active_mission_mut(
    state: &mut RampageState,
) -> Result<&mut RampageMission, FunctionCallError> {
    let mission_id = state
        .active_incomplete_mission()
        .map(|mission| mission.id.clone())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "no mutable Rampage mission exists; terminal missions are immutable, so start a new mission instead"
                    .to_string(),
            )
        })?;
    state
        .missions
        .iter_mut()
        .find(|mission| mission.id == mission_id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "active Rampage mission state is inconsistent".to_string(),
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

fn explicit_user_stop_request(message: &str) -> bool {
    let normalized = message
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut words = normalized.split_whitespace().collect::<Vec<_>>();
    if words
        .iter()
        .any(|word| matches!(*word, "not" | "dont" | "never" | "continue"))
    {
        return false;
    }
    words.retain(|word| !matches!(*word, "please" | "now"));
    let Some((verb, remainder)) = words.split_first() else {
        return false;
    };
    matches!(*verb, "stop" | "cancel" | "abort" | "end")
        && remainder.iter().all(|word| {
            matches!(
                *word,
                "this"
                    | "the"
                    | "current"
                    | "rampage"
                    | "absolute"
                    | "readonly"
                    | "research"
                    | "mission"
                    | "task"
                    | "work"
            )
        })
}

fn explicit_user_resume_request(message: &str) -> bool {
    let normalized = message
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    if words
        .iter()
        .any(|word| matches!(*word, "not" | "dont" | "never" | "stop" | "cancel"))
    {
        return false;
    }
    words.iter().any(|word| {
        matches!(
            *word,
            "resume" | "continue" | "retry" | "proceed" | "restart"
        )
    }) || words
        .windows(2)
        .any(|pair| matches!(pair, ["keep", "going"] | ["try", "again"] | ["go", "ahead"]))
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
                    "verifier_max_failures must be a non-negative integer or `infinite`"
                        .to_string(),
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

fn task_evidence_was_reviewed(mission: &RampageMission, task: &RampageTask) -> bool {
    let Some(continuity) = mission.verifier_continuity.as_ref() else {
        return false;
    };
    continuity
        .reviewed_evidence_task_ids
        .iter()
        .any(|task_id| task_id == &task.id)
        || mission
            .task_result_revisions
            .get(&task.id)
            .is_some_and(|revision| *revision <= continuity.reviewed_through_revision)
}

fn latest_fresh_support_agent_task<'a>(
    state: &'a RampageState,
    mission: &RampageMission,
    support_agent: &str,
) -> Option<&'a RampageTask> {
    let latest_worker_result_time = latest_substantive_worker_evidence_time(state, mission).ok()?;
    let latest_worker_revision = latest_substantive_worker_evidence_revision(state, mission);
    state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| task_support_agent_kind(task).is_some_and(|kind| kind == support_agent))
        .filter(|task| {
            latest_worker_revision.map_or_else(
                || task.time_created >= latest_worker_result_time,
                |revision| {
                    mission
                        .task_input_revisions
                        .get(&task.id)
                        .is_some_and(|input_revision| *input_revision >= revision)
                },
            )
        })
        .filter(|task| completed_spawned_task_result(task))
        .max_by_key(|task| {
            (
                mission
                    .task_result_revisions
                    .get(&task.id)
                    .copied()
                    .unwrap_or_default(),
                task.time_finished.unwrap_or(task.time_created),
                task.time_created,
            )
        })
}

fn verifier_evidence_tasks<'a>(
    state: &'a RampageState,
    mission: &RampageMission,
    exclude_task_id: Option<&str>,
) -> Vec<&'a RampageTask> {
    let mut evidence = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| exclude_task_id != Some(task.id.as_str()))
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| authoritative_terminal_spawned_task_result(task))
        .filter(|task| !task_evidence_was_reviewed(mission, task))
        .collect::<Vec<_>>();
    for support_agent in required_support_agents(&mission.support_agents) {
        if let Some(task) = latest_fresh_support_agent_task(state, mission, support_agent)
            && exclude_task_id != Some(task.id.as_str())
            && !task_evidence_was_reviewed(mission, task)
        {
            evidence.push(task);
        }
    }
    evidence
}

fn ensure_verifier_evidence_coverage(
    state: &RampageState,
    mission: &RampageMission,
    verify_task_id: &str,
) -> Result<(), FunctionCallError> {
    let eligible = verifier_evidence_tasks(state, mission, Some(verify_task_id)).len();
    if eligible <= VERIFIER_EVIDENCE_TASK_LIMIT {
        return Ok(());
    }
    Err(FunctionCallError::RespondToModel(format!(
        "verify task `{verify_task_id}` cannot be accepted: its unreviewed authenticated evidence window contains {eligible} mandatory results, above the bounded limit of {VERIFIER_EVIDENCE_TASK_LIMIT}. Rampage verification fails closed rather than silently omitting evidence."
    )))
}

fn unreviewed_substantive_window_count(state: &RampageState, mission: &RampageMission) -> usize {
    state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| {
            matches!(task.status.as_str(), "queued" | "running")
                || (authoritative_terminal_spawned_task_result(task)
                    && !task_evidence_was_reviewed(mission, task))
        })
        .count()
}

fn ensure_evidence_window_capacity_for_spawn(
    state: &RampageState,
    mission: &RampageMission,
    task_kind: &str,
    requested_support_agent: Option<&str>,
) -> Result<(), FunctionCallError> {
    if task_kind == "verify" {
        return ensure_verifier_evidence_coverage(state, mission, "pending-verifier");
    }
    let spawning_substantive =
        requested_support_agent.is_none() && matches!(task_kind, "research" | "work" | "review");
    if !spawning_substantive && requested_support_agent.is_none() {
        return Ok(());
    }

    let substantive_slots = unreviewed_substantive_window_count(state, mission);
    let support_slots = required_support_agents(&mission.support_agents).len();
    let projected_slots = substantive_slots
        .saturating_add(support_slots)
        .saturating_add(usize::from(spawning_substantive));
    if projected_slots <= VERIFIER_EVIDENCE_TASK_LIMIT {
        return Ok(());
    }

    Err(FunctionCallError::RespondToModel(format!(
        "rampage_spawn refused: the next bounded verifier window has {substantive_slots} active or unreviewed substantive task(s) and reserves {support_slots} selected support-agent slot(s). Spawning this task would require {projected_slots} slots, above the limit of {VERIFIER_EVIDENCE_TASK_LIMIT}. Run and record a verifier round before adding more substantive work."
    )))
}

fn ensure_required_new_substantive_evidence(
    state: &RampageState,
    mission: &RampageMission,
) -> Result<(), FunctionCallError> {
    let failed_round_revision = mission.verifier_continuity.as_ref().and_then(|continuity| {
        (continuity.pass_percentage < mission.verifier_pass_threshold)
            .then_some(continuity.reviewed_through_revision)
    });
    let required_after_revision = mission
        .fresh_worker_evidence_required_after_revision
        .into_iter()
        .chain(failed_round_revision)
        .max();
    let Some(required_after_revision) = required_after_revision else {
        return Ok(());
    };
    let has_fresh_evidence = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| authoritative_terminal_spawned_task_result(task))
        .any(|task| {
            mission
                .task_result_revisions
                .get(&task.id)
                .is_some_and(|revision| *revision > required_after_revision)
        });
    if has_fresh_evidence {
        return Ok(());
    }

    Err(FunctionCallError::RespondToModel(format!(
        "Rampage verification requires a fresh authenticated substantive work/research/review result after evidence revision {required_after_revision}. A failed prior verdict or revised goal/success criteria cannot be re-verified from continuity alone."
    )))
}

/// Confirms the named verifier was spawned, completed, and ran after every input
/// it was responsible for checking.
fn ensure_verify_agent_ran<'a>(
    state: &'a RampageState,
    mission: &RampageMission,
    verify_task_id: &str,
) -> Result<&'a RampageTask, FunctionCallError> {
    let latest_worker_result_time = latest_substantive_worker_evidence_time(state, mission)?;
    let latest_worker_revision = latest_substantive_worker_evidence_revision(state, mission);
    let mut latest_required_input_time = latest_worker_result_time;
    for support_agent in required_support_agents(&mission.support_agents) {
        let latest_support_result_time = state
            .tasks
            .iter()
            .filter(|task| task.mission_id == mission.id)
            .filter(|task| task_support_agent_kind(task).is_some_and(|kind| kind == support_agent))
            .filter(|task| {
                latest_worker_revision.map_or_else(
                    || task.time_created >= latest_worker_result_time,
                    |revision| {
                        mission
                            .task_input_revisions
                            .get(&task.id)
                            .is_some_and(|input_revision| *input_revision >= revision)
                    },
                )
            })
            .filter(|task| completed_spawned_task_result(task))
            .filter_map(|task| task.time_finished)
            .max()
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "{} has no fresh completed result for the verifier to inspect",
                    support_agent_display_name(support_agent)
                ))
            })?;
        latest_required_input_time = latest_required_input_time.max(latest_support_result_time);
    }

    let verify_task = state
        .tasks
        .iter()
        .find(|task| task.id == verify_task_id)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "unknown verify_task_id `{verify_task_id}`; pass the exact durable task id returned by `rampage_spawn kind=verify`"
            ))
        })?;
    if verify_task.mission_id != mission.id || verify_task.kind != "verify" {
        return Err(FunctionCallError::RespondToModel(format!(
            "task `{verify_task_id}` is not a kind=verify task for mission `{}`",
            mission.id
        )));
    }
    if mission.evidence_revision > 0 {
        let input_revision = mission
            .task_input_revisions
            .get(verify_task_id)
            .copied()
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "verify task `{verify_task_id}` has no authenticated evidence revision; spawn a fresh verifier"
                ))
            })?;
        if input_revision < mission.evidence_revision {
            return Err(FunctionCallError::RespondToModel(format!(
                "verify task `{verify_task_id}` is stale: it saw evidence revision {input_revision}, but the mission is now at revision {}. Spawn a fresh verifier after all worker, advisory, and criteria updates.",
                mission.evidence_revision
            )));
        }
    } else if verify_task.time_created < latest_required_input_time {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{verify_task_id}` is stale: it was created at {} before the latest worker/advisory result at {latest_required_input_time}. Spawn a fresh verifier after all required results.",
            verify_task.time_created
        )));
    }
    ensure_required_new_substantive_evidence(state, mission)?;
    if !completed_spawned_task_result(verify_task) {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{verify_task_id}` has not completed with a spawned worker and recorded final output"
        )));
    }
    ensure_verifier_evidence_coverage(state, mission, verify_task_id)?;
    Ok(verify_task)
}

fn parse_verifier_worker_verdict(
    verify_task: &RampageTask,
) -> Result<VerifierWorkerVerdict, FunctionCallError> {
    const OPEN: &str = "<rampage_verdict>";
    const CLOSE: &str = "</rampage_verdict>";
    let result = verify_task
        .result
        .as_deref()
        .filter(|result| !result.trim().is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(format!(
                "verify task `{}` has no final worker output",
                verify_task.id
            ))
        })?;
    let trimmed = result.trim();
    if trimmed.matches(OPEN).count() != 1
        || trimmed.matches(CLOSE).count() != 1
        || !trimmed.ends_with(CLOSE)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{}` must end with exactly one {OPEN} JSON verdict and no trailing text",
            verify_task.id
        )));
    }
    let open_index = trimmed.rfind(OPEN).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "verify task `{}` did not return the required {OPEN} JSON verdict",
            verify_task.id
        ))
    })?;
    let json_start = open_index + OPEN.len();
    let close_offset = trimmed[json_start..].find(CLOSE).ok_or_else(|| {
        FunctionCallError::RespondToModel(format!(
            "verify task `{}` did not close its {OPEN} JSON verdict",
            verify_task.id
        ))
    })?;
    let json = &trimmed[json_start..json_start + close_offset];
    let verdict = serde_json::from_str::<VerifierWorkerVerdict>(json.trim()).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "verify task `{}` returned invalid verdict JSON: {err}",
            verify_task.id
        ))
    })?;
    if !(0.0..=100.0).contains(&verdict.pass_percentage) {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{}` returned pass_percentage outside 0-100",
            verify_task.id
        )));
    }
    if verdict.notes.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(format!(
            "verify task `{}` returned empty verifier notes",
            verify_task.id
        )));
    }
    Ok(VerifierWorkerVerdict {
        pass_percentage: verdict.pass_percentage,
        notes: verdict.notes.trim().to_string(),
    })
}

fn latest_completed_worker_result_time(
    state: &RampageState,
    mission_id: &str,
) -> Result<i64, FunctionCallError> {
    state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission_id)
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| completed_spawned_task_result(task))
        .filter_map(|task| task.time_finished)
        .max()
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "Rampage verification requires at least one completed, spawned non-support mission worker with a recorded result. Mission Control cannot do the mission work itself; use `rampage_spawn kind=work|research|review`, wait for the worker, and record its result before verification."
                    .to_string(),
            )
        })
}

fn latest_substantive_worker_evidence_time(
    state: &RampageState,
    mission: &RampageMission,
) -> Result<i64, FunctionCallError> {
    let terminal_time = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| authoritative_terminal_spawned_task_result(task))
        .filter_map(|task| task.time_finished)
        .max();
    let checkpoint_time = mission
        .worker_checkpoints
        .iter()
        .filter(|(task_id, _)| {
            state.tasks.iter().any(|task| {
                task.id.as_str() == task_id.as_str()
                    && task.mission_id == mission.id
                    && is_substantive_mission_worker(task)
            })
        })
        .map(|(_, checkpoint)| checkpoint.time_updated)
        .max();
    terminal_time
        .into_iter()
        .chain(checkpoint_time)
        .max()
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "Rampage advisory and verification freshness requires an authenticated checkpoint or terminal result from a spawned non-support worker."
                    .to_string(),
            )
        })
}

fn latest_substantive_worker_evidence_revision(
    state: &RampageState,
    mission: &RampageMission,
) -> Option<u64> {
    let terminal_revision = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| is_substantive_mission_worker(task))
        .filter(|task| authoritative_terminal_spawned_task_result(task))
        .filter_map(|task| mission.task_result_revisions.get(&task.id).copied())
        .max();
    let checkpoint_revision = mission
        .worker_checkpoints
        .iter()
        .filter(|(task_id, _)| {
            state.tasks.iter().any(|task| {
                task.id.as_str() == task_id.as_str()
                    && task.mission_id == mission.id
                    && is_substantive_mission_worker(task)
            })
        })
        .map(|(_, checkpoint)| checkpoint.revision)
        .max();
    terminal_revision
        .into_iter()
        .chain(checkpoint_revision)
        .max()
}

fn completed_spawned_task_result(task: &RampageTask) -> bool {
    task.status == "done"
        && task.time_finished.is_some()
        && task
            .worker_session_id
            .as_deref()
            .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
        && task
            .result
            .as_deref()
            .is_some_and(|result| !result.trim().is_empty())
}

fn authoritative_terminal_spawned_task_result(task: &RampageTask) -> bool {
    matches!(
        task.status.as_str(),
        "done" | "blocked" | "failed" | "cancelled"
    ) && task.time_finished.is_some()
        && task
            .worker_session_id
            .as_deref()
            .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
        && authenticated_task_output(task).is_some()
}

fn authenticated_task_output(task: &RampageTask) -> Option<&str> {
    match task.status.as_str() {
        "failed" | "cancelled" => task.error.as_deref(),
        _ => task.result.as_deref(),
    }
    .filter(|output| !output.trim().is_empty())
}

fn ensure_completed_mission_worker_ran(
    state: &RampageState,
    mission_id: &str,
) -> Result<(), FunctionCallError> {
    if latest_completed_worker_result_time(state, mission_id).is_ok() {
        return Ok(());
    }

    Err(FunctionCallError::RespondToModel(
        "Rampage verification requires at least one completed, spawned non-support mission worker with a recorded result. Mission Control cannot do the mission work itself; use `rampage_spawn kind=work|research|review`, wait for the worker, and record its result before verification."
            .to_string(),
    ))
}

fn ensure_no_active_mission_tasks(
    state: &RampageState,
    mission_id: &str,
) -> Result<(), FunctionCallError> {
    let active = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission_id)
        .filter(|task| matches!(task.status.as_str(), "queued" | "running"))
        .map(|task| format!("{} ({})", task.id, task.title))
        .take(8)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(());
    }
    Err(FunctionCallError::RespondToModel(format!(
        "Rampage verification cannot finish while mission tasks are still active: {}. Wait for each worker and record its terminal result, or explicitly cancel it before verification.",
        active.join(", ")
    )))
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
    let latest_worker_result_time = latest_substantive_worker_evidence_time(state, mission)
        .map_err(|_| {
            format!(
                "{} cannot contribute before a non-support worker has returned an authenticated checkpoint or terminal result. Spawn a focused mission worker first.",
                support_agent_display_name(support_agent)
            )
        })?;
    let all_tasks = state
        .tasks
        .iter()
        .filter(|task| task.mission_id == mission.id)
        .filter(|task| task_support_agent_kind(task).is_some_and(|kind| kind == support_agent))
        .collect::<Vec<_>>();
    let display_name = support_agent_display_name(support_agent);
    if all_tasks.is_empty() {
        return Err(format!(
            "{display_name} was selected but has no durable `rampage_spawn` task created after the latest non-support worker result. Spawn it with the current named worker status/results, wait for it, and record its result before verification."
        ));
    }

    let latest_worker_revision = latest_substantive_worker_evidence_revision(state, mission);
    let tasks = all_tasks
        .iter()
        .copied()
        .filter(|task| {
            latest_worker_revision.map_or_else(
                || task.time_created >= latest_worker_result_time,
                |revision| {
                    mission
                        .task_input_revisions
                        .get(&task.id)
                        .is_some_and(|input_revision| *input_revision >= revision)
                },
            )
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return Err(format!(
            "{display_name}'s existing contribution is stale. Spawn a fresh advisory task after the latest non-support worker result, provide the current named worker status/results, and record the completed advisory result before verification."
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
            && task.time_finished.is_some()
            && task
                .worker_session_id
                .as_deref()
                .is_some_and(|worker_session_id| !worker_session_id.trim().is_empty())
            && task
                .result
                .as_deref()
                .is_some_and(|result| !result.trim().is_empty())
    });
    if has_task_result {
        return Ok(());
    }

    Err(format!(
        "{display_name} was selected and freshly spawned, but has no completed recorded output. Wait for the worker and record its result with `rampage_control action=task_result` before verification."
    ))
}

fn task_support_agent_kind(task: &RampageTask) -> Option<&'static str> {
    role_support_agent_kind(&task.role)
}

fn spawn_args_support_agent_kind(args: &RampageSpawnArgs) -> Option<&'static str> {
    args.role
        .as_deref()
        .and_then(role_support_agent_kind)
        .or_else(|| role_support_agent_kind(&args.task_name))
}

fn validate_spawn_contract(
    args: &RampageSpawnArgs,
    task_kind: &str,
    requested_support_agent: Option<&str>,
) -> Result<(), FunctionCallError> {
    if requested_support_agent.is_some() && task_kind == "verify" {
        return Err(FunctionCallError::RespondToModel(
            "a monitoring support agent cannot also be the independent verifier; spawn separate durable advisory and kind=verify tasks"
                .to_string(),
        ));
    }
    if requested_support_agent.is_some() && !matches!(task_kind, "research" | "review") {
        return Err(FunctionCallError::RespondToModel(
            "monitoring support agents must use kind=research or kind=review".to_string(),
        ));
    }
    if let Some(fork_turns) = nonempty(args.fork_turns.as_deref())
        && normalize_token(fork_turns) != "none"
    {
        return Err(FunctionCallError::RespondToModel(
            "rampage_spawn always uses fork_turns=none so workers receive only the durable mission brief and cannot inherit controller/advisor context"
                .to_string(),
        ));
    }
    Ok(())
}

fn ensure_support_agent_spawn_has_worker_evidence(
    state: &RampageState,
    mission: &RampageMission,
    support_agent: &str,
) -> Result<(), FunctionCallError> {
    latest_substantive_worker_evidence_time(state, mission)
        .map(|_| ())
        .map_err(|_| {
            FunctionCallError::RespondToModel(format!(
                "{} cannot spawn before a substantive non-support worker has returned an authenticated checkpoint or terminal result. Spawn a focused mission worker first, then retry the advisory task with that worker evidence.",
                support_agent_display_name(support_agent)
            ))
        })
}

fn role_support_agent_kind(role: &str) -> Option<&'static str> {
    match normalize_token(role).as_str() {
        "new_ideas" | "new_ideas_agent" => Some(SUPPORT_AGENT_NEW_IDEAS),
        "efficiency" | "efficiency_agent" | "efficiency_monitoring_agent" => {
            Some(SUPPORT_AGENT_EFFICIENCY)
        }
        _ => None,
    }
}

fn is_substantive_mission_worker(task: &RampageTask) -> bool {
    task_support_agent_kind(task).is_none()
        && matches!(task.kind.as_str(), "research" | "work" | "review")
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
            title: "New Ideas Agent - worker review",
            instructions: "Monitoring-only advisor: review the current named non-support worker status/results for blockers, weak paths, alternate strategies, shortcuts, existing tools/docs/repos/APIs/local artifacts, better worker prompts, and access workarounds. Never review Mission Control or advisory output, and never do mission work yourself. If no non-support worker exists, advise Mission Control to spawn one and do nothing else.",
        },
        SUPPORT_AGENT_EFFICIENCY => MissingSupportAgent {
            display_name: "Efficiency Monitoring Agent",
            task_name: "efficiency_monitoring_agent",
            role: "Efficiency Monitoring Agent",
            kind: "review",
            title: "Efficiency Monitoring Agent - worker review",
            instructions: "Monitoring-only advisor: review the current named non-support worker status/results for duplicate work, vague tasks, idle or unnecessary workers, pruning/merging/retasking opportunities, compaction timing, verification timing, and progress against success criteria. Never review Mission Control or advisory output, and never do mission work yourself. If no non-support worker exists, advise Mission Control to spawn one and do nothing else.",
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
    let limit = args
        .limit
        .unwrap_or(BOARD_LIST_LIMIT_DEFAULT)
        .min(BOARD_LIST_LIMIT_MAX);
    let mission_id = state.active_mission_id.as_deref();
    state
        .board_items
        .iter()
        .filter(|item| Some(item.mission_id.as_str()) == mission_id)
        .filter(|item| kind.as_ref().is_none_or(|kind| &item.kind == kind))
        .filter(|item| !active_only || item.active)
        .rev()
        .take(limit)
        .cloned()
        .map(|mut item| {
            truncate_board_item_for_output(&mut item);
            item
        })
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
    let support_agent_kind =
        spawn_args_support_agent_kind(args).or_else(|| role_support_agent_kind(role));
    let verifier = args
        .kind
        .as_deref()
        .is_some_and(|kind| normalize_token(kind) == "verify");
    let latest_brief = mission
        .latest_brief_id
        .as_deref()
        .and_then(|brief_id| state.briefs.iter().find(|brief| brief.id == brief_id));
    let board_context = if support_agent_kind.is_some() {
        "Omitted for advisory workers. Review only the named non-support worker snapshot below."
            .to_string()
    } else if verifier {
        "Omitted for the verifier. Use the canonical authenticated task-result snapshot below, not controller-authored Questboard prose."
            .to_string()
    } else {
        state
            .board_items
            .iter()
            .filter(|item| item.mission_id == mission.id && item.active)
            .rev()
            .take(12)
            .map(|item| {
                format!(
                    "- [{}] {}: {}",
                    item.kind,
                    item.title,
                    text_for_worker_brief(&item.body, OUTPUT_BOARD_TEXT_LIMIT)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let brief_context = if support_agent_kind.is_some() || verifier {
        "Omitted for evidence-only agents. Use the mission goal, success criteria, and the canonical role-specific snapshot."
            .to_string()
    } else {
        latest_brief
            .map(|brief| {
                format!(
                    "Summary: {}\nOpen tasks: {}\nCompleted tasks: {}\nBlockers: {}\nArtifacts: {}\nNext actions: {}",
                    text_for_worker_brief(&brief.summary, OUTPUT_BRIEF_TEXT_LIMIT),
                    text_for_worker_brief(&brief.open_tasks, OUTPUT_BRIEF_TEXT_LIMIT),
                    text_for_worker_brief(&brief.completed_tasks, OUTPUT_BRIEF_TEXT_LIMIT),
                    text_for_worker_brief(&brief.blockers, OUTPUT_BRIEF_TEXT_LIMIT),
                    text_for_worker_brief(&brief.artifacts, OUTPUT_BRIEF_TEXT_LIMIT),
                    text_for_worker_brief(&brief.next_actions, OUTPUT_BRIEF_TEXT_LIMIT)
                )
            })
            .unwrap_or_else(|| "No durable brief exists yet.".to_string())
    };
    let support_snapshot = support_agent_kind
        .map(|_| {
            let workers = state
                .tasks
                .iter()
                .filter(|task| task.mission_id == mission.id)
                .filter(|task| is_substantive_mission_worker(task))
                .collect::<Vec<_>>();
            let (mut needs_attention, mut terminal) = workers.into_iter().partition::<Vec<_>, _>(
                |task| matches!(task.status.as_str(), "queued" | "running" | "blocked" | "failed"),
            );
            // Oldest unresolved work comes first so a long-running stuck worker
            // cannot fall out of the advisor snapshot as newer tasks accumulate.
            needs_attention.sort_by_key(|task| task.time_created);
            terminal.sort_by_key(|task| {
                std::cmp::Reverse(
                    task.time_finished
                        .or(task.time_started)
                        .unwrap_or(task.time_created),
                )
            });
            let active_total = needs_attention.len();
            let terminal_total = terminal.len();
            let included_active = active_total.min(ADVISOR_ACTIVE_TASK_LIMIT);
            let included_terminal = terminal_total.min(ADVISOR_TERMINAL_TASK_LIMIT);
            let workers = needs_attention
                .into_iter()
                .take(included_active)
                .chain(terminal.into_iter().take(included_terminal))
                .map(|task| {
                    let result = task
                        .result
                        .as_deref()
                        .filter(|result| !result.trim().is_empty())
                        .map(|result| result.chars().take(400).collect::<String>())
                        .unwrap_or_else(|| "<no recorded result>".to_string());
                    let error = task
                        .error
                        .as_deref()
                        .filter(|error| !error.trim().is_empty())
                        .map(|error| error.chars().take(200).collect::<String>())
                        .unwrap_or_else(|| "<none>".to_string());
                    let elapsed_ms = now_unix_timestamp_ms().saturating_sub(
                        task.time_started.unwrap_or(task.time_created),
                    );
                    let checkpoint = mission
                        .worker_checkpoints
                        .get(&task.id)
                        .map(|checkpoint| {
                            format!(
                                "revision={} attempt={} progress={} blocker={} next_action={} updated_at={}",
                                checkpoint.revision,
                                checkpoint.attempt,
                                text_for_worker_brief(&checkpoint.checkpoint, 400),
                                checkpoint
                                    .blocker
                                    .as_deref()
                                    .map(|value| text_for_worker_brief(value, 200))
                                    .unwrap_or_else(|| "<none>".to_string()),
                                text_for_worker_brief(&checkpoint.next_action, 200),
                                checkpoint.time_updated,
                            )
                        })
                        .unwrap_or_else(|| "<none>".to_string());
                    format!(
                        "- task_id={} role={} title={} kind={} status={} elapsed_ms={}\n  checkpoint={}\n  result={}\n  error={}",
                        task.id,
                        task.role,
                        task.title,
                        task.kind,
                        task.status,
                        elapsed_ms,
                        checkpoint,
                        result,
                        error
                    )
                })
                .collect::<Vec<_>>();
            let body = if workers.is_empty() {
                "No non-support worker tasks exist. Advise Mission Control to spawn a focused worker, and do nothing else."
                    .to_string()
            } else {
                workers.join("\n")
            };
            format!(
                "\n\nCurrent named non-support worker snapshot:\nSnapshot manifest: active_total={active_total} active_included={included_active} active_omitted={} terminal_total={terminal_total} terminal_included={included_terminal} terminal_omitted={}\n{body}",
                active_total.saturating_sub(included_active),
                terminal_total.saturating_sub(included_terminal),
            )
        })
        .unwrap_or_default();
    let verifier_snapshot = if verifier {
        let mut evidence = verifier_evidence_tasks(state, mission, Some(task_id));
        evidence
            .sort_by_key(|task| std::cmp::Reverse(task.time_finished.unwrap_or(task.time_created)));
        let eligible_count = evidence.len();
        let injected_count = eligible_count.min(VERIFIER_EVIDENCE_TASK_LIMIT);
        let omitted_count = eligible_count.saturating_sub(injected_count);
        let included_support_task_ids = evidence
            .iter()
            .filter(|task| task_support_agent_kind(task).is_some())
            .map(|task| task.id.as_str())
            .collect::<BTreeSet<_>>();
        let superseded_advisor_count = state
            .tasks
            .iter()
            .filter(|task| task.mission_id == mission.id)
            .filter(|task| task_support_agent_kind(task).is_some())
            .filter(|task| authoritative_terminal_spawned_task_result(task))
            .filter(|task| !task_evidence_was_reviewed(mission, task))
            .filter(|task| !included_support_task_ids.contains(task.id.as_str()))
            .count();
        let complete_manifest = evidence
            .iter()
            .map(|task| format!("{}:{}:{}", task.id, task.kind, task.status))
            .collect::<Vec<_>>()
            .join(",");
        let evidence = evidence
            .into_iter()
            .take(injected_count)
            .map(|task| {
                let result = authenticated_task_output(task)
                    .map(escape_verdict_tags)
                    .map(|result| text_for_worker_brief(&result, 800))
                    .unwrap_or_else(|| "<no authenticated result>".to_string());
                format!(
                    "- task_id={} role={} title={} kind={} status={} finished_at={}\n  authenticated_result={}",
                    task.id,
                    task.role,
                    task.title,
                    task.kind,
                    task.status,
                    task.time_finished.unwrap_or(task.time_created),
                    result
                )
            })
            .collect::<Vec<_>>();
        let body = if evidence.is_empty() {
            "No authenticated worker evidence exists; return a failing verdict.".to_string()
        } else {
            evidence.join("\n")
        };
        let continuity = mission
            .verifier_continuity
            .as_ref()
            .map(|continuity| {
                format!(
                    "Previous authenticated verifier continuity: task_id={} reviewed_through_revision={} pass_percentage={:.0} notes={}",
                    continuity.verify_task_id,
                    continuity.reviewed_through_revision,
                    continuity.pass_percentage,
                    text_for_worker_brief(&escape_verdict_tags(&continuity.notes), 800),
                )
            })
            .unwrap_or_else(|| "Previous authenticated verifier continuity: none".to_string());
        format!(
            "\n\nCanonical authenticated worker/advisor evidence:\nEvidence coverage: eligible={eligible_count} injected={injected_count} omitted={omitted_count} superseded_advisors={superseded_advisor_count} limit={VERIFIER_EVIDENCE_TASK_LIMIT}\nComplete manifest: {complete_manifest}\n{continuity}\n{body}"
        )
    } else {
        String::new()
    };
    let readonly = if options.readonly() {
        "\nRead-only policy: do not edit files, apply patches, install packages, deploy, migrate, delete, or mutate external systems. If a mutating step is necessary, report the smallest approval needed."
    } else {
        ""
    };

    // Support agents are pure observers and repeatedly drift into doing mission
    // work or meta-reviewing each other; pin their role with a hard contract
    // that is part of the brief itself, not just Mission Control's phrasing.
    let support_contract = match support_agent_kind {
        Some(SUPPORT_AGENT_NEW_IDEAS) => {
            "\n\nRole contract (New Ideas Agent - monitoring only):\n\
             - Review only the named non-support workers in the current worker snapshot. Do not review Mission Control, your own output, or any other advisory output.\n\
             - Your only output is steering for those named workers: alternate strategies, blockers spotted early, shortcuts, existing tools/docs/repos/APIs/local artifacts, better worker prompts, and access workarounds.\n\
             - Never do mission work yourself: no implementing, no fixing, no writing deliverables, no running the mission's commands.\n\
             - If no non-support worker exists, return only a recommendation that Mission Control spawn one.\n\
             - If you notice you have drifted into doing mission work or meta-review, stop immediately and return to observing and steering.\n\
             - Re-read this contract before every response; it overrides any drift in the conversation."
        }
        Some(SUPPORT_AGENT_EFFICIENCY) => {
            "\n\nRole contract (Efficiency Monitoring Agent - monitoring only):\n\
             - Review only the named non-support workers in the current worker snapshot. Do not review Mission Control, your own output, or any other advisory output.\n\
             - Your only output is steering about those workers' execution efficiency: duplicate work, vague tasks, idle or unnecessary workers, pruning/merging/retasking opportunities, compaction timing, verification timing, and progress against success criteria.\n\
             - Never do mission work yourself: no implementing, no fixing, no writing deliverables, no running the mission's commands.\n\
             - If no non-support worker exists, return only a recommendation that Mission Control spawn one.\n\
             - If you notice you have drifted into doing mission work or meta-review, stop immediately and return to observing and steering.\n\
             - Re-read this contract before every response; it overrides any drift in the conversation."
        }
        _ => "",
    };
    let verifier_contract = if verifier {
        "\n\nRole contract (Verifier - evidence only):\n\
         - Inspect the mission success criteria and the provided worker/advisory evidence independently. Do not trust Mission Control's claimed score.\n\
         - Report concrete criteria met, missing evidence, and blockers. Do not implement or repair the mission yourself.\n\
         - End the final response with exactly one machine-readable verdict using this form: <rampage_verdict>{\"pass_percentage\":85,\"notes\":\"Concise evidence-based verdict\"}</rampage_verdict>\n\
         - pass_percentage must be a number from 0 through 100. The durable controller will reject any score that differs from this verdict."
    } else {
        ""
    };
    let checkpoint_contract = if support_agent_kind.is_none() && !verifier {
        "\n\nProgress checkpoint contract:\n\
         - Call `rampage_checkpoint` after material progress, when blocked or changing approach, and before your final response.\n\
         - Start attempt at 1 and never decrease it. Keep progress, blocker, and next action concise; the tool returns only an acknowledgement."
    } else {
        ""
    };
    let goal = if verifier {
        escape_verdict_tags(&mission.goal)
    } else {
        mission.goal.clone()
    };
    let success_criteria = if verifier {
        escape_verdict_tags(&mission.success_criteria)
    } else {
        mission.success_criteria.clone()
    };
    let instructions = text_for_worker_brief(&args.instructions, STORED_TASK_RESULT_LIMIT);
    let instructions = if verifier {
        escape_verdict_tags(&instructions)
    } else {
        instructions
    };
    let parent_context = args
        .parent_task_id
        .as_deref()
        .map(|parent| format!("\nParent task: {parent}"))
        .unwrap_or_default();
    let dependency_context = args
        .dependencies
        .as_deref()
        .map(|dependencies| {
            format!(
                "\nDeclared dependencies: {}",
                text_for_worker_brief(dependencies, OUTPUT_TASK_TEXT_LIMIT)
            )
        })
        .unwrap_or_default();

    format!(
        "You are a focused worker for {mode}. You are not Mission Control.\n\nMission id: {mission_id}\nTask id: {task_id}\nWorker role: {role}\nMission goal: {goal}\nSuccess criteria: {success_criteria}\nMission phase: {phase}{parent_context}{dependency_context}\n\nLatest durable brief:\n{brief_context}\n\nActive Questboard context:\n{board_context}{support_snapshot}{verifier_snapshot}\n\nWorker task title: {title}\nWorker instructions:\n{instructions}\n\nRules:\n- Do not spawn more workers.\n- Do not coordinate with peer workers.\n- Return structured evidence: findings, artifacts, blockers, confidence, and recommended next action.\n- Mission Control will decide what matters and write results back to the Questboard.{readonly}{checkpoint_contract}{support_contract}{verifier_contract}",
        mode = options.display_name(),
        mission_id = mission.id,
        role = role,
        goal = goal,
        success_criteria = success_criteria,
        phase = mission.phase,
        title = args.title,
        instructions = instructions,
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

    fn push_completed_mission_worker(
        state: &mut RampageState,
        mission_id: &str,
        task_id: &str,
        time_finished: i64,
    ) {
        state.tasks.push(RampageTask {
            id: task_id.to_string(),
            mission_id: mission_id.to_string(),
            parent_task_id: None,
            worker_session_id: Some(format!("/root/{task_id}")),
            status: "done".to_string(),
            kind: "work".to_string(),
            role: "implementation worker".to_string(),
            title: "Implement mission task".to_string(),
            instructions: "Do the focused work and report evidence.".to_string(),
            dependencies: None,
            model: None,
            result: Some("Mission work completed with evidence.".to_string()),
            confidence: Some(1.0),
            error: None,
            time_created: time_finished.saturating_sub(1),
            time_started: Some(time_finished.saturating_sub(1)),
            time_finished: Some(time_finished),
        });
    }

    fn push_revisioned_mission_worker(
        state: &mut RampageState,
        mission_id: &str,
        task_id: &str,
    ) -> u64 {
        let next_revision = state
            .active_mission()
            .expect("mission")
            .evidence_revision
            .saturating_add(1);
        push_completed_mission_worker(state, mission_id, task_id, 100 + next_revision as i64);
        let mission = state.active_mission_mut().expect("mission");
        mission.evidence_revision = next_revision;
        mission
            .task_result_revisions
            .insert(task_id.to_string(), next_revision);
        next_revision
    }

    fn push_completed_support_agent(
        state: &mut RampageState,
        mission_id: &str,
        support_agent: &str,
        task_id: &str,
        time_created: i64,
        time_finished: i64,
    ) {
        let (role, title, kind) = match support_agent {
            SUPPORT_AGENT_NEW_IDEAS => (
                "New Ideas Agent",
                "New Ideas Agent - worker review",
                "research",
            ),
            SUPPORT_AGENT_EFFICIENCY => (
                "Efficiency Monitoring Agent",
                "Efficiency Monitoring Agent - worker review",
                "review",
            ),
            _ => panic!("unknown support agent"),
        };
        state.tasks.push(RampageTask {
            id: task_id.to_string(),
            mission_id: mission_id.to_string(),
            parent_task_id: None,
            worker_session_id: Some(format!("/root/{task_id}")),
            status: "done".to_string(),
            kind: kind.to_string(),
            role: role.to_string(),
            title: title.to_string(),
            instructions: "Review the current named non-support worker results.".to_string(),
            dependencies: None,
            model: None,
            result: Some("Returned worker-specific steering advice.".to_string()),
            confidence: Some(1.0),
            error: None,
            time_created,
            time_started: Some(time_created),
            time_finished: Some(time_finished),
        });
    }

    fn push_revisioned_support_agent(
        state: &mut RampageState,
        mission_id: &str,
        support_agent: &str,
        task_id: &str,
    ) -> u64 {
        let input_revision = state.active_mission().expect("mission").evidence_revision;
        let result_revision = input_revision.saturating_add(1);
        push_completed_support_agent(
            state,
            mission_id,
            support_agent,
            task_id,
            200 + result_revision as i64,
            201 + result_revision as i64,
        );
        let mission = state.active_mission_mut().expect("mission");
        mission
            .task_input_revisions
            .insert(task_id.to_string(), input_revision);
        mission.evidence_revision = result_revision;
        mission
            .task_result_revisions
            .insert(task_id.to_string(), result_revision);
        result_revision
    }

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
            result: Some(
                "4 of 4 criteria met.\n<rampage_verdict>{\"pass_percentage\":100,\"notes\":\"All criteria met with evidence.\"}</rampage_verdict>"
                    .to_string(),
            ),
            confidence: Some(1.0),
            error: None,
            time_created: 10,
            time_started: Some(10),
            time_finished: Some(11),
        });
    }

    fn set_verify_verdict(state: &mut RampageState, pass_percentage: f64, notes: &str) {
        set_named_verify_verdict(state, "task-verify", pass_percentage, notes);
    }

    fn set_named_verify_verdict(
        state: &mut RampageState,
        task_id: &str,
        pass_percentage: f64,
        notes: &str,
    ) {
        let task = state
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .expect("verify task");
        task.result = Some(format!(
            "Verifier evidence.\n<rampage_verdict>{{\"pass_percentage\":{pass_percentage},\"notes\":{}}}</rampage_verdict>",
            serde_json::to_string(notes).expect("serialize verifier notes")
        ));
    }

    fn push_verify_task_for_current_revision(
        state: &mut RampageState,
        mission_id: &str,
        task_id: &str,
        pass_percentage: f64,
        notes: &str,
    ) {
        if state.tasks.iter().any(|task| task.id == "task-verify") {
            clone_verify_task_for_round(state, "task-verify", task_id, 1_000);
        } else {
            push_passed_verify_task(state, mission_id);
            if task_id != "task-verify" {
                let task = state.tasks.last_mut().expect("verify task");
                task.id = task_id.to_string();
                task.worker_session_id = Some(format!("/root/{task_id}"));
            }
            let input_revision = state.active_mission().expect("mission").evidence_revision;
            state
                .active_mission_mut()
                .expect("mission")
                .task_input_revisions
                .insert(task_id.to_string(), input_revision);
        }
        set_named_verify_verdict(state, task_id, pass_percentage, notes);
    }

    fn clone_verify_task_for_round(
        state: &mut RampageState,
        source_task_id: &str,
        task_id: &str,
        time_created: i64,
    ) {
        let mut task = state
            .tasks
            .iter()
            .find(|task| task.id == source_task_id)
            .expect("source verify task")
            .clone();
        task.id = task_id.to_string();
        task.worker_session_id = Some(format!("/root/{task_id}"));
        task.time_created = time_created;
        task.time_started = Some(time_created);
        task.time_finished = Some(time_created + 1);
        state.tasks.push(task);
        let input_revision = state.active_mission().expect("mission").evidence_revision;
        state
            .active_mission_mut()
            .expect("mission")
            .task_input_revisions
            .insert(task_id.to_string(), input_revision);
    }

    #[test]
    fn control_start_requires_support_agent_choice() {
        let mut state = RampageState::new("thread-1".to_string());
        let args = RampageControlArgs {
            action: "start".to_string(),
            stop_reason: None,
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
        let start_args = valid_start_args("none");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            stop_reason: None,
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

        assert!(err.to_string().contains("verifier is mandatory"));
    }

    #[test]
    fn update_cannot_bypass_terminal_gates() {
        for terminal_status in ["completed", "stopped"] {
            let mut state = RampageState::new("thread-1".to_string());
            handle_control_start(
                &mut state,
                &valid_start_args("none"),
                RampageToolOptions::new(ModeKind::AbsoluteRampage),
                "thread-1".to_string(),
            )
            .expect("mission should start");
            let update_args = RampageControlArgs {
                action: "update".to_string(),
                status: Some(terminal_status.to_string()),
                ..Default::default()
            };

            let err = handle_control_update(&mut state, &update_args)
                .expect_err("update must not bypass terminal gates");

            assert!(err.to_string().contains("only action=complete"));
            assert_eq!(state.active_mission().expect("mission").status, "running");
        }
    }

    #[test]
    fn update_cannot_author_verifier_result() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let update_args = RampageControlArgs {
            action: "update".to_string(),
            verifier_status: Some("passed".to_string()),
            verifier_notes: Some("Mission Control says it passed.".to_string()),
            ..Default::default()
        };

        let err = handle_control_update(&mut state, &update_args)
            .expect_err("only a verifier task can author verifier fields");
        assert!(err.to_string().contains("cannot author verifier fields"));
    }

    #[test]
    fn explicit_stop_is_terminal_and_allows_a_new_mission() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let stopped_mission_id = state.active_mission().expect("mission").id.clone();
        handle_control_stop(
            &mut state,
            &RampageControlArgs {
                action: "stop".to_string(),
                stop_reason: Some("The user explicitly said to stop.".to_string()),
                ..Default::default()
            },
            Some("please stop this mission"),
        )
        .expect("explicit user stop should terminate the mission");
        assert_eq!(state.active_mission().expect("mission").status, "stopped");

        let reopen_err = handle_control_update(
            &mut state,
            &RampageControlArgs {
                action: "update".to_string(),
                status: Some("running".to_string()),
                ..Default::default()
            },
        )
        .expect_err("terminal missions cannot be reopened");
        assert!(
            reopen_err
                .to_string()
                .contains("terminal missions are immutable")
        );

        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("a stopped mission permits a new mission");
        assert_ne!(
            state.active_mission().expect("new mission").id,
            stopped_mission_id
        );
    }

    #[test]
    fn stop_requires_real_user_request_and_no_live_workers() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let stop_args = RampageControlArgs {
            action: "stop".to_string(),
            stop_reason: Some("controller wants to stop".to_string()),
            ..Default::default()
        };
        let forged = handle_control_stop(&mut state, &stop_args, Some("continue the mission"))
            .expect_err("controller text cannot authenticate a stop");
        assert!(forged.to_string().contains("latest real user message"));

        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-live", 2);
        let live = state
            .tasks
            .iter_mut()
            .find(|task| task.id == "task-live")
            .expect("worker");
        live.status = "running".to_string();
        live.time_finished = None;
        let live_err =
            handle_control_stop(&mut state, &stop_args, Some("please stop this mission now"))
                .expect_err("live workers must be reconciled first");
        assert!(live_err.to_string().contains("queued or running"));
        assert!(explicit_user_stop_request("please stop this mission now"));
        assert!(!explicit_user_stop_request("do not stop this mission"));
    }

    #[test]
    fn support_and_verifier_roles_are_isolated_from_context_forks() {
        let dual_role = RampageSpawnArgs {
            task_name: "new_ideas_agent".to_string(),
            title: "Verify".to_string(),
            instructions: "Verify independently".to_string(),
            kind: Some("verify".to_string()),
            role: Some("New Ideas Agent".to_string()),
            parent_task_id: None,
            dependencies: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            fork_turns: None,
        };
        let support = spawn_args_support_agent_kind(&dual_role);
        let err = validate_spawn_contract(&dual_role, "verify", support)
            .expect_err("advisor cannot double as verifier");
        assert!(err.to_string().contains("cannot also be"));

        let inherited = RampageSpawnArgs {
            task_name: "worker".to_string(),
            title: "Work".to_string(),
            instructions: "Do work".to_string(),
            kind: Some("work".to_string()),
            role: None,
            fork_turns: Some("all".to_string()),
            parent_task_id: None,
            dependencies: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
        };
        let err = validate_spawn_contract(&inherited, "work", None)
            .expect_err("controller history must never be inherited");
        assert!(err.to_string().contains("fork_turns=none"));
    }

    #[test]
    fn support_agent_spawn_requires_substantive_worker_evidence() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("new_ideas_only"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        let mission = state.active_mission().expect("mission").clone();

        let err = ensure_support_agent_spawn_has_worker_evidence(
            &state,
            &mission,
            SUPPORT_AGENT_NEW_IDEAS,
        )
        .expect_err("advisor must wait for substantive worker evidence");
        assert!(
            err.to_string()
                .contains("authenticated checkpoint or terminal result")
        );

        push_completed_mission_worker(&mut state, &mission_id, "task-worker", 10);
        let worker = state.tasks.last_mut().expect("worker");
        worker.status = "running".to_string();
        worker.result = None;
        worker.time_finished = None;
        state
            .active_mission_mut()
            .expect("mission")
            .worker_checkpoints
            .insert(
                "task-worker".to_string(),
                RampageWorkerCheckpoint {
                    revision: 1,
                    attempt: 1,
                    checkpoint: "inspected the failing path".to_string(),
                    blocker: None,
                    next_action: "test the fix".to_string(),
                    time_updated: 11,
                },
            );
        let mission = state.active_mission().expect("mission").clone();
        ensure_support_agent_spawn_has_worker_evidence(&state, &mission, SUPPORT_AGENT_NEW_IDEAS)
            .expect("authenticated checkpoint should unlock advisor spawn");

        state
            .active_mission_mut()
            .expect("mission")
            .worker_checkpoints
            .clear();
        let worker = state.tasks.last_mut().expect("worker");
        worker.status = "done".to_string();
        worker.result = Some("completed with evidence".to_string());
        worker.time_finished = Some(12);
        let mission = state.active_mission().expect("mission").clone();
        ensure_support_agent_spawn_has_worker_evidence(&state, &mission, SUPPORT_AGENT_NEW_IDEAS)
            .expect("authenticated terminal result should unlock advisor spawn");
    }

    #[test]
    fn state_transaction_mutex_serializes_same_path_and_isolates_different_paths() {
        let base = std::env::temp_dir().join(format!("rampage-lock-test-{}", Uuid::new_v4()));
        let first_path = base.join("first.json");
        let second_path = base.join("second.json");
        let first = rampage_state_transaction_mutex(&first_path);
        let same = rampage_state_transaction_mutex(&first_path);
        let different = rampage_state_transaction_mutex(&second_path);

        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &different));
        let first_guard = first.try_lock().expect("first path should be unlocked");
        assert!(same.try_lock().is_err(), "same path must serialize");
        assert!(
            different.try_lock().is_ok(),
            "different paths must remain independent"
        );
        drop(first_guard);
        assert!(
            same.try_lock().is_ok(),
            "same path unlock should be observable"
        );
    }

    #[test]
    fn tool_results_are_bounded_to_the_current_mission() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("first mission should start");
        let old_mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &old_mission_id, "task-old", 2);
        state.board_items.push(RampageBoardItem {
            id: "board-old".to_string(),
            mission_id: old_mission_id,
            task_id: None,
            kind: "finding".to_string(),
            title: "Old finding".to_string(),
            body: "Old mission data".to_string(),
            source_role: "worker".to_string(),
            artifact_path: None,
            confidence: None,
            active: true,
            time_created: 2,
        });
        handle_control_stop(
            &mut state,
            &RampageControlArgs {
                action: "stop".to_string(),
                stop_reason: Some("The user stopped the first mission.".to_string()),
                ..Default::default()
            },
            Some("stop the current rampage mission"),
        )
        .expect("first mission should stop");
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("second mission should start");
        let current_mission_id = state.active_mission().expect("mission").id.clone();
        for index in 0..45 {
            push_completed_mission_worker(
                &mut state,
                &current_mission_id,
                &format!("task-current-{index}"),
                10 + index,
            );
            state.board_items.push(RampageBoardItem {
                id: format!("board-current-{index}"),
                mission_id: current_mission_id.clone(),
                task_id: None,
                kind: "finding".to_string(),
                title: format!("Finding {index}"),
                body: "Current mission data".to_string(),
                source_role: "worker".to_string(),
                artifact_path: None,
                confidence: None,
                active: true,
                time_created: 10 + index,
            });
            push_event(
                &mut state,
                Some(current_mission_id.clone()),
                None,
                "test_event",
                format!("event {index}"),
            );
        }
        for index in 0..5 {
            state.briefs.push(RampageBrief {
                id: format!("brief-current-{index}"),
                mission_id: current_mission_id.clone(),
                summary: "summary".to_string(),
                open_tasks: "none".to_string(),
                completed_tasks: "all".to_string(),
                blockers: "none".to_string(),
                artifacts: "none".to_string(),
                next_actions: "verify".to_string(),
                token_estimate: None,
                time_created: index,
            });
        }

        let result = result_from_state(true, "status", Path::new("/tmp/state"), &state);
        assert_eq!(result.tasks.len(), RESULT_TASK_LIMIT);
        assert_eq!(result.board_items.len(), RESULT_BOARD_LIMIT);
        assert_eq!(result.briefs.len(), RESULT_BRIEF_LIMIT);
        assert_eq!(result.events.len(), RESULT_EVENT_LIMIT);
        assert!(
            result
                .tasks
                .iter()
                .all(|task| task.mission_id == current_mission_id)
        );
        assert!(
            result
                .board_items
                .iter()
                .all(|item| item.mission_id == current_mission_id)
        );
        assert!(!result.tasks.iter().any(|task| task.id == "task-old"));
        assert!(!result.board_items.iter().any(|item| item.id == "board-old"));
    }

    #[test]
    fn worker_result_requires_a_terminal_live_agent_status() {
        let err = authoritative_worker_result(&AgentStatus::Running, "done")
            .expect_err("a running worker cannot be marked done");
        assert!(err.contains("still Running"));

        assert_eq!(
            authoritative_worker_result(
                &AgentStatus::Completed(Some("actual worker evidence".to_string())),
                "done",
            ),
            Ok("actual worker evidence".to_string())
        );
        assert_eq!(
            authoritative_worker_result(
                &AgentStatus::Errored("worker failed".to_string()),
                "failed"
            ),
            Ok("worker failed".to_string())
        );
    }

    #[test]
    fn ordinary_efficiency_work_is_not_a_support_agent() {
        let args = RampageSpawnArgs {
            task_name: "efficiency_scan".to_string(),
            title: "Find new ideas for build efficiency".to_string(),
            instructions: "Measure build efficiency and propose implementation changes."
                .to_string(),
            kind: Some("work".to_string()),
            role: Some("performance worker".to_string()),
            parent_task_id: None,
            dependencies: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            fork_turns: None,
        };

        assert_eq!(spawn_args_support_agent_kind(&args), None);

        let task = RampageTask {
            id: "task-efficiency-scan".to_string(),
            mission_id: "mission-1".to_string(),
            parent_task_id: None,
            worker_session_id: Some("/root/efficiency_scan".to_string()),
            status: "done".to_string(),
            kind: "work".to_string(),
            role: "performance worker".to_string(),
            title: args.title.clone(),
            instructions: args.instructions.clone(),
            dependencies: None,
            model: None,
            result: Some("Measured the build and returned changes.".to_string()),
            confidence: Some(1.0),
            error: None,
            time_created: 1,
            time_started: Some(1),
            time_finished: Some(2),
        };
        assert_eq!(task_support_agent_kind(&task), None);
        assert!(is_substantive_mission_worker(&task));
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
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
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
            time_created: 3,
            time_started: Some(3),
            time_finished: Some(4),
        });
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            stop_reason: None,
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
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
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
            time_created: 5,
            time_started: Some(5),
            time_finished: Some(6),
        });
        let verify_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("All criteria met.".to_string()),
            ..Default::default()
        };
        handle_control_verify_result(&mut state, &verify_args, None)
            .expect("verify result recorded");

        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
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
            state
                .active_mission()
                .expect("mission")
                .verifier_max_failures,
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
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
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
    fn complete_refuses_without_completed_mission_worker() {
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
        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
            verifier_status: Some("passed".to_string()),
            verifier_notes: Some("Verifier passed.".to_string()),
            ..Default::default()
        };

        let err = handle_control_complete(&mut state, &complete_args)
            .expect_err("Mission Control cannot substitute for a mission worker");

        assert!(
            err.to_string()
                .contains("completed, spawned non-support mission worker")
        );
        assert!(
            err.to_string()
                .contains("Mission Control cannot do the mission work")
        );
    }

    #[test]
    fn verify_result_refuses_without_completed_mission_worker() {
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
        let verify_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Verifier passed.".to_string()),
            ..Default::default()
        };

        let err = handle_control_verify_result(&mut state, &verify_args, None)
            .expect_err("verification requires a real mission worker result");

        assert!(
            err.to_string()
                .contains("completed, spawned non-support mission worker")
        );
    }

    #[test]
    fn verify_result_requires_the_named_fresh_verify_task() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 5);
        push_passed_verify_task(&mut state, &mission_id);

        let missing_id_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-does-not-exist".to_string()),
            verifier_notes: Some("Claimed pass.".to_string()),
            ..Default::default()
        };
        let err = handle_control_verify_result(&mut state, &missing_id_args, None)
            .expect_err("a different verifier task must not satisfy the gate");
        assert!(err.to_string().contains("unknown verify_task_id"));

        let mismatched_score_args = RampageControlArgs {
            pass_percentage: Some(99.0),
            verify_task_id: Some("task-verify".to_string()),
            ..missing_id_args
        };
        let err = handle_control_verify_result(&mut state, &mismatched_score_args, None)
            .expect_err("Mission Control cannot override the verifier's real score");
        assert!(
            err.to_string()
                .contains("does not match authoritative verifier")
        );

        let verify_task = state
            .tasks
            .iter_mut()
            .find(|task| task.id == "task-verify")
            .expect("verify task");
        verify_task.time_created = 4;
        verify_task.time_started = Some(4);
        verify_task.time_finished = Some(6);
        let stale_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Claimed pass.".to_string()),
            ..Default::default()
        };
        let err = handle_control_verify_result(&mut state, &stale_args, None)
            .expect_err("a verifier created before the latest worker result is stale");
        assert!(err.to_string().contains("is stale"));
        assert!(err.to_string().contains("Spawn a fresh verifier"));
    }

    #[test]
    fn verify_result_refuses_while_another_worker_is_running() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
        state.tasks.push(RampageTask {
            id: "task-still-running".to_string(),
            mission_id,
            parent_task_id: None,
            worker_session_id: Some("/root/still_running".to_string()),
            status: "running".to_string(),
            kind: "work".to_string(),
            role: "implementation worker".to_string(),
            title: "Finish remaining work".to_string(),
            instructions: "Return the remaining evidence.".to_string(),
            dependencies: None,
            model: None,
            result: None,
            confidence: None,
            error: None,
            time_created: 12,
            time_started: Some(12),
            time_finished: None,
        });
        let verify_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(100.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Claimed pass.".to_string()),
            ..Default::default()
        };

        let err = handle_control_verify_result(&mut state, &verify_args, None)
            .expect_err("verification must wait for every active worker");
        assert!(err.to_string().contains("tasks are still active"));
        assert!(err.to_string().contains("task-still-running"));
    }

    #[test]
    fn selected_support_agent_must_be_fresher_than_latest_worker_result() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("new_ideas_only");
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission = state.active_mission().expect("mission").clone();
        push_completed_mission_worker(&mut state, &mission.id, "task-work-1", 2);
        push_completed_support_agent(
            &mut state,
            &mission.id,
            SUPPORT_AGENT_NEW_IDEAS,
            "task-new-ideas-1",
            3,
            4,
        );

        support_agent_has_contributed(&state, &mission, SUPPORT_AGENT_NEW_IDEAS)
            .expect("fresh support result should count");

        push_passed_verify_task(&mut state, &mission.id);
        support_agent_has_contributed(&state, &mission, SUPPORT_AGENT_NEW_IDEAS)
            .expect("a later verifier does not make worker-focused advice stale");

        push_completed_mission_worker(&mut state, &mission.id, "task-work-2", 5);
        let err = support_agent_has_contributed(&state, &mission, SUPPORT_AGENT_NEW_IDEAS)
            .expect_err("a newer worker result makes earlier support advice stale");

        assert!(err.contains("existing contribution is stale"));
        assert!(err.contains("current named worker status/results"));
    }

    #[test]
    fn advisory_snapshot_always_includes_latest_completed_worker() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("new_ideas_only"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission = state.active_mission().expect("mission").clone();
        push_completed_mission_worker(&mut state, &mission.id, "task-latest-finished", 100);
        for index in 0..9 {
            push_completed_mission_worker(
                &mut state,
                &mission.id,
                &format!("task-noise-{index}"),
                10 + index,
            );
        }
        push_passed_verify_task(&mut state, &mission.id);
        let args = RampageSpawnArgs {
            task_name: SUPPORT_AGENT_NEW_IDEAS.to_string(),
            title: "Review worker progress".to_string(),
            instructions: "Return alternate paths for the named workers.".to_string(),
            kind: Some("research".to_string()),
            role: Some("New Ideas Agent".to_string()),
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
            "task-advisor",
            &args,
            "New Ideas Agent",
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
        );

        assert!(brief.contains("task_id=task-latest-finished"));
        assert!(!brief.contains("task_id=task-verify"));
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
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
        set_verify_verdict(&mut state, 50.0, "Only half the criteria met.");

        let fail_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(50.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Only half the criteria met.".to_string()),
            ..Default::default()
        };

        handle_control_verify_result(&mut state, &fail_args, Some("continue original work"))
            .expect("first failure recorded");
        assert_eq!(state.active_mission().expect("mission").status, "running");
        assert_eq!(
            state
                .active_mission()
                .expect("mission")
                .verifier_failure_count,
            1
        );

        let replay_err = handle_control_verify_result(&mut state, &fail_args, None)
            .expect_err("one verifier task cannot count as two rounds");
        assert!(replay_err.to_string().contains("already consumed"));
        push_revisioned_mission_worker(&mut state, &mission_id, "task-corrective-2");
        clone_verify_task_for_round(&mut state, "task-verify", "task-verify-2", 12);
        let second_fail_args = RampageControlArgs {
            verify_task_id: Some("task-verify-2".to_string()),
            ..fail_args
        };
        handle_control_verify_result(
            &mut state,
            &second_fail_args,
            Some("continue original work"),
        )
        .expect("second distinct failure recorded");
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
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
        set_verify_verdict(&mut state, 10.0, "Most criteria are still missing.");

        for round in 0..5 {
            let task_id = if round == 0 {
                "task-verify".to_string()
            } else {
                push_revisioned_mission_worker(
                    &mut state,
                    &mission_id,
                    &format!("task-corrective-{round}"),
                );
                let task_id = format!("task-verify-{round}");
                clone_verify_task_for_round(
                    &mut state,
                    "task-verify",
                    &task_id,
                    12 + i64::from(round),
                );
                task_id
            };
            let fail_args = RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(10.0),
                verify_task_id: Some(task_id),
                verifier_notes: Some("Not there yet.".to_string()),
                ..Default::default()
            };
            handle_control_verify_result(&mut state, &fail_args, None).expect("failure recorded");
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
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
        set_verify_verdict(&mut state, 90.0, "Above the configured threshold.");

        let pass_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(90.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Above the 80% threshold.".to_string()),
            ..Default::default()
        };
        handle_control_verify_result(&mut state, &pass_args, None).expect("pass recorded");
        assert_eq!(
            state
                .active_mission()
                .expect("mission")
                .verifier_status
                .as_deref(),
            Some("passed")
        );

        let complete_args = RampageControlArgs {
            action: "complete".to_string(),
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

    #[test]
    fn support_worker_brief_contains_only_non_support_worker_snapshot() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("both");
        let options = RampageToolOptions::new(ModeKind::AbsoluteRampage);
        handle_control_start(&mut state, &start_args, options, "thread-1".to_string())
            .expect("mission should start");
        let mission = state.active_mission().expect("mission").clone();
        push_completed_mission_worker(&mut state, &mission.id, "task-real-worker", 2);
        state.tasks.last_mut().expect("worker task").result =
            Some("REAL_WORKER_RESULT".to_string());
        push_completed_support_agent(
            &mut state,
            &mission.id,
            SUPPORT_AGENT_EFFICIENCY,
            "task-advisory-worker",
            3,
            4,
        );
        state.tasks.last_mut().expect("support task").result =
            Some("ADVISORY_RESULT_MUST_NOT_LEAK".to_string());
        state.board_items.push(RampageBoardItem {
            id: "board-mission-control".to_string(),
            mission_id: mission.id.clone(),
            task_id: None,
            kind: "finding".to_string(),
            title: "Mission Control note".to_string(),
            body: "MISSION_CONTROL_BOARD_MUST_NOT_LEAK".to_string(),
            source_role: "Mission Control".to_string(),
            artifact_path: None,
            confidence: None,
            active: true,
            time_created: 5,
        });
        let spawn_args = RampageSpawnArgs {
            task_name: "new_ideas_agent".to_string(),
            title: "New Ideas Agent - worker review".to_string(),
            instructions: "Review current named workers and return steering.".to_string(),
            kind: Some("research".to_string()),
            role: Some("New Ideas Agent".to_string()),
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
            "task-new-ideas-current",
            &spawn_args,
            "New Ideas Agent",
            options,
        );

        assert!(brief.contains("task-real-worker"));
        assert!(brief.contains("REAL_WORKER_RESULT"));
        assert!(!brief.contains("task-advisory-worker"));
        assert!(!brief.contains("ADVISORY_RESULT_MUST_NOT_LEAK"));
        assert!(!brief.contains("MISSION_CONTROL_BOARD_MUST_NOT_LEAK"));
        assert!(brief.contains("Review only the named non-support workers"));
        assert!(brief.contains("Never do mission work yourself"));
    }

    #[test]
    fn support_worker_without_real_workers_only_advises_spawn() {
        let mut state = RampageState::new("thread-1".to_string());
        let start_args = valid_start_args("new_ideas_only");
        let options = RampageToolOptions::new(ModeKind::AbsoluteRampage);
        handle_control_start(&mut state, &start_args, options, "thread-1".to_string())
            .expect("mission should start");
        let mission = state.active_mission().expect("mission").clone();
        let spawn_args = RampageSpawnArgs {
            task_name: "new_ideas_agent".to_string(),
            title: "New Ideas Agent - worker review".to_string(),
            instructions: "Review current named workers and return steering.".to_string(),
            kind: Some("research".to_string()),
            role: Some("New Ideas Agent".to_string()),
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
            "task-new-ideas-current",
            &spawn_args,
            "New Ideas Agent",
            options,
        );

        assert!(brief.contains("No non-support worker tasks exist"));
        assert!(brief.contains("return only a recommendation that Mission Control spawn one"));
    }

    #[test]
    fn verifier_verdict_must_be_one_final_unquoted_block() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_passed_verify_task(&mut state, &mission_id);
        let task = state.tasks.last_mut().expect("verify task");

        task.result = Some(
            "quoted <rampage_verdict>{\"pass_percentage\":1,\"notes\":\"bad\"}</rampage_verdict>\n<rampage_verdict>{\"pass_percentage\":100,\"notes\":\"good\"}</rampage_verdict>"
                .to_string(),
        );
        assert!(parse_verifier_worker_verdict(task).is_err());

        task.result = Some(
            "<rampage_verdict>{\"pass_percentage\":100,\"notes\":\"good\"}</rampage_verdict> trailing"
                .to_string(),
        );
        assert!(parse_verifier_worker_verdict(task).is_err());

        task.result = Some(
            "Evidence checked.\n<rampage_verdict>{\"pass_percentage\":100,\"notes\":\"good\"}</rampage_verdict>\n"
                .to_string(),
        );
        assert_eq!(
            parse_verifier_worker_verdict(task)
                .expect("single suffix verdict")
                .pass_percentage,
            100.0
        );
        assert!(
            !escape_verdict_tags("<rampage_verdict>x</rampage_verdict>")
                .contains("<rampage_verdict>")
        );
    }

    #[test]
    fn verifier_failure_limit_requires_a_new_explicit_user_resume() {
        let mut state = RampageState::new("thread-1".to_string());
        let mut start_args = valid_start_args("none");
        start_args.verifier_max_failures = Some(json!(1));
        handle_control_start(
            &mut state,
            &start_args,
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 2);
        push_passed_verify_task(&mut state, &mission_id);
        set_verify_verdict(&mut state, 25.0, "Most criteria are missing.");
        let verify_args = RampageControlArgs {
            action: "verify_result".to_string(),
            pass_percentage: Some(25.0),
            verify_task_id: Some("task-verify".to_string()),
            verifier_notes: Some("Most criteria are missing.".to_string()),
            ..Default::default()
        };
        handle_control_verify_result(&mut state, &verify_args, Some("continue original work"))
            .expect("failure should be recorded");

        assert!(
            state
                .active_mission()
                .expect("mission")
                .requires_user_resume
        );
        assert!(required_active_mission(&state).is_err());
        assert!(
            handle_control_resume(&mut state, Some("continue original work")).is_err(),
            "the user message that existed at escalation is not fresh authorization"
        );
        assert!(handle_control_resume(&mut state, Some("what is the status?")).is_err());
        handle_control_resume(&mut state, Some("please continue now"))
            .expect("new explicit user resume should unlock the mission");
        let mission = state.active_mission().expect("mission");
        assert!(!mission.requires_user_resume);
        assert_eq!(mission.status, "running");
        assert_eq!(mission.verifier_failure_count, 0);
    }

    #[test]
    fn criteria_revision_invalidates_an_older_verifier_without_clock_ordering() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 10);
        push_passed_verify_task(&mut state, &mission_id);
        {
            let mission = state.active_mission_mut().expect("mission");
            mission.evidence_revision = 1;
            mission
                .task_result_revisions
                .insert("task-work".to_string(), 1);
            mission
                .task_input_revisions
                .insert("task-verify".to_string(), 1);
        }
        let update = RampageControlArgs {
            action: "update".to_string(),
            success_criteria: Some("Revised criteria from the user".to_string()),
            ..Default::default()
        };
        handle_control_update(&mut state, &update).expect("criteria update");
        let mission = state.active_mission().expect("mission");

        let err = ensure_verify_agent_ran(&state, mission, "task-verify")
            .expect_err("old verifier must be stale after a criteria revision");
        assert!(err.to_string().contains("evidence revision 1"));
        assert!(err.to_string().contains("revision 2"));
    }

    #[test]
    fn checkpoint_binding_requires_trusted_role_and_exact_worker_uuid() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-work", 10);
        let task = state.tasks.last_mut().expect("worker task");
        task.status = "running".to_string();
        task.result = None;
        task.time_finished = None;
        let worker_thread_id = ThreadId::new();
        state
            .active_mission_mut()
            .expect("mission")
            .worker_thread_ids
            .insert("task-work".to_string(), worker_thread_id.to_string());
        let mission = state.active_mission().expect("mission");

        assert_eq!(
            authenticated_checkpoint_task_id(
                &state,
                mission,
                &worker_thread_id.to_string(),
                "rampage-worker",
            )
            .expect("exact binding"),
            "task-work"
        );
        assert!(
            authenticated_checkpoint_task_id(
                &state,
                mission,
                &ThreadId::new().to_string(),
                "rampage-worker",
            )
            .is_err()
        );
        assert!(
            authenticated_checkpoint_task_id(
                &state,
                mission,
                &worker_thread_id.to_string(),
                "rampage-advisor",
            )
            .is_err()
        );

        let parent_thread_id = ThreadId::new();
        let worker_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("rampage-worker".to_string()),
        });
        assert_eq!(
            checkpoint_source_binding(&worker_source)
                .expect("trusted worker source")
                .0,
            parent_thread_id
        );
        assert!(checkpoint_source_binding(&SessionSource::Cli).is_err());
    }

    #[test]
    fn advisor_snapshot_includes_checkpoints_and_explicit_omitted_counts() {
        let mut state = RampageState::new("thread-1".to_string());
        let options = RampageToolOptions::new(ModeKind::AbsoluteRampage);
        handle_control_start(
            &mut state,
            &valid_start_args("new_ideas_only"),
            options,
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        for index in 0..=ADVISOR_ACTIVE_TASK_LIMIT {
            let task_id = format!("task-active-{index}");
            push_completed_mission_worker(&mut state, &mission_id, &task_id, 10 + index as i64);
            let task = state.tasks.last_mut().expect("worker task");
            task.status = "running".to_string();
            task.result = None;
            task.time_finished = None;
        }
        state
            .active_mission_mut()
            .expect("mission")
            .worker_checkpoints
            .insert(
                "task-active-0".to_string(),
                RampageWorkerCheckpoint {
                    revision: 1,
                    attempt: 2,
                    checkpoint: "inspected parser and isolated the failure".to_string(),
                    blocker: Some("waiting for fixture".to_string()),
                    next_action: "run the focused fixture".to_string(),
                    time_updated: 20,
                },
            );
        let mission = state.active_mission().expect("mission").clone();
        let args = RampageSpawnArgs {
            task_name: "new_ideas_agent".to_string(),
            title: "Review workers".to_string(),
            instructions: "Return steering.".to_string(),
            kind: Some("review".to_string()),
            role: Some("New Ideas Agent".to_string()),
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
            "task-advisor",
            &args,
            "New Ideas Agent",
            options,
        );
        assert!(brief.contains("attempt=2"));
        assert!(brief.contains("inspected parser and isolated the failure"));
        assert!(brief.contains("active_omitted=1"));
        assert!(brief.contains("terminal_omitted=0"));
    }

    #[test]
    fn verifier_coverage_manifest_is_explicit_and_overflow_fails_closed() {
        let mut state = RampageState::new("thread-1".to_string());
        let options = RampageToolOptions::new(ModeKind::AbsoluteRampage);
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            options,
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        for index in 0..=VERIFIER_EVIDENCE_TASK_LIMIT {
            push_completed_mission_worker(
                &mut state,
                &mission_id,
                &format!("task-work-{index}"),
                10 + index as i64,
            );
        }
        let mission = state.active_mission().expect("mission").clone();
        let args = RampageSpawnArgs {
            task_name: "verifier".to_string(),
            title: "Verify mission".to_string(),
            instructions: "Score all criteria.".to_string(),
            kind: Some("verify".to_string()),
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
            "task-verify-current",
            &args,
            "Verifier",
            options,
        );

        assert!(brief.contains("eligible=13 injected=12 omitted=1"));
        assert!(brief.contains("superseded_advisors=0 limit=12"));
        assert!(brief.contains("Complete manifest:"));
        assert!(brief.contains("task-work-0:work:done"));
        assert!(brief.contains("task-work-12:work:done"));
        let err = ensure_verifier_evidence_coverage(&state, &mission, "task-verify-current")
            .expect_err("omitted authenticated evidence must fail closed");
        assert!(err.to_string().contains("fails closed"));
    }

    #[test]
    fn evidence_window_reserves_selected_support_slots_before_spawn() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("both"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        for index in 0..9 {
            push_revisioned_mission_worker(&mut state, &mission_id, &format!("task-work-{index}"));
        }
        let mission = state.active_mission().expect("mission").clone();
        ensure_evidence_window_capacity_for_spawn(&state, &mission, "work", None)
            .expect("nine workers plus one more leave both advisor slots reserved");

        push_revisioned_mission_worker(&mut state, &mission_id, "task-work-9");
        let mission = state.active_mission().expect("mission").clone();
        let err = ensure_evidence_window_capacity_for_spawn(&state, &mission, "work", None)
            .expect_err("an eleventh worker would consume a selected advisor slot");
        assert!(
            err.to_string()
                .contains("reserves 2 selected support-agent slot")
        );
        ensure_evidence_window_capacity_for_spawn(
            &state,
            &mission,
            "research",
            Some(SUPPORT_AGENT_NEW_IDEAS),
        )
        .expect("the reserved advisor slot must remain usable");
    }

    #[test]
    fn failed_verifier_round_retires_its_exact_bounded_window() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        for index in 0..VERIFIER_EVIDENCE_TASK_LIMIT {
            push_revisioned_mission_worker(
                &mut state,
                &mission_id,
                &format!("task-window-{index}"),
            );
        }
        push_verify_task_for_current_revision(
            &mut state,
            &mission_id,
            "task-verify-window-1",
            50.0,
            "Corrective work is required.",
        );
        handle_control_verify_result(
            &mut state,
            &RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(50.0),
                verify_task_id: Some("task-verify-window-1".to_string()),
                verifier_notes: Some("Corrective work is required.".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect("bounded failed round should be authenticated");

        let mission = state.active_mission().expect("mission").clone();
        assert_eq!(
            mission
                .verifier_continuity
                .as_ref()
                .expect("continuity")
                .reviewed_through_revision,
            VERIFIER_EVIDENCE_TASK_LIMIT as u64
        );
        assert!(verifier_evidence_tasks(&state, &mission, None).is_empty());
        ensure_evidence_window_capacity_for_spawn(&state, &mission, "work", None)
            .expect("reviewed evidence must not permanently consume the next window");
    }

    #[test]
    fn failed_round_requires_new_substantive_evidence_not_advice_only() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("new_ideas_only"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_revisioned_mission_worker(&mut state, &mission_id, "task-initial");
        push_revisioned_support_agent(
            &mut state,
            &mission_id,
            SUPPORT_AGENT_NEW_IDEAS,
            "task-ideas-1",
        );
        push_verify_task_for_current_revision(
            &mut state,
            &mission_id,
            "task-verify-advice-1",
            40.0,
            "A corrective implementation is missing.",
        );
        handle_control_verify_result(
            &mut state,
            &RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(40.0),
                verify_task_id: Some("task-verify-advice-1".to_string()),
                verifier_notes: Some("A corrective implementation is missing.".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect("first verifier round should be recorded");

        push_revisioned_support_agent(
            &mut state,
            &mission_id,
            SUPPORT_AGENT_NEW_IDEAS,
            "task-ideas-2",
        );
        let mission = state.active_mission().expect("mission").clone();
        let err = ensure_required_new_substantive_evidence(&state, &mission)
            .expect_err("advisory-only retries must not satisfy a failed verifier");
        assert!(err.to_string().contains("fresh authenticated substantive"));

        push_revisioned_mission_worker(&mut state, &mission_id, "task-corrective");
        let mission = state.active_mission().expect("mission").clone();
        ensure_required_new_substantive_evidence(&state, &mission)
            .expect("fresh corrective worker evidence should unlock the next round");
    }

    #[test]
    fn next_verifier_brief_has_only_unreviewed_evidence_and_prior_continuity() {
        let mut state = RampageState::new("thread-1".to_string());
        let options = RampageToolOptions::new(ModeKind::AbsoluteRampage);
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            options,
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_revisioned_mission_worker(&mut state, &mission_id, "task-initial");
        push_verify_task_for_current_revision(
            &mut state,
            &mission_id,
            "task-verify-brief-1",
            30.0,
            "The parser fix is still missing.",
        );
        handle_control_verify_result(
            &mut state,
            &RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(30.0),
                verify_task_id: Some("task-verify-brief-1".to_string()),
                verifier_notes: Some("The parser fix is still missing.".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect("first failed round should be recorded");
        push_revisioned_mission_worker(&mut state, &mission_id, "task-corrective");

        let mission = state.active_mission().expect("mission").clone();
        let args = RampageSpawnArgs {
            task_name: "verifier".to_string(),
            title: "Verify corrective round".to_string(),
            instructions: "Score all criteria against the new evidence.".to_string(),
            kind: Some("verify".to_string()),
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
            "task-verify-brief-2",
            &args,
            "Verifier",
            options,
        );

        assert!(brief.contains("eligible=1 injected=1 omitted=0"));
        assert!(brief.contains("task-corrective:work:done"));
        assert!(!brief.contains("task-initial:work:done"));
        assert!(brief.contains("Previous authenticated verifier continuity"));
        assert!(brief.contains("The parser fix is still missing."));
        ensure_verifier_evidence_coverage(&state, &mission, "task-verify-brief-2")
            .expect("the complete corrective window should fit without omission");
    }

    #[test]
    fn failed_round_can_recover_with_corrective_evidence_and_later_pass() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_revisioned_mission_worker(&mut state, &mission_id, "task-initial");
        push_verify_task_for_current_revision(
            &mut state,
            &mission_id,
            "task-verify-recovery-1",
            50.0,
            "One criterion remains unmet.",
        );
        handle_control_verify_result(
            &mut state,
            &RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(50.0),
                verify_task_id: Some("task-verify-recovery-1".to_string()),
                verifier_notes: Some("One criterion remains unmet.".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect("first round should fail without deadlocking the mission");

        push_revisioned_mission_worker(&mut state, &mission_id, "task-corrective");
        push_verify_task_for_current_revision(
            &mut state,
            &mission_id,
            "task-verify-recovery-2",
            100.0,
            "All criteria now have authenticated evidence.",
        );
        handle_control_verify_result(
            &mut state,
            &RampageControlArgs {
                action: "verify_result".to_string(),
                pass_percentage: Some(100.0),
                verify_task_id: Some("task-verify-recovery-2".to_string()),
                verifier_notes: Some("All criteria now have authenticated evidence.".to_string()),
                ..Default::default()
            },
            None,
        )
        .expect("fresh corrective evidence should permit a later pass");
        handle_control_complete(
            &mut state,
            &RampageControlArgs {
                action: "complete".to_string(),
                ..Default::default()
            },
        )
        .expect("the later authenticated pass should complete the mission");
        assert_eq!(state.active_mission().expect("mission").status, "completed");
    }

    #[test]
    fn orphan_reconciliation_only_fails_safely_unbound_active_tasks() {
        let mut state = RampageState::new("thread-1".to_string());
        handle_control_start(
            &mut state,
            &valid_start_args("none"),
            RampageToolOptions::new(ModeKind::AbsoluteRampage),
            "thread-1".to_string(),
        )
        .expect("mission should start");
        let mission_id = state.active_mission().expect("mission").id.clone();
        push_completed_mission_worker(&mut state, &mission_id, "task-partial", 2);
        let partial = state.tasks.last_mut().expect("partial task");
        partial.status = "running".to_string();
        partial.result = None;
        partial.time_finished = None;
        push_completed_mission_worker(&mut state, &mission_id, "task-recent", 2);
        let recent = state.tasks.last_mut().expect("recent task");
        recent.status = "running".to_string();
        recent.worker_session_id = None;
        recent.result = None;
        recent.time_created = 100;
        recent.time_started = Some(100);
        recent.time_finished = None;
        push_completed_mission_worker(&mut state, &mission_id, "task-bound", 2);
        let bound = state.tasks.last_mut().expect("bound task");
        bound.status = "running".to_string();
        bound.result = None;
        bound.time_finished = None;
        state
            .active_mission_mut()
            .expect("mission")
            .worker_thread_ids
            .insert("task-bound".to_string(), ThreadId::new().to_string());

        assert!(reconcile_unbound_active_tasks(&mut state, 101));
        assert_eq!(
            state
                .tasks
                .iter()
                .find(|task| task.id == "task-partial")
                .expect("partial")
                .status,
            "failed"
        );
        assert_eq!(
            state
                .tasks
                .iter()
                .find(|task| task.id == "task-recent")
                .expect("recent")
                .status,
            "running"
        );
        assert_eq!(
            state
                .tasks
                .iter()
                .find(|task| task.id == "task-bound")
                .expect("bound")
                .status,
            "running"
        );
    }

    #[tokio::test]
    async fn durable_attestation_authenticates_an_evicted_worker_result() {
        let home = std::env::temp_dir().join(format!("rampage-test-{}", Uuid::new_v4()));
        let state_path = rampage_state_file_path(&home, "thread-1");
        let path = rampage_attestation_file_path(&state_path, "mission-1", "task-1");
        let worker_thread_id = ThreadId::new();
        let attestation = terminal_attestation_from_status(
            "mission-1",
            "task-1",
            worker_thread_id,
            &AgentStatus::Completed(Some("authenticated output".to_string())),
        )
        .expect("terminal attestation");
        save_worker_attestation(&path, &attestation)
            .await
            .expect("save attestation");
        let loaded = load_worker_attestation(&path)
            .await
            .expect("load attestation");

        assert_eq!(loaded.worker_thread_id, worker_thread_id.to_string());
        assert_eq!(
            authoritative_attested_worker_result(&loaded, "done").expect("attested done result"),
            "authenticated output"
        );
        assert!(authoritative_attested_worker_result(&loaded, "failed").is_err());
        let lost = lost_worker_attestation(
            "mission-1",
            "task-1",
            worker_thread_id,
            "status channel closed",
        );
        assert_eq!(
            authoritative_attested_worker_result(&lost, "failed")
                .expect("lost worker is an authenticated failure"),
            "status channel closed"
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
