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

/// Inter-delta gaps longer than this are excluded from the live estimate. Once
/// usage closes a call, its measured first-token-to-completion span replaces
/// this estimate so streamed tool arguments are represented too.
const DECODE_GAP_CAP: Duration = Duration::from_millis(2000);

/// Trailing window over which the CURRENT (live) decode rate is measured.
const RECENT_WINDOW: Duration = Duration::from_millis(3000);

/// Per-call accumulator plus session-wide totals that back the running average.
/// A single agent turn can issue many LLM calls around tool execution, so usage
/// updates, rather than turn boundaries, delimit calls.
#[derive(Debug, Default, Clone)]
pub(crate) struct InferenceTracker {
    /// When the current turn started (request dispatched).
    turn_start: Option<Instant>,
    /// Arrival of the first streamed delta of the turn.
    first_token_at: Option<Instant>,
    /// Arrival of the first streamed delta of the current internal LLM call.
    call_first_token_at: Option<Instant>,
    /// Arrival of the most recent streamed delta.
    last_token_at: Option<Instant>,
    /// Summed active generation time for the current call (inter-delta gaps capped at
    /// [`DECODE_GAP_CAP`]).
    active_decode: Duration,
    /// Total output characters streamed by the current LLM call (text +
    /// reasoning).
    output_chars: usize,
    /// `last_token_usage.input_tokens` observed at the start of this turn. Used
    /// to detect when *this* turn's usage has actually landed (input changes
    /// from the previous turn's value) so prefill is never computed from a
    /// stale prompt size.
    input_baseline: i64,
    /// Cumulative output-token count observed when this turn started or when the
    /// most recent LLM call completed. `None` means no trustworthy baseline was
    /// available (for example, immediately after resuming a thread).
    output_token_baseline: Option<i64>,
    /// Session-wide generated tokens across all COMPLETED LLM calls (exact when
    /// usage was available, char estimate otherwise). Backs the running average.
    session_tokens: f64,
    /// Session-wide active generation time across all COMPLETED LLM calls.
    session_decode: Duration,
    /// Most recently completed call, retained so decode settles to an exact
    /// per-call rate while the agent is running a tool or otherwise idle.
    last_call_tokens: f64,
    last_call_decode: Duration,
    /// Recent (arrival, chars) deltas, used to compute the CURRENT decode rate
    /// over a trailing window. Pruned to [`RECENT_WINDOW`] on each delta.
    recent: VecDeque<(Instant, u32)>,
}

impl InferenceTracker {
    /// Begin a fresh turn. Session totals are never cleared. Any unreported call
    /// must be folded before this is called.
    fn reset(&mut self, now: Instant, input_baseline: i64, output_token_baseline: Option<i64>) {
        self.turn_start = Some(now);
        self.first_token_at = None;
        self.call_first_token_at = None;
        self.last_token_at = None;
        self.active_decode = Duration::ZERO;
        self.output_chars = 0;
        self.input_baseline = input_baseline;
        self.output_token_baseline = output_token_baseline;
        self.last_call_tokens = 0.0;
        self.last_call_decode = Duration::ZERO;
        self.recent.clear();
    }

    /// Fold an LLM call into the session totals and reset only the per-call
    /// state. `output_tokens` already includes reasoning tokens; the provider's
    /// reasoning count is a detail field and must not be added again.
    fn fold_call(&mut self, output_tokens: f64, completed_at: Instant) {
        if self.turn_start.is_none() || output_tokens <= 0.0 {
            return;
        }

        let completed_decode = self
            .call_first_token_at
            .map(|first| completed_at.saturating_duration_since(first))
            .unwrap_or(self.active_decode);

        if completed_decode.as_secs_f64() > 0.05 {
            self.session_tokens += output_tokens;
            self.session_decode += completed_decode;
            self.last_call_tokens = output_tokens;
            self.last_call_decode = completed_decode;
        }

        self.call_first_token_at = None;
        self.last_token_at = None;
        self.active_decode = Duration::ZERO;
        self.output_chars = 0;
        self.recent.clear();
    }

    /// Record the usage event that closes one internal LLM call. The cumulative
    /// output counter deduplicates repeated notifications; when it resets, the
    /// event's last-call count is the only safe fallback.
    fn record_usage(
        &mut self,
        total_output_tokens: i64,
        last_output_tokens: i64,
        completed_at: Instant,
    ) {
        if self.turn_start.is_none() {
            self.output_token_baseline = Some(total_output_tokens);
            return;
        }

        let output_tokens = match self.output_token_baseline {
            Some(baseline) if total_output_tokens > baseline => total_output_tokens - baseline,
            Some(baseline) if total_output_tokens == baseline => 0,
            Some(_) | None => last_output_tokens.max(0),
        };
        self.output_token_baseline = Some(total_output_tokens);
        if output_tokens > 0 {
            self.fold_call(output_tokens as f64, completed_at);
        }
    }

