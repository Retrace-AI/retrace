//! A one-line inference-speed strip rendered as a fixed row directly above the
//! composer (see `chatwidget/rendering.rs`). Because it is a laid-out row — not
//! an overlay — it never paints over the conversation, stays put, and persists
//! after a turn (or the whole session) ends. The `●` dot dims when idle so the
//! numbers never read as live when nothing is generating.

use ratatui::buffer::Buffer;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::chatwidget::inference::InferenceMetrics;
use crate::render::renderable::Renderable;

/// A fixed one-row renderable that draws the inference strip right-aligned.
/// Reserves a row only when there is a line to show. Placed directly above the
/// composer input (below the working-status line) by the bottom pane.
pub(crate) struct InferenceStripRenderable {
    pub line: Option<Line<'static>>,
}

impl Renderable for InferenceStripRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(line) = &self.line {
            let row = Rect::new(area.x, area.y, area.width.saturating_sub(1), 1);
            Paragraph::new(line.clone())
                .alignment(Alignment::Right)
                .render(row, buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        u16::from(self.line.is_some())
    }
}

// Named ANSI colors so the terminal renders each accent legibly for ITS OWN
// theme — bright on a dark background, appropriately dark on a light one. This
// is what keeps the strip readable on both (hardcoded RGB only worked on dark
// terminals). Values are additionally bolded for emphasis.
const RUN_DOT: Color = Color::Green; // generating
const IDLE_DOT: Color = Color::DarkGray; // idle
const SEP: Color = Color::DarkGray;

const TTFT_COLOR: Color = Color::Cyan;
const DECODE_COLOR: Color = Color::Green;
const AVG_COLOR: Color = Color::LightGreen; // session average
const PREFILL_COLOR: Color = Color::Yellow; // estimate

/// Build the colored strip line, or `None` when there is nothing to show yet.
pub(crate) fn inference_strip_line(m: &InferenceMetrics) -> Option<Line<'static>> {
    if !m.has_data {
        return None;
    }
    let dot = if m.running { RUN_DOT } else { IDLE_DOT };
    let mut spans = vec![Span::styled(
        "● ",
        Style::default().fg(dot).add_modifier(Modifier::BOLD),
    )];
    push_metric(&mut spans, "ttft", ttft_text(m.ttft_ms), TTFT_COLOR);
    push_sep(&mut spans);
    push_metric(
        &mut spans,
        "decode",
        rate_text(m.decode_tps, m.decode_estimated),
        DECODE_COLOR,
    );
    push_sep(&mut spans);
    push_metric(&mut spans, "avg", rate_text(m.avg_decode_tps, true), AVG_COLOR);
    push_sep(&mut spans);
    push_metric(
        &mut spans,
        "prefill",
        rate_text(m.prefill_tps, true),
        PREFILL_COLOR,
    );
    Some(Line::from(spans))
}

fn push_metric(spans: &mut Vec<Span<'static>>, label: &'static str, value: String, color: Color) {
    // Label uses the terminal's default foreground (no fg set) so it is legible
    // on any theme; the value is the accent color, bolded.
    spans.push(Span::raw(format!("{label} ")));
    spans.push(Span::styled(
        value,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
}

fn push_sep(spans: &mut Vec<Span<'static>>) {
    spans.push(Span::styled("  ·  ", Style::default().fg(SEP)));
}

fn ttft_text(ttft_ms: Option<u64>) -> String {
    match ttft_ms {
        // Unmeasured (no request yet): show 0 rather than a dash.
        None => "0 ms".to_string(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{:.2} s", ms as f64 / 1000.0),
    }
}

fn rate_text(tps: Option<f64>, estimated: bool) -> String {
    match tps {
        // Unmeasured (no request yet): show 0 rather than a dash.
        None => "0 tok/s".to_string(),
        Some(v) => {
            let prefix = if estimated { "~" } else { "" };
            format!("{prefix}{v:.0} tok/s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn hidden_when_no_data() {
        assert!(inference_strip_line(&InferenceMetrics::default()).is_none());
    }

    #[test]
    fn shows_all_metrics_while_running() {
        let m = InferenceMetrics {
            ttft_ms: Some(620),
            decode_tps: Some(48.0),
            avg_decode_tps: Some(51.0),
            prefill_tps: Some(412.0),
            decode_estimated: true,
            running: true,
            has_data: true,
        };
        let line = inference_strip_line(&m).expect("line");
        let t = text(&line);
        assert!(t.contains("ttft 620 ms"), "{t}");
        assert!(t.contains("decode ~48 tok/s"), "{t}");
        assert!(t.contains("avg ~51 tok/s"), "{t}");
        assert!(t.contains("prefill ~412 tok/s"), "{t}");
    }

    #[test]
    fn persists_with_only_average_when_idle() {
        // After a session ends the tracker may only have a session average; the
        // strip must still render (rest shown as em dashes).
        let m = InferenceMetrics {
            ttft_ms: None,
            decode_tps: None,
            avg_decode_tps: Some(44.0),
            prefill_tps: None,
            decode_estimated: false,
            running: false,
            has_data: true,
        };
        let line = inference_strip_line(&m).expect("line");
        let t = text(&line);
        assert!(t.contains("avg ~44 tok/s"), "{t}");
        // Unmeasured metrics render as 0 (not a dash).
        assert!(t.contains("ttft 0 ms"), "{t}");
        assert!(t.contains("decode 0 tok/s"), "{t}");
    }

    #[test]
    fn ttft_switches_to_seconds_over_one_second() {
        assert_eq!(ttft_text(Some(620)), "620 ms");
        assert_eq!(ttft_text(Some(1240)), "1.24 s");
        assert_eq!(ttft_text(None), "0 ms");
    }
}
