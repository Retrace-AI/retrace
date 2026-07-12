//! Live inference-speed tracking for the top-right metrics overlay.
//!
//! Everything here is measured directly from the streamed deltas the TUI
//! already receives, so the numbers are provider-agnostic and do not depend on
//! otel/debug metric snapshots being enabled. Three numbers are surfaced:
//!
//! - **TTFT** (time-to-first-token): wall time from turn start to the first
//!   streamed delta (text or reasoning). A real, measured latency.
//! - **decode** (output tokens / sec): the CURRENT generation speed — a rolling
//!   rate over the last [`RECENT_WINDOW`] of streamed output. Fluctuates as the
//!   model speeds up / slows down. When idle it settles to the last call's
//!   overall rate.
//! - **avg** (output tokens / sec): the session-wide average decode across all
//!   LLM calls — total generated tokens / total active generation time.
//! - **prefill** (input tokens / sec): an *estimate* = input_tokens / TTFT.
//!   The gateway returns no true prompt-eval timing, and TTFT also folds in
//!   network + queue latency, so this is labelled as an estimate in the UI.

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use super::ChatWidget;

/// Rough characters-per-token used for the live decode estimate before the
/// exact usage count is available.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Inter-delta gaps longer than this are treated as non-generation time (tool
/// calls, network stalls) and excluded from the decode window.
const DECODE_GAP_CAP: Duration = Duration::from_millis(2000);

/// Trailing window over which the CURRENT (live) decode rate is measured.
const RECENT_WINDOW: Duration = Duration::from_millis(3000);

/// Per-turn accumulator (reset each turn) plus session-wide totals (never
/// reset) that back the running average.
#[derive(Debug, Default, Clone)]
pub(crate) struct InferenceTracker {
    /// When the current turn started (request dispatched).
    turn_start: Option<Instant>,
    /// Arrival of the first streamed delta of the turn.
    first_token_at: Option<Instant>,
    /// Arrival of the most recent streamed delta.
    last_token_at: Option<Instant>,
    /// Summed active generation time this turn (inter-delta gaps capped at
    /// [`DECODE_GAP_CAP`]).
    active_decode: Duration,
    /// Total output characters streamed this turn (text + reasoning).
    output_chars: usize,
    /// `last_token_usage.input_tokens` observed at the start of this turn. Used
    /// to detect when *this* turn's usage has actually landed (input changes
    /// from the previous turn's value) so prefill is never computed from a
    /// stale prompt size.
    input_baseline: i64,
    /// Whether the current turn's tokens/time have already been folded into the
    /// session totals (so the running average never double-counts a call).
    turn_finalized: bool,
    /// Session-wide generated tokens across all COMPLETED LLM calls (exact when
    /// usage was available, char estimate otherwise). Backs the running average.
    session_tokens: f64,
    /// Session-wide active generation time across all COMPLETED LLM calls.
    session_decode: Duration,
    /// Recent (arrival, chars) deltas, used to compute the CURRENT decode rate
    /// over a trailing window. Pruned to [`RECENT_WINDOW`] on each delta.
    recent: VecDeque<(Instant, u32)>,
}

impl InferenceTracker {
    /// Begin a fresh turn. Session totals are never cleared. The just-completed
    /// turn must be folded in via [`fold_turn`] (from the ChatWidget side, which
    /// has the exact token count) BEFORE this is called.
    fn reset(&mut self, now: Instant, input_baseline: i64) {
        self.turn_start = Some(now);
        self.first_token_at = None;
        self.last_token_at = None;
        self.active_decode = Duration::ZERO;
        self.output_chars = 0;
        self.input_baseline = input_baseline;
        self.turn_finalized = false;
        self.recent.clear();
    }

