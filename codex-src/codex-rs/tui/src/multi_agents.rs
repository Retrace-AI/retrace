//! Helpers for rendering and navigating multi-agent state in the TUI.
//!
//! This module owns the shared presentation contracts for multi-agent history rows, `/agent` picker
//! entries, and the fast-switch keyboard shortcuts. Higher-level coordination, such as deciding
//! which thread becomes active or when a thread closes, stays in [`crate::app::App`].

use crate::history_cell::PlainHistoryCell;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::render::line_utils::prefix_lines;
use crate::render::renderable::Renderable;
use crate::text_formatting::truncate_text;
use crate::tui::FrameRequester;
use codex_app_server_protocol::CollabAgentState;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::ThreadItem;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
#[cfg(target_os = "macos")]
use crossterm::event::KeyEventKind;
#[cfg(target_os = "macos")]
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

const COLLAB_PROMPT_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES: usize = 240;
const LIVE_AGENT_DETAIL_PREVIEW_GRAPHEMES: usize = 180;
// Show every running agent. Rampage/Readonly Research missions routinely run more
// than a handful of workers plus support and verifier agents, and hiding any of them
// makes the mission look smaller than it is. A generous ceiling still guards against a
// pathological runaway from blowing up the footer height.
const LIVE_AGENT_MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPickerThreadEntry {
    /// Human-friendly nickname shown in picker rows and footer labels.
    pub(crate) agent_nickname: Option<String>,
    /// Agent type shown in brackets when present, for example `worker`.
    pub(crate) agent_role: Option<String>,
    /// Whether the thread has emitted a close event and should render dimmed.
    pub(crate) is_closed: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentMetadata {
    /// Human-friendly nickname shown in rendered tool-call rows.
    pub(crate) agent_nickname: Option<String>,
    /// Agent type shown in brackets when present, for example `worker`.
    pub(crate) agent_role: Option<String>,
}

#[derive(Clone, Copy)]
struct AgentLabel<'a> {
    thread_id: Option<ThreadId>,
    nickname: Option<&'a str>,
    role: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequestSummary {
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffortConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveAgentStatus {
    waiting: bool,
    entries: Vec<LiveAgentStatusEntry>,
    spawn_call_entries: HashMap<String, usize>,
    thread_entries: HashMap<ThreadId, usize>,
    thread_activity_item_ids: HashMap<ThreadId, String>,
    thread_activity_buffers: HashMap<ThreadId, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveAgentStatusEntry {
    thread_id: Option<ThreadId>,
    label: String,
    stage: LiveAgentStage,
    detail: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffortConfig>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveAgentStage {
    Spawning,
    Spawned,
    Running,
    Waiting,
    Completed,
    Failed,
    Interrupted,
    Closed,
    Resuming,
    Shutdown,
}

pub(crate) struct LiveAgentStatusPanel {
    status: LiveAgentStatus,
    frame_requester: FrameRequester,
    animations_enabled: bool,
    animation_started_at: Instant,
}

pub(crate) fn agent_picker_status_dot_spans(is_closed: bool) -> Vec<Span<'static>> {
    let dot = if is_closed {
        "•".into()
    } else {
        "•".green()
    };
    vec![dot, " ".into()]
}

pub(crate) fn format_agent_picker_item_name(
    agent_nickname: Option<&str>,
    agent_role: Option<&str>,
    is_primary: bool,
) -> String {
    if is_primary {
        return "Main [default]".to_string();
    }

    let agent_nickname = agent_nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let agent_role = agent_role.map(str::trim).filter(|role| !role.is_empty());
    match (agent_nickname, agent_role) {
        (Some(agent_nickname), Some(agent_role)) => format!("{agent_nickname} [{agent_role}]"),
        (Some(agent_nickname), None) => agent_nickname.to_string(),
        (None, Some(agent_role)) => format!("[{agent_role}]"),
        (None, None) => "Agent".to_string(),
    }
}

pub(crate) fn previous_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Left)
}

pub(crate) fn next_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Right)
}

/// Matches the canonical "previous agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn previous_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    previous_agent_shortcut().is_press(key_event)
        || previous_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

/// Matches the canonical "next agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn next_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    next_agent_shortcut().is_press(key_event)
        || next_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

