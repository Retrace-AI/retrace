//! Gen-Z mission dashboard for ABSOLUTE RAMPAGE / READONLY RESEARCH modes.
//!
//! The dashboard reads the durable Rampage state file that the core
//! `rampage_control` / `rampage_board` / `rampage_spawn` tools maintain
//! (`<codex_home>/rampage/<thread_id>.json`) and renders the whole mission: the
//! mission header, a verifier strip (pass % vs. threshold and failure count), the
//! full task/agent roster with no cap, and the recent Questboard items.
//!
//! It is intentionally self-contained: it re-reads and re-parses the (small, local)
//! state file on each render pass, so it always reflects the latest durable state
//! without any event plumbing. The panel is empty (and therefore hidden) whenever
//! there is no active mission for the current thread.

use crate::color::is_light;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;
use crate::terminal_palette::best_color;
use crate::terminal_palette::default_bg;
use crate::tui::FrameRequester;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

// Accent colors are theme-aware. On a light terminal the bright ANSI/RGB neons wash out
// (and `Color::White` disappears entirely), so on light backgrounds we map each accent to
// a darker, higher-contrast shade via `best_color`, matching how the rest of the TUI
// (`accent_style_for`) adapts. Primary text always uses `primary()` — the terminal's own
// default foreground — so it stays legible on any background.

fn on_light_bg() -> bool {
    default_bg().is_some_and(is_light)
}

/// Picks a light-background-safe (darker) color or a dark-background color.
fn adaptive(light_rgb: (u8, u8, u8), dark: Color) -> Color {
    if on_light_bg() {
        best_color(light_rgb)
    } else {
        dark
    }
}

fn accent_pink() -> Color {
    adaptive((150, 0, 130), Color::Magenta)
}
fn accent_cyan() -> Color {
    adaptive((0, 95, 135), Color::Cyan)
}
fn accent_green() -> Color {
    adaptive((0, 120, 0), Color::Green)
}
fn accent_orange() -> Color {
    adaptive((165, 90, 0), Color::Rgb(255, 140, 0))
}
fn accent_red() -> Color {
    adaptive((175, 0, 0), Color::Red)
}
fn accent_yellow() -> Color {
    adaptive((150, 110, 0), Color::Yellow)
}

/// Style for primary text: the terminal's default foreground (always readable).
fn primary() -> Style {
    Style::default()
}

/// Style for de-emphasized secondary text, readable on light and dark backgrounds.
fn secondary() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Maximum Questboard items to surface in the dashboard.
const MAX_BOARD_ITEMS: usize = 8;

// --- Deserialized subset of the durable Rampage state file ------------------

#[derive(Debug, Clone, Deserialize)]
struct RampageStateFile {
    #[serde(default)]
    active_mission_id: Option<String>,
    #[serde(default)]
    missions: Vec<MissionRow>,
    #[serde(default)]
    tasks: Vec<TaskRow>,
    #[serde(default)]
    board_items: Vec<BoardRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct MissionRow {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    support_agents: String,
    #[serde(default)]
    verifier_status: Option<String>,
    #[serde(default = "default_pass_threshold")]
    verifier_pass_threshold: f64,
    #[serde(default)]
    verifier_pass_percentage: Option<f64>,
    #[serde(default)]
    verifier_max_failures: Option<u64>,
    #[serde(default)]
    verifier_failure_count: u64,
}

fn default_pass_threshold() -> f64 {
    100.0
}

#[derive(Debug, Clone, Deserialize)]
struct TaskRow {
    #[serde(default)]
    mission_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    worker_session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BoardRow {
    #[serde(default)]
    mission_id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    active: bool,
}

// --- Panel ------------------------------------------------------------------

pub(crate) struct RampageDashboardPanel {
    state_path: Option<PathBuf>,
    frame_requester: FrameRequester,
    animations_enabled: bool,
}

impl RampageDashboardPanel {
    pub(crate) fn new(frame_requester: FrameRequester, animations_enabled: bool) -> Self {
        Self {
            state_path: None,
            frame_requester,
            animations_enabled,
        }
    }

    /// Points the dashboard at the durable Rampage state file for `thread_id`.
    pub(crate) fn set_source(&mut self, codex_home: &std::path::Path, thread_id: &str) {
        let path = codex_home
            .join("rampage")
            .join(format!("{thread_id}.json"));
        self.state_path = Some(path);
    }