    /// Fold the current turn's generated tokens + active time into the session
    /// totals exactly once. `exact_tokens` is `output + reasoning` from usage;
    /// falls back to the char estimate when usage never arrived.
    fn fold_turn(&mut self, exact_tokens: f64) {
        if self.turn_finalized || self.turn_start.is_none() {
            return;
        }
        self.turn_finalized = true;
        let tokens = if exact_tokens > 0.0 {
            exact_tokens
        } else {
            self.output_chars as f64 / CHARS_PER_TOKEN
        };
        self.session_tokens += tokens;
        self.session_decode += self.active_decode;
    }

    /// Record a streamed delta (assistant text or reasoning). Reasoning deltas
    /// count toward output, so decode/average reflect total generation.
    fn record_delta(&mut self, delta: &str, now: Instant) {
        if delta.is_empty() {
            return;
        }
        match self.first_token_at {
            // First delta of the turn: starts the clock, no interval yet. The
            // large inter-turn gap is naturally excluded because this arm adds
            // nothing to the window.
            None => self.first_token_at = Some(now),
            Some(_) => {
                if let Some(last) = self.last_token_at {
                    let gap = now.saturating_duration_since(last);
                    if gap <= DECODE_GAP_CAP {
                        self.active_decode += gap;
                    }
                }
            }
        }
        self.last_token_at = Some(now);
        let chars = delta.chars().count();
        self.output_chars += chars;

        // Maintain the trailing window for the current-rate calculation.
        self.recent.push_back((now, u32::try_from(chars).unwrap_or(u32::MAX)));
        while let Some(&(t, _)) = self.recent.front() {
            if now.saturating_duration_since(t) > RECENT_WINDOW {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }

    /// The CURRENT decode rate: tokens streamed within the trailing
    /// [`RECENT_WINDOW`] ending now. `None` when nothing streamed recently (e.g.
    /// idle or mid tool-call), so callers fall back to the call's overall rate.
    fn current_decode(&self, now: Instant) -> Option<f64> {
        let mut chars = 0u64;
        let mut oldest: Option<Instant> = None;
        for &(t, c) in &self.recent {
            if now.saturating_duration_since(t) <= RECENT_WINDOW {
                chars += u64::from(c);
                oldest.get_or_insert(t);
            }
        }
        if chars == 0 {
            return None;
        }
        let span = now.saturating_duration_since(oldest?).as_secs_f64().max(0.25);
        Some((chars as f64 / CHARS_PER_TOKEN) / span)
    }
}

/// A snapshot of the three overlay numbers.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InferenceMetrics {
    /// Time-to-first-token, milliseconds. `None` until the first delta lands.
    pub ttft_ms: Option<u64>,
    /// Live output tokens/sec for the current turn. `None` until there is a
    /// usable window.
    pub decode_tps: Option<f64>,
    /// Session-wide average output tokens/sec across all turns so far.
    pub avg_decode_tps: Option<f64>,
    /// Estimated prefill tokens/sec (input / TTFT). `None` until known.
    pub prefill_tps: Option<f64>,
    /// True while decode is derived from the char estimate (not exact usage).
    pub decode_estimated: bool,
    /// True while a turn is actively generating.
    pub running: bool,
    /// True once there is anything worth showing.
    pub has_data: bool,
}

impl ChatWidget {
    /// Reset live inference tracking at the start of a turn. First folds the
    /// just-completed turn into the session totals with its exact token count
    /// (so the running average is accurate across every LLM call), then snapshots
    /// the current input-token count as the prefill freshness baseline.
    pub(super) fn reset_inference_tracking(&mut self) {
        self.inference.fold_turn(self.exact_generated_tokens());
        let input_baseline = self
            .token_info
            .as_ref()
            .map(|i| i.last_token_usage.input_tokens)
            .unwrap_or(0);
        self.inference.reset(Instant::now(), input_baseline);
    }

    /// This turn's exact generated tokens (output + reasoning) from usage, or 0
    /// if usage has not landed yet.
    fn exact_generated_tokens(&self) -> f64 {
        self.token_info
            .as_ref()
            .map(|i| (i.last_token_usage.output_tokens + i.last_token_usage.reasoning_output_tokens) as f64)
            .unwrap_or(0.0)
    }