#[cfg(target_os = "macos")]
fn previous_agent_word_motion_fallback(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn previous_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn next_agent_word_motion_fallback(key_event: KeyEvent, allow_word_motion_fallback: bool) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn next_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

pub(crate) fn spawn_request_summary(item: &ThreadItem) -> Option<SpawnRequestSummary> {
    match item {
        ThreadItem::CollabAgentToolCall {
            tool: CollabAgentTool::SpawnAgent,
            model: Some(model),
            reasoning_effort: Some(reasoning_effort),
            ..
        } => Some(SpawnRequestSummary {
            model: model.clone(),
            reasoning_effort: reasoning_effort.clone(),
        }),
        _ => None,
    }
}

pub(crate) fn should_render_collab_tool_call_in_history(
    tool: &CollabAgentTool,
    active_turn_live_status: bool,
) -> bool {
    if !active_turn_live_status {
        return true;
    }

    !matches!(
        tool,
        CollabAgentTool::SpawnAgent
            | CollabAgentTool::SendInput
            | CollabAgentTool::ResumeAgent
            | CollabAgentTool::Wait
            | CollabAgentTool::CloseAgent
    )
}

impl LiveAgentStatus {
    pub(crate) fn is_empty(&self) -> bool {
        !self.waiting && self.entries.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.waiting = false;
        self.entries.clear();
        self.spawn_call_entries.clear();
        self.thread_entries.clear();
        self.thread_activity_item_ids.clear();
        self.thread_activity_buffers.clear();
    }

    pub(crate) fn update_metadata(&mut self, thread_id: ThreadId, metadata: AgentMetadata) -> bool {
        let Some(index) = self.thread_entries.get(&thread_id).copied() else {
            return false;
        };
        let label = metadata_label(&metadata);
        let Some(label) = label else {
            return false;
        };
        if self.entries[index].label == label {
            return false;
        }
        self.entries[index].label = label;
        true
    }

    pub(crate) fn apply_tool_call(
        &mut self,
        item: &ThreadItem,
        cached_spawn_request: Option<&SpawnRequestSummary>,
        mut agent_metadata: impl FnMut(ThreadId) -> AgentMetadata,
    ) {
        let ThreadItem::CollabAgentToolCall {
            id,
            tool,
            status,
            receiver_thread_ids,
            prompt,
            agents_states,
            ..
        } = item
        else {
            return;
        };

        let prompt = prompt.as_deref().unwrap_or_default();
        match tool {
            CollabAgentTool::SpawnAgent => {
                let fallback_spawn_request = spawn_request_summary(item);
                let spawn_request = cached_spawn_request.or(fallback_spawn_request.as_ref());
                self.apply_spawn_tool_call(
                    id,
                    status,
                    receiver_thread_ids,
                    prompt,
                    agents_states,
                    spawn_request,
                    &mut agent_metadata,
                );
            }
            CollabAgentTool::SendInput => {
                for thread_id in receiver_thread_ids
                    .iter()
                    .filter_map(|id| parse_thread_id(id))
                {
                    let metadata = agent_metadata(thread_id);
                    let index = self.ensure_thread_entry(thread_id, metadata, prompt);
                    self.entries[index].stage =
                        if matches!(status, CollabAgentToolCallStatus::Failed) {
                            LiveAgentStage::Failed
                        } else {
                            LiveAgentStage::Running
                        };
                    self.entries[index].detail = prompt_preview(prompt);
                }
                self.apply_agent_states(receiver_thread_ids, agents_states, &mut agent_metadata);
            }
            CollabAgentTool::ResumeAgent => {
                for thread_id in receiver_thread_ids
                    .iter()
                    .filter_map(|id| parse_thread_id(id))
                {
                    let metadata = agent_metadata(thread_id);
                    let index = self.ensure_thread_entry(thread_id, metadata, prompt);
                    self.entries[index].stage =
                        if matches!(status, CollabAgentToolCallStatus::Failed) {
                            LiveAgentStage::Failed
                        } else if matches!(status, CollabAgentToolCallStatus::InProgress) {
                            LiveAgentStage::Resuming
                        } else {
                            LiveAgentStage::Running
                        };
                    if let Some(detail) = prompt_preview(prompt) {
                        self.entries[index].detail = Some(detail);
                    }
                }
                self.apply_agent_states(receiver_thread_ids, agents_states, &mut agent_metadata);
            }
            CollabAgentTool::Wait => {
                self.waiting = matches!(status, CollabAgentToolCallStatus::InProgress);
                if self.waiting {
                    for thread_id in receiver_thread_ids
                        .iter()
                        .filter_map(|id| parse_thread_id(id))
                    {
                        let metadata = agent_metadata(thread_id);
                        let index = self.ensure_thread_entry(thread_id, metadata, prompt);
                        if matches!(self.entries[index].stage, LiveAgentStage::Spawned) {
                            self.entries[index].stage = LiveAgentStage::Waiting;
                        }
                    }
                }
                self.apply_agent_states(receiver_thread_ids, agents_states, &mut agent_metadata);
            }
            CollabAgentTool::CloseAgent => {
                for thread_id in receiver_thread_ids
                    .iter()
                    .filter_map(|id| parse_thread_id(id))
                {
                    let metadata = agent_metadata(thread_id);
                    let index = self.ensure_thread_entry(thread_id, metadata, prompt);
                    self.entries[index].stage =
                        if matches!(status, CollabAgentToolCallStatus::Failed) {
                            LiveAgentStage::Failed
                        } else {
                            LiveAgentStage::Closed
                        };
                }
                self.apply_agent_states(receiver_thread_ids, agents_states, &mut agent_metadata);
            }
        }
    }

    pub(crate) fn mark_agent_activity_started(
        &mut self,
        thread_id: ThreadId,
        metadata: AgentMetadata,
    ) -> bool {
        let index = self.ensure_thread_entry(thread_id, metadata, "");
        let entry = &mut self.entries[index];
        let changed = entry.stage != LiveAgentStage::Running
            || entry.detail.as_deref() != Some("LLM call started");
        entry.stage = LiveAgentStage::Running;
        entry.detail = Some("LLM call started".to_string());
        self.thread_activity_item_ids.remove(&thread_id);
        self.thread_activity_buffers.remove(&thread_id);
        changed
    }

    pub(crate) fn apply_agent_activity_delta(
        &mut self,
        thread_id: ThreadId,
        metadata: AgentMetadata,
        item_id: &str,
        delta: &str,
    ) -> bool {
        if delta.is_empty() {
            return false;
        }

        let index = self.ensure_thread_entry(thread_id, metadata, "");
        let current_item_id = self.thread_activity_item_ids.get(&thread_id);
        if current_item_id.is_none_or(|current| current != item_id) {
            self.thread_activity_item_ids
                .insert(thread_id, item_id.to_string());
            self.thread_activity_buffers
                .insert(thread_id, String::new());
        }

        let buffer = self.thread_activity_buffers.entry(thread_id).or_default();
        buffer.push_str(delta);
        let Some(activity) = latest_agent_activity_preview(buffer) else {
            return false;
        };

        let entry = &mut self.entries[index];
        let changed = entry.stage != LiveAgentStage::Running
            || entry.detail.as_deref() != Some(activity.as_str());
        entry.stage = LiveAgentStage::Running;
        entry.detail = Some(activity);
        changed
    }

    fn apply_spawn_tool_call(
        &mut self,
        call_id: &str,
        status: &CollabAgentToolCallStatus,
        receiver_thread_ids: &[String],
        prompt: &str,
        agents_states: &HashMap<String, CollabAgentState>,
        spawn_request: Option<&SpawnRequestSummary>,
        agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
    ) {
        let label = spawn_prompt_label(prompt);
        let detail = prompt_preview(prompt);
        let pending_index = self.spawn_call_entries.get(call_id).copied();

        if matches!(status, CollabAgentToolCallStatus::InProgress) {
            let index = pending_index.unwrap_or_else(|| {
                let index = self.entries.len();
                self.entries.push(LiveAgentStatusEntry {
                    thread_id: None,
                    label: label.clone(),
                    stage: LiveAgentStage::Spawning,
                    detail: detail.clone(),
                    model: spawn_request.map(|request| request.model.clone()),
                    reasoning_effort: spawn_request.map(|request| request.reasoning_effort.clone()),
                });
                self.spawn_call_entries.insert(call_id.to_string(), index);
                index
            });
            self.entries[index].stage = LiveAgentStage::Spawning;
            self.entries[index].label = label;
            self.entries[index].detail = detail;
            if let Some(request) = spawn_request {
                self.entries[index].model = Some(request.model.clone());
                self.entries[index].reasoning_effort = Some(request.reasoning_effort.clone());
            }
            return;
        }

        let first_thread_id = receiver_thread_ids
            .first()
            .and_then(|id| parse_thread_id(id));
        let index = match (pending_index, first_thread_id) {
            (Some(index), Some(thread_id)) => {
                if let Some(existing) = self.thread_entries.get(&thread_id).copied()
                    && existing != index
                {
                    // The child's own activity (ThreadStarted/deltas) can reach the
                    // panel before this SpawnEnd and create a live entry for the
                    // thread. Blindly remapping the thread to the pending spawn row
                    // would orphan that live entry as a duplicate stuck on
                    // "running". Keep the live entry and fold the pending spawn row
                    // into it instead.
                    if self.entries[existing].model.is_none() {
                        self.entries[existing].model = self.entries[index].model.clone();
                    }
                    if self.entries[existing].reasoning_effort.is_none() {
                        self.entries[existing].reasoning_effort =
                            self.entries[index].reasoning_effort.clone();
                    }
                    self.remove_entry(index);
                    let existing = if existing > index { existing - 1 } else { existing };
                    self.entries[existing].thread_id = Some(thread_id);
                    existing
                } else {
                    self.entries[index].thread_id = Some(thread_id);
                    self.thread_entries.insert(thread_id, index);
                    index
                }
            }
            (Some(index), None) => index,
            (None, Some(thread_id)) => {
                let metadata = agent_metadata(thread_id);
                self.ensure_thread_entry(thread_id, metadata, prompt)
            }
            (None, None) => {
                let index = self.entries.len();
                self.entries.push(LiveAgentStatusEntry {
                    thread_id: None,
                    label: label.clone(),
                    stage: LiveAgentStage::Spawning,
                    detail: detail.clone(),
                    model: spawn_request.map(|request| request.model.clone()),
                    reasoning_effort: spawn_request.map(|request| request.reasoning_effort.clone()),
                });
                index
            }
        };
        self.spawn_call_entries.remove(call_id);

        if let Some(thread_id) = first_thread_id {
            let metadata = agent_metadata(thread_id);
            if let Some(metadata_label) = metadata_label(&metadata) {
                self.entries[index].label = metadata_label;
            } else if self.entries[index].label.trim().is_empty() {
                self.entries[index].label = label;
            }
        } else {
            self.entries[index].label = label;
        }
        self.entries[index].detail = detail;
        if let Some(request) = spawn_request {
            self.entries[index].model = Some(request.model.clone());
            self.entries[index].reasoning_effort = Some(request.reasoning_effort.clone());
        }
        self.entries[index].stage = if matches!(status, CollabAgentToolCallStatus::Failed) {
            LiveAgentStage::Failed
        } else {
            LiveAgentStage::Spawned
        };
        self.apply_agent_states(receiver_thread_ids, agents_states, agent_metadata);
    }

    fn apply_agent_states(
        &mut self,
        receiver_thread_ids: &[String],
        agents_states: &HashMap<String, CollabAgentState>,
        agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
    ) {
        let mut seen = HashSet::new();
        for thread_id in receiver_thread_ids
            .iter()
            .filter_map(|id| parse_thread_id(id))
        {
            seen.insert(thread_id);
            if let Some(status) = agents_states.get(&thread_id.to_string()) {
                let metadata = agent_metadata(thread_id);
                self.apply_agent_state(thread_id, metadata, status);
            }
        }

        let mut extras = agents_states
            .iter()
            .filter_map(|(thread_id, status)| {
                let thread_id = parse_thread_id(thread_id)?;
                (!seen.contains(&thread_id)).then_some((thread_id, status))
            })
            .collect::<Vec<_>>();
        extras.sort_by_key(|entry| entry.0.to_string());
        for (thread_id, status) in extras {
            let metadata = agent_metadata(thread_id);
            self.apply_agent_state(thread_id, metadata, status);
        }
    }

    fn apply_agent_state(
        &mut self,
        thread_id: ThreadId,
        metadata: AgentMetadata,
        status: &CollabAgentState,
    ) {
        let index = self.ensure_thread_entry(thread_id, metadata, "");
        self.entries[index].stage = live_stage_from_status(&status.status);
        if let Some(message) = status.message.as_deref() {
            let message = truncate_text(
                &message.split_whitespace().collect::<Vec<_>>().join(" "),
                LIVE_AGENT_DETAIL_PREVIEW_GRAPHEMES,
            );
            if !message.is_empty() {
                self.entries[index].detail = Some(message);
            }
        }
    }

    /// Removes an entry and repairs every stored index in both lookup maps.
    /// Entries are addressed by `Vec` position, so removal shifts everything
    /// after `index` down by one.
    fn remove_entry(&mut self, index: usize) {
        self.entries.remove(index);
        self.spawn_call_entries.retain(|_, i| *i != index);
        for i in self.spawn_call_entries.values_mut() {
            if *i > index {
                *i -= 1;
            }
        }
        self.thread_entries.retain(|_, i| *i != index);
        for i in self.thread_entries.values_mut() {
            if *i > index {
                *i -= 1;
            }
        }
    }

    fn ensure_thread_entry(
        &mut self,
        thread_id: ThreadId,
        metadata: AgentMetadata,
        prompt: &str,
    ) -> usize {
        if let Some(index) = self.thread_entries.get(&thread_id).copied() {
            if let Some(label) = metadata_label(&metadata) {
                self.entries[index].label = label;
            }
            return index;
        }

        let index = self.entries.len();
        self.thread_entries.insert(thread_id, index);
        self.entries.push(LiveAgentStatusEntry {
            thread_id: Some(thread_id),
            label: metadata_label(&metadata).unwrap_or_else(|| spawn_prompt_label(prompt)),
            stage: LiveAgentStage::Running,
            detail: prompt_preview(prompt),
            model: None,
            reasoning_effort: None,
        });
        index
    }
}

impl LiveAgentStatusPanel {
    pub(crate) fn new(frame_requester: FrameRequester, animations_enabled: bool) -> Self {
        Self {
            status: LiveAgentStatus::default(),
            frame_requester,
            animations_enabled,
            animation_started_at: Instant::now(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.status.is_empty()
    }

    pub(crate) fn set_status(&mut self, status: LiveAgentStatus) -> bool {
        if self.status == status {
            return false;
        }
        self.status = status;
        true
    }

    pub(crate) fn clear(&mut self) -> bool {
        if self.status.is_empty() {
            return false;
        }
        self.status.clear();
        true
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.status.waiting {
            // Only count agents that are still active. Completed/failed/closed agents
            // remain listed (so their final state is visible) but must not inflate the
            // "Waiting for N agents" headline — otherwise a wait on six agents where
            // five have already finished still reads "Waiting for 6 agents".
            let waiting_count = self
                .status
                .entries
                .iter()
                .filter(|entry| live_stage_is_active(entry.stage))
                .count();
            lines.push(waiting_live_line(
                waiting_count,
                self.animations_enabled,
                self.animation_started_at,
            ));
        }

        for entry in self.status.entries.iter().take(LIVE_AGENT_MAX_ENTRIES) {
            lines.push(live_agent_entry_line(
                entry,
                self.animations_enabled,
                self.animation_started_at,
            ));
        }

        let hidden = self
            .status
            .entries
            .len()
            .saturating_sub(LIVE_AGENT_MAX_ENTRIES);
        if hidden > 0 {
            lines.push(Line::from(vec![
                "  ".into(),
                format!("+{hidden} more agents").dim(),
            ]));
        }

        lines
    }
}

impl Renderable for LiveAgentStatusPanel {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.status.is_empty() {
            return;
        }
        if self.animations_enabled
            && (self.status.waiting
                || self
                    .status
                    .entries
                    .iter()
                    .any(|entry| live_stage_is_active(entry.stage)))
        {
            self.frame_requester
                .schedule_frame_in(Duration::from_millis(80));
        }

        let lines = self
            .lines()
            .into_iter()
            .take(usize::from(area.height))
            .map(|line| truncate_line_with_ellipsis_if_overflow(line, usize::from(area.width)))
            .collect::<Vec<_>>();
        Paragraph::new(lines).render(area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        u16::try_from(self.lines().len()).unwrap_or(u16::MAX)
    }
}

fn metadata_label(metadata: &AgentMetadata) -> Option<String> {
    let nickname = metadata
        .agent_nickname
        .as_deref()
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = metadata
        .agent_role
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());

    match (nickname, role) {
        (Some(nickname), Some(role)) => Some(format!("{nickname} [{role}]")),
        (Some(nickname), None) => Some(nickname.to_string()),
        (None, Some(role)) => Some(format!("[{role}]")),
        (None, None) => None,
    }
}

fn prompt_preview(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return None;
    }
    if task_name_from_prompt(trimmed).is_some() {
        return None;
    }
    Some(truncate_text(
        &trimmed.split_whitespace().collect::<Vec<_>>().join(" "),
        LIVE_AGENT_DETAIL_PREVIEW_GRAPHEMES,
    ))
    .filter(|preview| !preview.is_empty())
}

fn spawn_prompt_label(prompt: &str) -> String {
    if let Some(task_name) = task_name_from_prompt(prompt) {
        return task_name_label(task_name);
    }

    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !line.starts_with("```")
                && !line.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        })
        .unwrap_or("Worker");
    let lower = first_line.to_ascii_lowercase();
    let candidate = if (lower.starts_with("connect ") || lower.starts_with("ssh "))
        && let Some(idx) = lower.find(" and ")
    {
        &first_line[idx + " and ".len()..]
    } else if lower.starts_with("please ") {
        &first_line["please ".len()..]
    } else {
        first_line
    };
    let mut end = candidate.len();
    for marker in [":", ".", "\n"] {
        if let Some(idx) = candidate.find(marker) {
            end = end.min(idx);
        }
    }
    let label = candidate[..end].trim();
    let label = label
        .strip_prefix("to ")
        .unwrap_or(label)
        .strip_prefix("the ")
        .unwrap_or(label)
        .trim();
    let label = if label.is_empty() { "Worker" } else { label };
    capitalize_label(&truncate_text(label, 48))
}