    fn load(&self) -> Option<(MissionRow, Vec<TaskRow>, Vec<BoardRow>)> {
        let path = self.state_path.as_ref()?;
        let contents = std::fs::read_to_string(path).ok()?;
        let state: RampageStateFile = serde_json::from_str(&contents).ok()?;
        let active_id = state.active_mission_id.as_deref()?;
        let mission = state
            .missions
            .iter()
            .find(|mission| mission.id == active_id)?
            .clone();
        // Only surface the dashboard while the mission is live.
        if !matches!(
            mission.status.as_str(),
            "running" | "blocked" | "verifying"
        ) {
            return None;
        }
        let tasks = state
            .tasks
            .into_iter()
            .filter(|task| task.mission_id == mission.id)
            .collect::<Vec<_>>();
        let board_items = state
            .board_items
            .into_iter()
            .filter(|item| item.mission_id == mission.id && item.active)
            .collect::<Vec<_>>();
        Some((mission, tasks, board_items))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.load().is_none()
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let Some((mission, tasks, board_items)) = self.load() else {
            return Vec::new();
        };
        let mut lines: Vec<Line<'static>> = Vec::new();

        // Header: ⚡ ABSOLUTE RAMPAGE · <title> · <status pill>
        let mut header: Vec<Span<'static>> = vec![
            Span::styled(
                "⚡ MISSION ",
                Style::default().fg(accent_pink()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                mission.title.clone(),
                primary().add_modifier(Modifier::BOLD),
            ),
            "  ".into(),
        ];
        header.push(status_pill(&mission.status));
        lines.push(Line::from(header));

        // Phase + support-agent choice.
        lines.push(Line::from(vec![
            "  ".into(),
            Span::styled("phase ", secondary()),
            Span::styled(mission.phase.clone(), Style::default().fg(accent_cyan())),
            "   ".into(),
            Span::styled("support ", secondary()),
            Span::styled(mission.support_agents.clone(), Style::default().fg(accent_cyan())),
        ]));

        // Verifier strip.
        lines.push(verifier_line(&mission));

        // Agent / task roster (uncapped).
        if !tasks.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "  ▸ agents",
                Style::default().fg(accent_orange()).add_modifier(Modifier::BOLD),
            )]));
            for task in &tasks {
                lines.push(task_line(task));
            }
        }

        // Questboard.
        if !board_items.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "  ▸ questboard",
                Style::default().fg(accent_orange()).add_modifier(Modifier::BOLD),
            )]));
            let total = board_items.len();
            for item in board_items.iter().rev().take(MAX_BOARD_ITEMS) {
                lines.push(board_line(item));
            }
            if total > MAX_BOARD_ITEMS {
                lines.push(Line::from(vec![Span::styled(
                    format!("    +{} more board items", total - MAX_BOARD_ITEMS),
                    secondary(),
                )]));
            }
        }

        lines
    }
}

impl Renderable for RampageDashboardPanel {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let lines = self.lines();
        if lines.is_empty() {
            return;
        }
        // Keep the dashboard fresh while a mission is live.
        if self.animations_enabled {
            self.frame_requester
                .schedule_frame_in(Duration::from_millis(400));
        }
        let lines = lines
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

// --- Rendering helpers ------------------------------------------------------

fn status_pill(status: &str) -> Span<'static> {
    let (label, color) = match status {
        "running" => ("🟢 RUNNING", accent_green()),
        "verifying" => ("🔍 VERIFYING", accent_cyan()),
        "blocked" => ("⛔ BLOCKED", accent_red()),
        "paused" => ("⏸ PAUSED", accent_yellow()),
        "completed" => ("✅ COMPLETE", accent_green()),
        "stopped" => ("🛑 STOPPED", accent_red()),
        other => return Span::styled(other.to_uppercase(), primary()),
    };
    Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn verifier_line(mission: &MissionRow) -> Line<'static> {
    let threshold = mission.verifier_pass_threshold;
    let max_failures = mission
        .verifier_max_failures
        .map(|max| max.to_string())
        .unwrap_or_else(|| "∞".to_string());
    let mut spans: Vec<Span<'static>> = vec![
        "  ".into(),
        Span::styled(
            "✔ verify ",
            Style::default().fg(accent_pink()).add_modifier(Modifier::BOLD),
        ),
    ];
    match mission.verifier_pass_percentage {
        Some(pass) => {
            let color = if pass >= threshold { accent_green() } else { accent_red() };
            spans.push(Span::styled(
                format!("{pass:.0}%"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" / need {threshold:.0}%"),
                secondary(),
            ));
        }
        None => {
            spans.push(Span::styled(
                "not scored yet",
                secondary(),
            ));
            spans.push(Span::styled(
                format!(" · need {threshold:.0}%"),
                secondary(),
            ));
        }
    }
    spans.push(Span::styled(
        format!("  · failures {}/{max_failures}", mission.verifier_failure_count),
        Style::default().fg(if mission.verifier_failure_count > 0 {
            accent_orange()
        } else {
            Color::DarkGray
        }),
    ));
    if let Some(status) = mission.verifier_status.as_deref() {
        spans.push(Span::styled(
            format!("  [{status}]"),
            secondary(),
        ));
    }
    Line::from(spans)
}