    /// Record a streamed delta into the live inference tracker.
    pub(super) fn record_inference_delta(&mut self, delta: &str) {
        self.inference.record_delta(delta, Instant::now());
    }

    /// Compute the current overlay numbers. While a turn is running this
    /// reflects live progress; when idle it returns the last turn's frozen
    /// snapshot.
    pub(crate) fn inference_metrics(&self) -> InferenceMetrics {
        let running = self.turn_lifecycle.agent_turn_running;
        let t = &self.inference;

        // No turn has ever started: nothing to show.
        let Some(start) = t.turn_start else {
            return InferenceMetrics::default();
        };

        let ttft_ms = t
            .first_token_at
            .map(|f| u64::try_from(f.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX));

        // Exact generated tokens from usage — output PLUS reasoning, since the
        // char estimate counts reasoning deltas too (so both are consistent and
        // decode reflects total generation). Usage usually lands at turn end.
        let exact_out = self
            .token_info
            .as_ref()
            .map(|i| i.last_token_usage.output_tokens + i.last_token_usage.reasoning_output_tokens)
            .unwrap_or(0);

        // Whole-call token count: exact once usage lands, else char estimate.
        // Feeds the session average and the idle (settled) decode fallback.
        let (out_tokens, out_exact) = if !running && exact_out > 0 {
            (exact_out as f64, true)
        } else {
            (t.output_chars as f64 / CHARS_PER_TOKEN, false)
        };

        // decode = the CURRENT rate (rolling window) while generating; when
        // nothing streamed recently, fall back to the call's overall rate.
        let (decode_tps, decode_estimated) = match t.current_decode(Instant::now()) {
            Some(rate) => (Some(rate), true),
            None => (tps(out_tokens, t.active_decode), !out_exact),
        };

        // Session-wide average decode across ALL LLM calls: total generated
        // tokens / total active generation time. Completed calls contribute
        // their exact token counts (folded in at each turn's start); the current
        // in-flight call is added live (via its estimate) until it too is folded,
        // so the average updates actively as the session progresses.
        let (avg_tokens, avg_time) = if t.turn_finalized {
            (t.session_tokens, t.session_decode)
        } else {
            (t.session_tokens + out_tokens, t.session_decode + t.active_decode)
        };
        let avg_decode_tps = tps(avg_tokens, avg_time);

        // Prefill = input_tokens / TTFT. `input_tokens` is only correct for
        // *this* turn once the turn's usage lands (via ThreadTokenUsageUpdated).
        // While running, `token_info` may still hold the PREVIOUS turn's usage,
        // so we only surface prefill when this turn's usage is confirmed in —
        // either the turn is idle, or the input count has changed from the
        // baseline captured at turn start. This prevents dividing the previous
        // turn's prompt size by this turn's TTFT (a confidently-wrong number)
        // while still showing prefill as soon as fresh usage arrives.
        let input_tokens = self
            .token_info
            .as_ref()
            .map(|i| i.last_token_usage.input_tokens)
            .unwrap_or(0);
        let input_is_fresh = !running || input_tokens != t.input_baseline;
        let prefill_tps = match ttft_ms {
            Some(ms) if ms > 0 && input_tokens > 0 && input_is_fresh => {
                Some(input_tokens as f64 * 1000.0 / ms as f64)
            }
            _ => None,
        };

        InferenceMetrics {
            ttft_ms,
            decode_tps,
            avg_decode_tps,
            prefill_tps,
            decode_estimated,
            running,
            has_data: ttft_ms.is_some() || decode_tps.is_some() || avg_decode_tps.is_some(),
        }
    }
}

/// Tokens-per-second over an active-generation window, or `None` if the window
/// is too small to be meaningful.
fn tps(tokens: f64, window: Duration) -> Option<f64> {
    let secs = window.as_secs_f64();
    (secs > 0.05 && tokens > 0.0).then_some(tokens / secs)
}