fn task_name_from_prompt(prompt: &str) -> Option<&str> {
    let trimmed = prompt.trim();
    trimmed
        .strip_prefix("task:")
        .or_else(|| trimmed.strip_prefix("Task:"))
        .map(str::trim)
        .filter(|task_name| !task_name.is_empty())
}

fn task_name_label(task_name: &str) -> String {
    let name = task_name
        .trim()
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(task_name)
        .trim();
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    let agent_name = match normalized.as_str() {
        "system_info" => "System Exploration Agent".to_string(),
        "service_info" | "services_info" => "Services Inspection Agent".to_string(),
        "config_info" => "Configuration Analysis Agent".to_string(),
        "network_info" | "network_scan" | "network_sniffing" => {
            "Network Sniffing Agent".to_string()
        }
        "discover_models" => "Model Discovery Agent".to_string(),
        "benchmark_tools" => "Benchmarking Agent".to_string(),
        _ => generic_task_agent_label(name),
    };
    truncate_text(&agent_name, 56)
}

fn capitalize_label(label: &str) -> String {
    let mut chars = label.chars();
    let Some(first) = chars.next() else {
        return "Agent".to_string();
    };
    first.to_uppercase().collect::<String>() + chars.as_str()
}

fn generic_task_agent_label(task_name: &str) -> String {
    let words = task_name
        .trim()
        .chars()
        .map(|ch| if ch == '_' || ch == '-' { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty() {
        return "Worker Agent".to_string();
    }

    let mut label = words.join(" ");
    if !label.to_ascii_lowercase().ends_with("agent") {
        label.push_str(" Agent");
    }
    label
}

fn latest_agent_activity_preview(buffer: &str) -> Option<String> {
    let activity = buffer
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| task_name_from_prompt(line).is_none())
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    if activity.is_empty() {
        return None;
    }
    Some(truncate_activity_tail(
        &activity,
        LIVE_AGENT_DETAIL_PREVIEW_GRAPHEMES,
    ))
    .filter(|preview| !preview.is_empty())
}

fn truncate_activity_tail(text: &str, max_graphemes: usize) -> String {
    if max_graphemes == 0 {
        return String::new();
    }
    let graphemes = UnicodeSegmentation::graphemes(text, true).collect::<Vec<_>>();
    if graphemes.len() <= max_graphemes {
        return text.to_string();
    }
    if max_graphemes <= 3 {
        return graphemes[graphemes.len().saturating_sub(max_graphemes)..].concat();
    }

    let keep = max_graphemes - 3;
    format!("...{}", graphemes[graphemes.len() - keep..].concat())
}

fn live_stage_from_status(status: &CollabAgentStatus) -> LiveAgentStage {
    match status {
        CollabAgentStatus::PendingInit => LiveAgentStage::Spawned,
        CollabAgentStatus::Running => LiveAgentStage::Running,
        CollabAgentStatus::Interrupted => LiveAgentStage::Interrupted,
        CollabAgentStatus::Completed => LiveAgentStage::Completed,
        CollabAgentStatus::Errored | CollabAgentStatus::NotFound => LiveAgentStage::Failed,
        CollabAgentStatus::Shutdown => LiveAgentStage::Shutdown,
    }
}

fn waiting_live_line(
    agent_count: usize,
    animations_enabled: bool,
    animation_started_at: Instant,
) -> Line<'static> {
    let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
    let mut spans = Vec::new();
    if let Some(indicator) = activity_indicator(
        Some(animation_started_at),
        motion_mode,
        ReducedMotionIndicator::StaticBullet,
    ) {
        spans.push(indicator);
        spans.push(" ".into());
    }
    let label = if agent_count == 1 {
        "Waiting for agent".to_string()
    } else if agent_count > 1 {
        format!("Waiting for {agent_count} agents")
    } else {
        "Waiting for agents".to_string()
    };
    spans.push(label.cyan().bold());
    spans.into()
}