fn task_line(task: &TaskRow) -> Line<'static> {
    let (dot, color) = task_status_dot(&task.status);
    let label = if !task.role.trim().is_empty() {
        task.role.clone()
    } else {
        task.title.clone()
    };
    let running = task
        .worker_session_id
        .as_deref()
        .map(str::trim)
        .filter(|worker| !worker.is_empty());
    let mut spans: Vec<Span<'static>> = vec![
        "    ".into(),
        Span::styled(dot, Style::default().fg(color)),
        " ".into(),
        Span::styled(
            kind_badge(&task.kind),
            Style::default().fg(accent_cyan()),
        ),
        " ".into(),
        Span::styled(label, primary()),
        Span::styled(
            format!("  {}", task.status),
            Style::default().fg(color),
        ),
    ];
    if let Some(worker) = running {
        spans.push(Span::styled(
            format!("  {worker}"),
            secondary(),
        ));
    }
    Line::from(spans)
}

fn task_status_dot(status: &str) -> (&'static str, Color) {
    match status {
        "running" => ("●", accent_green()),
        "queued" => ("○", accent_yellow()),
        "done" => ("✔", accent_cyan()),
        "blocked" => ("■", accent_red()),
        "failed" => ("✖", accent_red()),
        "cancelled" => ("–", Color::DarkGray),
        _ => ("•", accent_cyan()),
    }
}

fn kind_badge(kind: &str) -> String {
    format!("[{kind}]")
}

fn board_line(item: &BoardRow) -> Line<'static> {
    let color = match item.kind.as_str() {
        "finding" => accent_green(),
        "decision" => accent_cyan(),
        "blocker" => accent_red(),
        "artifact" => accent_pink(),
        "assumption" => accent_yellow(),
        "next_action" => accent_orange(),
        _ => accent_cyan(),
    };
    Line::from(vec![
        "    ".into(),
        Span::styled(kind_badge(&item.kind), Style::default().fg(color)),
        " ".into(),
        Span::styled(item.title.clone(), primary()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn panel_for(json: &str) -> (RampageDashboardPanel, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rampage-dash-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("rampage")).expect("create dir");
        let thread_id = "thread-dash";
        std::fs::write(dir.join("rampage").join(format!("{thread_id}.json")), json)
            .expect("write state");
        let mut panel = RampageDashboardPanel::new(FrameRequester::test_dummy(), false);
        panel.set_source(&dir, thread_id);
        (panel, dir)
    }

    const RUNNING_MISSION: &str = r#"{
        "active_mission_id": "mission-1",
        "missions": [{
            "id": "mission-1",
            "status": "running",
            "title": "Ship the thing",
            "phase": "workers",
            "support_agents": "both",
            "verifier_status": "failed",
            "verifier_pass_threshold": 80.0,
            "verifier_pass_percentage": 50.0,
            "verifier_max_failures": 3,
            "verifier_failure_count": 1
        }],
        "tasks": [
            {"mission_id":"mission-1","status":"running","kind":"work","role":"builder","title":"Build","worker_session_id":"/root/builder"},
            {"mission_id":"mission-1","status":"done","kind":"research","role":"new_ideas_agent","title":"New Ideas","worker_session_id":"/root/new_ideas"},
            {"mission_id":"mission-1","status":"queued","kind":"verify","role":"verifier","title":"Verify","worker_session_id":null}
        ],
        "board_items": [
            {"mission_id":"mission-1","kind":"finding","title":"Found a thing","active":true},
            {"mission_id":"mission-1","kind":"blocker","title":"Stuck on auth","active":true}
        ]
    }"#;

    #[test]
    fn renders_active_mission_with_all_agents_and_verifier() {
        let (panel, dir) = panel_for(RUNNING_MISSION);
        assert!(!panel.is_empty());
        let text = panel
            .lines()
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Ship the thing"));
        assert!(text.contains("RUNNING"));
        // Verifier strip shows pass % vs threshold and the failure count.
        assert!(text.contains("50%"));
        assert!(text.contains("need 80%"));
        assert!(text.contains("failures 1/3"));
        // All three agents/tasks are listed (no cap).
        assert!(text.contains("builder"));
        assert!(text.contains("new_ideas_agent"));
        assert!(text.contains("verifier"));
        // Questboard items surface.
        assert!(text.contains("Found a thing"));
        assert!(text.contains("Stuck on auth"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn infinite_failures_render_as_symbol() {
        let json = RUNNING_MISSION.replace("\"verifier_max_failures\": 3", "\"verifier_max_failures\": null");
        let (panel, dir) = panel_for(&json);
        let text = panel
            .lines()
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("failures 1/∞"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn completed_mission_is_hidden() {
        let json = RUNNING_MISSION.replace("\"status\": \"running\"", "\"status\": \"completed\"");
        let (panel, dir) = panel_for(&json);
        assert!(panel.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_empty() {
        let mut panel = RampageDashboardPanel::new(FrameRequester::test_dummy(), false);
        panel.set_source(std::path::Path::new("/nonexistent"), "nope");
        assert!(panel.is_empty());
    }
}