    /// Preserve a measurable call if a provider never sends usage. This is only
    /// a fallback at the next user-turn boundary; normal calls are finalized by
    /// [`Self::record_usage`].
    fn fold_unreported_call(&mut self, completed_at: Instant) {
        let estimated_tokens = self.output_chars as f64 / CHARS_PER_TOKEN;
        if estimated_tokens > 0.0 {
            self.fold_call(estimated_tokens, completed_at);
        }
    }

    /// Record a streamed delta (assistant text or reasoning). Reasoning deltas
    /// count toward output, so decode/average reflect total generation.
    fn record_delta(&mut self, delta: &str, now: Instant) {
        if delta.is_empty() {
            return;
        }
        self.first_token_at.get_or_insert(now);
        self.call_first_token_at.get_or_insert(now);
        if let Some(last) = self.last_token_at {
            let gap = now.saturating_duration_since(last);
            if gap <= DECODE_GAP_CAP {
                self.active_decode += gap;
            }
        }
        self.last_token_at = Some(now);
        let chars = delta.chars().count();
        self.output_chars += chars;

        // Maintain the trailing window for the current-rate calculation.
        self.recent
            .push_back((now, u32::try_from(chars).unwrap_or(u32::MAX)));
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
        let span = now
            .saturating_duration_since(oldest?)
            .as_secs_f64()
            .max(0.25);
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
    /// Reset live inference tracking at the start of a turn. First preserves
    /// any call whose provider omitted usage, then snapshots the current usage
    /// counters as freshness/deduplication baselines.
    pub(super) fn reset_inference_tracking(&mut self) {
        let now = Instant::now();
        self.inference.fold_unreported_call(now);
        let input_baseline = self
            .token_info
            .as_ref()
            .map(|i| i.last_token_usage.input_tokens)
            .unwrap_or(0);
        let output_token_baseline = self
            .token_info
            .as_ref()
            .map(|i| i.total_token_usage.output_tokens);
        self.inference
            .reset(now, input_baseline, output_token_baseline);
    }

    /// Record a streamed delta into the live inference tracker.
    pub(super) fn record_inference_delta(&mut self, delta: &str) {
        self.inference.record_delta(delta, Instant::now());
    }

    /// Finalize one internal LLM call from its cumulative usage update.
    pub(super) fn record_inference_usage(&mut self, info: &crate::token_usage::TokenUsageInfo) {
        self.inference.record_usage(
            info.total_token_usage.output_tokens,
            info.last_token_usage.output_tokens,
            Instant::now(),
        );
    }

    /// Compute the current overlay numbers. While a turn is running this
    /// reflects live progress; when idle it returns the last turn's frozen
    /// snapshot.
    pub(crate) fn inference_metrics(&self) -> InferenceMetrics {
        let running = self.turn_lifecycle.agent_turn_running;
        let t = &self.inference;

        // No turn has ever started yet: still show the strip, with zeros.
        let Some(start) = t.turn_start else {
            return InferenceMetrics {
                has_data: true,
                ..InferenceMetrics::default()
            };
        };

        let ttft_ms = t.first_token_at.map(|f| {
            u64::try_from(f.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
        });

        // The in-flight call is estimated until its usage event arrives. Each
        // completed internal call is already exact in the session accumulator.
        let out_tokens = t.output_chars as f64 / CHARS_PER_TOKEN;

        // decode = the CURRENT rate (rolling window) while generating; when
        // nothing streamed recently, fall back to the call's overall rate.
        let (decode_tps, decode_estimated) = match t.current_decode(Instant::now()) {
            Some(rate) => (Some(rate), true),
            None if out_tokens > 0.0 => (tps(out_tokens, t.active_decode), true),
            None => (
                tps(t.last_call_tokens, t.last_call_decode),
                t.last_call_tokens <= 0.0,
            ),
        };

        // Session-wide average decode across ALL LLM calls: total generated
        // tokens / total active generation time. Usage updates fold every
        // completed internal call independently; only the current call remains
        // estimated here.
        let avg_tokens = t.session_tokens + out_tokens;
        let avg_time = t.session_decode + t.active_decode;
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
            // Always show the strip; unmeasured values render as 0.
            has_data: true,
        }
    }
}

#[cfg(test)]
#[path = "inference_tests.rs"]
mod tests;

/// Tokens-per-second over an active-generation window, or `None` if the window
/// is too small to be meaningful.
fn tps(tokens: f64, window: Duration) -> Option<f64> {
    let secs = window.as_secs_f64();
    (secs > 0.05 && tokens > 0.0).then_some(tokens / secs)
}