fn live_agent_entry_line(
    entry: &LiveAgentStatusEntry,
    animations_enabled: bool,
    animation_started_at: Instant,
) -> Line<'static> {
    let motion_mode = MotionMode::from_animations_enabled(animations_enabled);
    let mut spans = Vec::new();
    if live_stage_is_active(entry.stage) {
        if let Some(indicator) = activity_indicator(
            Some(animation_started_at),
            motion_mode,
            ReducedMotionIndicator::StaticBullet,
        ) {
            spans.push(indicator);
            spans.push(" ".into());
        }
    } else {
        spans.push("• ".dim());
    }
    spans.push(entry.label.clone().cyan().bold());
    spans.push(" · ".dim());
    spans.push(stage_span(entry.stage));

    if let Some(activity) = live_agent_activity_text(entry) {
        spans.push(" · ".dim());
        spans.push(activity.dim());
    }
    spans.into()
}

fn live_agent_activity_text(entry: &LiveAgentStatusEntry) -> Option<String> {
    if let Some(detail) = entry
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .filter(|detail| task_name_from_prompt(detail).is_none())
    {
        return Some(detail.to_string());
    }

    match entry.stage {
        LiveAgentStage::Spawning => Some("starting worker".to_string()),
        LiveAgentStage::Spawned => Some("awaiting first LLM update".to_string()),
        LiveAgentStage::Running => Some("LLM call active".to_string()),
        LiveAgentStage::Waiting => Some("waiting for result".to_string()),
        LiveAgentStage::Resuming => Some("resuming LLM call".to_string()),
        LiveAgentStage::Completed
        | LiveAgentStage::Failed
        | LiveAgentStage::Interrupted
        | LiveAgentStage::Closed
        | LiveAgentStage::Shutdown => None,
    }
}

fn live_stage_is_active(stage: LiveAgentStage) -> bool {
    matches!(
        stage,
        LiveAgentStage::Spawning
            | LiveAgentStage::Spawned
            | LiveAgentStage::Running
            | LiveAgentStage::Waiting
            | LiveAgentStage::Resuming
    )
}

fn stage_span(stage: LiveAgentStage) -> Span<'static> {
    match stage {
        LiveAgentStage::Spawning => "spawning".yellow(),
        LiveAgentStage::Spawned => "spawned".green(),
        LiveAgentStage::Running => "running".cyan(),
        LiveAgentStage::Waiting => "waiting".cyan(),
        LiveAgentStage::Completed => "completed".green(),
        LiveAgentStage::Failed => "failed".red(),
        LiveAgentStage::Interrupted => "interrupted".yellow(),
        LiveAgentStage::Closed => "closed".dim(),
        LiveAgentStage::Resuming => "resuming".cyan(),
        LiveAgentStage::Shutdown => "shutdown".dim(),
    }
}

pub(crate) fn tool_call_history_cell(
    item: &ThreadItem,
    cached_spawn_request: Option<&SpawnRequestSummary>,
    mut agent_metadata: impl FnMut(ThreadId) -> AgentMetadata,
) -> Option<PlainHistoryCell> {
    let ThreadItem::CollabAgentToolCall {
        tool,
        status,
        receiver_thread_ids,
        prompt,
        agents_states,
        ..
    } = item
    else {
        return None;
    };

    let first_receiver = receiver_thread_ids
        .first()
        .and_then(|id| parse_thread_id(id));
    let prompt = prompt.as_deref().unwrap_or_default();

    match tool {
        CollabAgentTool::SpawnAgent => {
            if matches!(status, CollabAgentToolCallStatus::InProgress) {
                return Some(spawn_begin(prompt, spawn_request_summary(item).as_ref()));
            }
            let fallback_spawn_request = spawn_request_summary(item);
            let spawn_request = cached_spawn_request.or(fallback_spawn_request.as_ref());
            Some(spawn_end(
                first_receiver,
                prompt,
                spawn_request,
                &mut agent_metadata,
            ))
        }
        CollabAgentTool::SendInput => {
            if matches!(status, CollabAgentToolCallStatus::InProgress) {
                return None;
            }
            first_receiver.map(|receiver_thread_id| {
                interaction_end(receiver_thread_id, prompt, &mut agent_metadata)
            })
        }
        CollabAgentTool::ResumeAgent => first_receiver.map(|receiver_thread_id| {
            if matches!(status, CollabAgentToolCallStatus::InProgress) {
                resume_begin(receiver_thread_id, &mut agent_metadata)
            } else {
                let state = first_agent_state(receiver_thread_ids, agents_states);
                resume_end(
                    receiver_thread_id,
                    state,
                    "Agent resume failed",
                    &mut agent_metadata,
                )
            }
        }),
        CollabAgentTool::Wait => {
            if matches!(status, CollabAgentToolCallStatus::InProgress) {
                Some(waiting_begin(receiver_thread_ids, &mut agent_metadata))
            } else {
                Some(waiting_end(
                    receiver_thread_ids,
                    agents_states,
                    &mut agent_metadata,
                ))
            }
        }
        CollabAgentTool::CloseAgent => {
            if matches!(status, CollabAgentToolCallStatus::InProgress) {
                return None;
            }
            first_receiver
                .map(|receiver_thread_id| close_end(receiver_thread_id, &mut agent_metadata))
        }
    }
}

fn spawn_begin(prompt: &str, spawn_request: Option<&SpawnRequestSummary>) -> PlainHistoryCell {
    let mut title = vec![
        Span::from("Spawning ").bold(),
        Span::from("agent").cyan().bold(),
    ];
    title.extend(spawn_request_spans(spawn_request));

    let mut details = Vec::new();
    if let Some(line) = prompt_line(prompt) {
        details.push(line);
    }

    collab_event(title_spans_line(title), details)
}

fn spawn_end(
    new_thread_id: Option<ThreadId>,
    prompt: &str,
    spawn_request: Option<&SpawnRequestSummary>,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    let title = match new_thread_id {
        Some(thread_id) => title_with_agent(
            "Spawned",
            agent_label(thread_id, &agent_metadata(thread_id)),
            spawn_request,
        ),
        None => title_text("Agent spawn failed"),
    };

    let mut details = Vec::new();
    if let Some(line) = prompt_line(prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

fn interaction_end(
    receiver_thread_id: ThreadId,
    prompt: &str,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    let title = title_with_agent(
        "Sent input to",
        agent_label(receiver_thread_id, &agent_metadata(receiver_thread_id)),
        /*spawn_request*/ None,
    );

    let mut details = Vec::new();
    if let Some(line) = prompt_line(prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

fn waiting_begin(
    receiver_thread_ids: &[String],
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    let receiver_agents = receiver_thread_ids
        .iter()
        .filter_map(|thread_id| parse_thread_id(thread_id))
        .map(|thread_id| (thread_id, agent_metadata(thread_id)))
        .collect::<Vec<_>>();

    let title = match receiver_agents.as_slice() {
        [(thread_id, metadata)] => title_with_agent(
            "Waiting for",
            agent_label(*thread_id, metadata),
            /*spawn_request*/ None,
        ),
        [] => title_text("Waiting for agents"),
        _ => title_text(format!("Waiting for {} agents", receiver_agents.len())),
    };

    let details = if receiver_agents.len() > 1 {
        receiver_agents
            .iter()
            .map(|(thread_id, metadata)| agent_label_line(agent_label(*thread_id, metadata)))
            .collect()
    } else {
        Vec::new()
    };

    collab_event(title, details)
}

fn waiting_end(
    receiver_thread_ids: &[String],
    agents_states: &std::collections::HashMap<String, CollabAgentState>,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    let details = wait_complete_lines(receiver_thread_ids, agents_states, agent_metadata);
    collab_event(title_text("Finished waiting"), details)
}

fn close_end(
    receiver_thread_id: ThreadId,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    collab_event(
        title_with_agent(
            "Closed",
            agent_label(receiver_thread_id, &agent_metadata(receiver_thread_id)),
            /*spawn_request*/ None,
        ),
        Vec::new(),
    )
}

fn resume_begin(
    receiver_thread_id: ThreadId,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    collab_event(
        title_with_agent(
            "Resuming",
            agent_label(receiver_thread_id, &agent_metadata(receiver_thread_id)),
            /*spawn_request*/ None,
        ),
        Vec::new(),
    )
}

fn resume_end(
    receiver_thread_id: ThreadId,
    status: Option<&CollabAgentState>,
    fallback_error: &str,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> PlainHistoryCell {
    collab_event(
        title_with_agent(
            "Resumed",
            agent_label(receiver_thread_id, &agent_metadata(receiver_thread_id)),
            /*spawn_request*/ None,
        ),
        vec![status_summary_line(status, fallback_error)],
    )
}

fn collab_event(title: Line<'static>, details: Vec<Line<'static>>) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![title];
    if !details.is_empty() {
        lines.extend(prefix_lines(details, "  └ ".dim(), "    ".into()));
    }
    PlainHistoryCell::new(lines)
}

fn title_text(title: impl Into<String>) -> Line<'static> {
    title_spans_line(vec![Span::from(title.into()).bold()])
}

fn title_with_agent(
    prefix: &str,
    agent: AgentLabel<'_>,
    spawn_request: Option<&SpawnRequestSummary>,
) -> Line<'static> {
    let mut spans = vec![Span::from(format!("{prefix} ")).bold()];
    spans.extend(agent_label_spans(agent));
    spans.extend(spawn_request_spans(spawn_request));
    title_spans_line(spans)
}

fn title_spans_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    let mut title = Vec::with_capacity(spans.len() + 1);
    title.push(Span::from("• ").dim());
    title.append(&mut spans);
    title.into()
}

fn parse_thread_id(thread_id: &str) -> Option<ThreadId> {
    ThreadId::from_string(thread_id).ok()
}

fn agent_label(thread_id: ThreadId, metadata: &AgentMetadata) -> AgentLabel<'_> {
    AgentLabel {
        thread_id: Some(thread_id),
        nickname: metadata.agent_nickname.as_deref(),
        role: metadata.agent_role.as_deref(),
    }
}

fn agent_label_line(agent: AgentLabel<'_>) -> Line<'static> {
    agent_label_spans(agent).into()
}

fn agent_label_spans(agent: AgentLabel<'_>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let nickname = agent
        .nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = agent.role.map(str::trim).filter(|role| !role.is_empty());

    if let Some(nickname) = nickname {
        spans.push(Span::from(nickname.to_string()).cyan().bold());
    } else if let Some(thread_id) = agent.thread_id {
        spans.push(Span::from(thread_id.to_string()).cyan());
    } else {
        spans.push(Span::from("agent").cyan());
    }

    if let Some(role) = role {
        spans.push(Span::from(" ").dim());
        spans.push(Span::from(format!("[{role}]")));
    }

    spans
}

fn spawn_request_spans(spawn_request: Option<&SpawnRequestSummary>) -> Vec<Span<'static>> {
    let Some(spawn_request) = spawn_request else {
        return Vec::new();
    };

    let model = spawn_request.model.trim();
    if model.is_empty() && spawn_request.reasoning_effort == ReasoningEffortConfig::default() {
        return Vec::new();
    }

    let details = if model.is_empty() {
        format!("({})", spawn_request.reasoning_effort)
    } else {
        format!("({model} {})", spawn_request.reasoning_effort)
    };

    vec![Span::from(" ").dim(), Span::from(details).magenta()]
}

fn prompt_line(prompt: &str) -> Option<Line<'static>> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Line::from(Span::from(truncate_text(
            trimmed,
            COLLAB_PROMPT_PREVIEW_GRAPHEMES,
        ))))
    }
}

fn wait_complete_lines(
    receiver_thread_ids: &[String],
    agents_states: &std::collections::HashMap<String, CollabAgentState>,
    agent_metadata: &mut impl FnMut(ThreadId) -> AgentMetadata,
) -> Vec<Line<'static>> {
    let mut seen = HashSet::new();
    let mut entries = receiver_thread_ids
        .iter()
        .filter_map(|thread_id| {
            let parsed_thread_id = parse_thread_id(thread_id)?;
            let status = agents_states.get(thread_id)?;
            seen.insert(parsed_thread_id);
            Some((parsed_thread_id, agent_metadata(parsed_thread_id), status))
        })
        .collect::<Vec<_>>();

    let mut extras = agents_states
        .iter()
        .filter_map(|(thread_id, status)| {
            let parsed_thread_id = parse_thread_id(thread_id)?;
            (!seen.contains(&parsed_thread_id))
                .then(|| (parsed_thread_id, agent_metadata(parsed_thread_id), status))
        })
        .collect::<Vec<_>>();
    extras.sort_by_key(|entry| entry.0.to_string());
    entries.extend(extras);

    if entries.is_empty() {
        vec![Line::from(Span::from("No agents completed yet"))]
    } else {
        entries
            .into_iter()
            .map(|(thread_id, metadata, status)| {
                let mut spans = agent_label_spans(agent_label(thread_id, &metadata));
                spans.push(Span::from(": ").dim());
                spans.extend(status_summary_spans(status));
                spans.into()
            })
            .collect()
    }
}

fn first_agent_state<'a>(
    receiver_thread_ids: &[String],
    agents_states: &'a std::collections::HashMap<String, CollabAgentState>,
) -> Option<&'a CollabAgentState> {
    receiver_thread_ids
        .iter()
        .find_map(|thread_id| agents_states.get(thread_id))
        .or_else(|| {
            agents_states
                .iter()
                .min_by(|left, right| left.0.cmp(right.0))
                .map(|(_, status)| status)
        })
}

fn status_summary_line(status: Option<&CollabAgentState>, fallback_error: &str) -> Line<'static> {
    match status {
        Some(status) => status_summary_spans(status).into(),
        None => error_summary_spans(fallback_error).into(),
    }
}

fn status_summary_spans(status: &CollabAgentState) -> Vec<Span<'static>> {
    match status.status {
        CollabAgentStatus::PendingInit => vec![Span::from("Pending init").cyan()],
        CollabAgentStatus::Running => vec![Span::from("Running").cyan().bold()],
        // Allow `.yellow()`
        #[allow(clippy::disallowed_methods)]
        CollabAgentStatus::Interrupted => vec![Span::from("Interrupted").yellow()],
        CollabAgentStatus::Completed => {
            let mut spans = vec![Span::from("Completed").green()];
            if let Some(message) = status.message.as_ref() {
                let message_preview = truncate_text(
                    &message.split_whitespace().collect::<Vec<_>>().join(" "),
                    COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES,
                );
                if !message_preview.is_empty() {
                    spans.push(Span::from(" - ").dim());
                    spans.push(Span::from(message_preview));
                }
            }
            spans
        }
        CollabAgentStatus::Errored => {
            error_summary_spans(status.message.as_deref().unwrap_or("Agent errored"))
        }
        CollabAgentStatus::Shutdown => vec![Span::from("Shutdown")],
        CollabAgentStatus::NotFound => vec![Span::from("Not found").red()],
    }
}

fn error_summary_spans(error: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::from("Error").red()];
    let error_preview = truncate_text(
        &error.split_whitespace().collect::<Vec<_>>().join(" "),
        COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES,
    );
    if !error_preview.is_empty() {
        spans.push(Span::from(" - ").dim());
        spans.push(Span::from(error_preview));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryCell;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyEvent;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyModifiers;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use std::collections::HashMap;

    #[test]
    fn collab_events_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let spawn = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-spawn".to_string(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: Some("Compute 11! and reply with just the integer result.".to_string()),
                model: Some("gpt-5".to_string()),
                reasoning_effort: Some(ReasoningEffortConfig::High),
                agents_states: HashMap::from([(
                    robie_id.to_string(),
                    agent_state(CollabAgentStatus::PendingInit, /*message*/ None),
                )]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, bob_id),
        )
        .expect("spawn item renders");

        let send = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-send".to_string(),
                tool: CollabAgentTool::SendInput,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: Some("Please continue and return the answer only.".to_string()),
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::from([(
                    robie_id.to_string(),
                    agent_state(CollabAgentStatus::Running, /*message*/ None),
                )]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, bob_id),
        )
        .expect("send-input item renders");

        let waiting = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-wait".to_string(),
                tool: CollabAgentTool::Wait,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::new(),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, bob_id),
        )
        .expect("wait begin item renders");

        let finished = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-wait".to_string(),
                tool: CollabAgentTool::Wait,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string(), bob_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::from([
                    (
                        robie_id.to_string(),
                        agent_state(CollabAgentStatus::Completed, Some("39916800")),
                    ),
                    (
                        bob_id.to_string(),
                        agent_state(CollabAgentStatus::Errored, Some("tool timeout")),
                    ),
                ]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, bob_id),
        )
        .expect("wait end item renders");

        let close = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-close".to_string(),
                tool: CollabAgentTool::CloseAgent,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::from([(
                    robie_id.to_string(),
                    agent_state(CollabAgentStatus::Completed, Some("39916800")),
                )]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, bob_id),
        )
        .expect("close item renders");

        let snapshot = [spawn, send, waiting, finished, close]
            .iter()
            .map(cell_to_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_agent_transcript", snapshot);
    }

    #[test]
    fn spawn_in_progress_renders_live_status() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let cell = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-spawn".to_string(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: Vec::new(),
                prompt: Some("Inspect provider setup and report blockers.".to_string()),
                model: Some("Chitti-Smart".to_string()),
                reasoning_effort: Some(ReasoningEffortConfig::High),
                agents_states: HashMap::new(),
            },
            /*cached_spawn_request*/ None,
            |_thread_id| AgentMetadata::default(),
        )
        .expect("spawn begin should render");

        assert_snapshot!("spawn_in_progress_renders_live_status", cell_to_text(&cell));
    }

    #[test]
    fn task_name_prompt_label_humanizes_spawn_task_name() {
        assert_eq!(
            spawn_prompt_label("task: /root/discover_models"),
            "Model Discovery Agent"
        );
        assert_eq!(
            spawn_prompt_label("task: benchmark-tools"),
            "Benchmarking Agent"
        );
        assert_eq!(
            spawn_prompt_label("task: system_info"),
            "System Exploration Agent"
        );
        assert_eq!(
            spawn_prompt_label("task: network-sniffing"),
            "Network Sniffing Agent"
        );
    }

    #[test]
    fn task_prompt_is_not_rendered_as_live_activity() {
        assert_eq!(prompt_preview("task: system_info"), None);

        let entry = LiveAgentStatusEntry {
            thread_id: None,
            label: "System Exploration Agent".to_string(),
            stage: LiveAgentStage::Spawned,
            detail: Some("task: system_info".to_string()),
            model: None,
            reasoning_effort: None,
        };

        assert_eq!(
            live_agent_activity_text(&entry),
            Some("awaiting first LLM update".to_string())
        );
    }

    #[test]
    fn live_agent_activity_delta_updates_row_detail() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000050").expect("valid thread id");
        let mut status = LiveAgentStatus::default();
        assert!(status.mark_agent_activity_started(thread_id, AgentMetadata::default()));
        assert!(status.apply_agent_activity_delta(
            thread_id,
            AgentMetadata::default(),
            "item-1",
            "Inspecting kernel services"
        ));

        let entry = status
            .entries
            .iter()
            .find(|entry| entry.thread_id == Some(thread_id))
            .expect("entry should exist");
        assert_eq!(entry.stage, LiveAgentStage::Running);
        assert_eq!(entry.detail.as_deref(), Some("Inspecting kernel services"));
        assert_eq!(
            live_agent_entry_line(entry, /*animations_enabled*/ false, Instant::now())
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "• Worker · running · Inspecting kernel services"
        );
    }

    #[test]
    fn live_agent_activity_delta_streams_chunks_and_resets_per_item() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000051").expect("valid thread id");
        let mut status = LiveAgentStatus::default();
        assert!(status.mark_agent_activity_started(thread_id, AgentMetadata::default()));
        assert!(status.apply_agent_activity_delta(
            thread_id,
            AgentMetadata::default(),
            "item-1",
            "Inspecting"
        ));
        assert!(status.apply_agent_activity_delta(
            thread_id,
            AgentMetadata::default(),
            "item-1",
            " services\nChecking logs"
        ));

        let entry = status
            .entries
            .iter()
            .find(|entry| entry.thread_id == Some(thread_id))
            .expect("entry should exist");
        assert_eq!(
            entry.detail.as_deref(),
            Some("Inspecting services Checking logs")
        );

        assert!(status.apply_agent_activity_delta(
            thread_id,
            AgentMetadata::default(),
            "item-2",
            "Reviewing config"
        ));
        let entry = status
            .entries
            .iter()
            .find(|entry| entry.thread_id == Some(thread_id))
            .expect("entry should exist");
        assert_eq!(entry.detail.as_deref(), Some("Reviewing config"));
    }

    #[test]
    fn live_agent_activity_preview_tracks_latest_tail() {
        let older = "older context ".repeat(30);
        let latest = "current streamed update is still moving";
        let preview = latest_agent_activity_preview(&format!("{older}{latest}"))
            .expect("preview should render");

        assert!(preview.starts_with("..."));
        assert!(preview.ends_with(latest));
    }

    #[test]
    fn live_agent_entry_omits_model_effort_and_shows_inline_activity() {
        let entry = LiveAgentStatusEntry {
            thread_id: None,
            label: "Model Discovery Agent".to_string(),
            stage: LiveAgentStage::Spawned,
            detail: Some("checking remote models".to_string()),
            model: Some("Chitti-Smart".to_string()),
            reasoning_effort: Some(ReasoningEffortConfig::High),
        };

        let line = live_agent_entry_line(&entry, /*animations_enabled*/ false, Instant::now());
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(
            rendered,
            "• Model Discovery Agent · spawned · checking remote models"
        );
        assert!(!rendered.contains("Chitti-Smart"));
        assert!(!rendered.contains("high"));
    }

    #[test]
    fn live_agent_entry_shows_running_fallback_activity() {
        let entry = LiveAgentStatusEntry {
            thread_id: None,
            label: "Benchmark tools".to_string(),
            stage: LiveAgentStage::Running,
            detail: None,
            model: None,
            reasoning_effort: None,
        };

        let line = live_agent_entry_line(&entry, /*animations_enabled*/ false, Instant::now());
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(rendered, "• Benchmark tools · running · LLM call active");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn agent_shortcut_matches_option_arrow_word_motion_fallbacks_only_when_allowed() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn agent_shortcut_matches_option_arrows_only() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
    }

    #[test]
    fn title_styles_nickname_and_role() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let cell = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-spawn".to_string(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: Some(String::new()),
                model: Some("gpt-5".to_string()),
                reasoning_effort: Some(ReasoningEffortConfig::High),
                agents_states: HashMap::from([(
                    robie_id.to_string(),
                    agent_state(CollabAgentStatus::PendingInit, /*message*/ None),
                )]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
        )
        .expect("spawn item renders");

        let lines = cell.display_lines(/*width*/ 200);
        let title = &lines[0];
        assert_eq!(title.spans[2].content.as_ref(), "Robie");
        assert_eq!(title.spans[2].style.fg, Some(Color::Cyan));
        assert!(title.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title.spans[4].content.as_ref(), "[explorer]");
        assert_eq!(title.spans[4].style.fg, None);
        assert!(!title.spans[4].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(title.spans[6].content.as_ref(), "(gpt-5 high)");
        assert_eq!(title.spans[6].style.fg, Some(Color::Magenta));
    }

    #[test]
    fn collab_resume_interrupted_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = tool_call_history_cell(
            &ThreadItem::CollabAgentToolCall {
                id: "call-resume".to_string(),
                tool: CollabAgentTool::ResumeAgent,
                status: CollabAgentToolCallStatus::Completed,
                sender_thread_id: sender_thread_id.to_string(),
                receiver_thread_ids: vec![robie_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states: HashMap::from([(
                    robie_id.to_string(),
                    agent_state(CollabAgentStatus::Interrupted, /*message*/ None),
                )]),
            },
            /*cached_spawn_request*/ None,
            |thread_id| metadata_for(thread_id, robie_id, ThreadId::new()),
        )
        .expect("resume item renders");

        assert_snapshot!("collab_resume_interrupted", cell_to_text(&cell));
    }

    fn agent_state(status: CollabAgentStatus, message: Option<&str>) -> CollabAgentState {
        CollabAgentState {
            status,
            message: message.map(str::to_string),
        }
    }

    fn metadata_for(thread_id: ThreadId, robie_id: ThreadId, bob_id: ThreadId) -> AgentMetadata {
        if thread_id == robie_id {
            AgentMetadata {
                agent_nickname: Some("Robie".to_string()),
                agent_role: Some("explorer".to_string()),
            }
        } else if thread_id == bob_id {
            AgentMetadata {
                agent_nickname: Some("Bob".to_string()),
                agent_role: Some("worker".to_string()),
            }
        } else {
            AgentMetadata::default()
        }
    }

    fn cell_to_text(cell: &PlainHistoryCell) -> String {
        cell.display_lines(/*width*/ 200)
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_to_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }
}
