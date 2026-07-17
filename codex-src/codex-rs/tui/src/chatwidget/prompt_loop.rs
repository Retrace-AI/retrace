//! Session-scoped prompt loops started by `/loop` and `/ralphloop`.

use super::*;

const BUSY_LOOP_RETRY: Duration = Duration::from_secs(1);
const LOOP_USAGE: &str = "Usage: /loop [15s | 15 seconds] <prompt> | /loop status | /loop stop";
const RALPH_LOOP_USAGE: &str = concat!(
    "Usage: /ralphloop <prompt> [for N iterations | --max-iterations N] ",
    "[--completion-promise TEXT] | /ralphloop status | /ralphloop stop"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromptLoopPhase {
    Waiting,
    Due,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PromptLoopKind {
    Timed {
        interval: Option<Duration>,
    },
    Ralph {
        max_iterations: Option<u64>,
        completion_promise: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PromptLoopState {
    pub(super) generation: u64,
    pub(super) thread_id: ThreadId,
    pub(super) prompt: String,
    pub(super) kind: PromptLoopKind,
    pub(super) phase: PromptLoopPhase,
    pub(super) iteration: u64,
    pub(super) timed_tick_pending: bool,
    pub(super) next_tick_at: Option<tokio::time::Instant>,
    pub(super) retry_scheduled: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct TimedLoopArgs {
    interval: Option<Duration>,
    prompt: String,
}

#[derive(Debug, PartialEq, Eq)]
struct RalphLoopArgs {
    prompt: String,
    max_iterations: Option<u64>,
    completion_promise: Option<String>,
}

enum LoopTurnAction {
    None,
    Submit,
    Retry,
    Stop(String),
}

impl ChatWidget {
    pub(super) fn handle_loop_command_args(&mut self, args: String) {
        let trimmed = args.trim();
        if is_stop_command(trimmed) {
            self.stop_prompt_loop(/*ralph_only*/ false);
            return;
        }
        if trimmed.eq_ignore_ascii_case("status") {
            self.show_prompt_loop_status(/*ralph_only*/ false);
            return;
        }

        let parsed = match parse_timed_loop_args(trimmed) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "The session is still starting; try /loop again in a moment.".to_string(),
            );
            return;
        };

        let next_tick_at = match parsed
            .interval
            .map(initial_prompt_loop_deadline)
            .transpose()
        {
            Ok(deadline) => deadline,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        let generation = self.replace_prompt_loop_generation();
        self.prompt_loop = Some(PromptLoopState {
            generation,
            thread_id,
            prompt: parsed.prompt,
            kind: PromptLoopKind::Timed {
                interval: parsed.interval,
            },
            phase: if parsed.interval.is_some() {
                PromptLoopPhase::Waiting
            } else {
                PromptLoopPhase::Due
            },
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at,
            retry_scheduled: false,
        });
        if let Some(deadline) = next_tick_at {
            self.schedule_prompt_loop_wakeup_at(
                thread_id,
                generation,
                deadline,
                PromptLoopWakeupReason::Interval,
            );
        }
        let message = parsed.interval.map_or_else(
            || {
                "Self-paced loop started and will run again after every completed turn until you run `/loop stop`."
                    .to_string()
            },
            |interval| {
                format!(
                    "Loop scheduled every {} and will continue until you run `/loop stop`.",
                    format_interval(interval)
                )
            },
        );
        self.add_info_message(
            message,
            Some("Running /loop or /ralphloop again replaces this loop.".to_string()),
        );
        if parsed.interval.is_none() {
            self.try_submit_due_prompt_loop();
        }
    }

    pub(super) fn handle_ralphloop_command_args(&mut self, args: String) {
        let trimmed = args.trim();
        if is_stop_command(trimmed) {
            self.stop_prompt_loop(/*ralph_only*/ true);
            return;
        }
        if trimmed.eq_ignore_ascii_case("status") {
            self.show_prompt_loop_status(/*ralph_only*/ true);
            return;
        }

        let parsed = match parse_ralph_loop_args(trimmed) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "The session is still starting; try /ralphloop again in a moment.".to_string(),
            );
            return;
        };

        let generation = self.replace_prompt_loop_generation();
        let prompt = ralph_submission_prompt(&parsed);
        self.prompt_loop = Some(PromptLoopState {
            generation,
            thread_id,
            prompt,
            kind: PromptLoopKind::Ralph {
                max_iterations: parsed.max_iterations,
                completion_promise: parsed.completion_promise.clone(),
            },
            phase: PromptLoopPhase::Due,
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        let limit = parsed
            .max_iterations
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unlimited".to_string());
        let promise = parsed
            .completion_promise
            .as_deref()
            .map(|value| format!("`<promise>{value}</promise>`"))
            .unwrap_or_else(|| "none".to_string());
        self.add_info_message(
            format!("RalphLoop started. Max iterations: {limit}; completion promise: {promise}."),
            Some("Use `/ralphloop stop` to cancel.".to_string()),
        );

        self.try_submit_due_prompt_loop();
    }

    pub(crate) fn on_prompt_loop_wakeup(
        &mut self,
        thread_id: ThreadId,
        generation: u64,
        reason: PromptLoopWakeupReason,
    ) {
        let Some(state) = self.prompt_loop.as_mut() else {
            return;
        };
        if state.thread_id != thread_id
            || state.generation != generation
            || self.thread_id != Some(thread_id)
        {
            return;
        }

        if reason == PromptLoopWakeupReason::Retry {
            if !state.retry_scheduled {
                return;
            }
            state.retry_scheduled = false;
        }
        let mut next_interval_deadline = None;
        let should_try_submit = match (&state.kind, state.phase, reason) {
            (
                PromptLoopKind::Timed {
                    interval: Some(interval),
                },
                PromptLoopPhase::Running,
                PromptLoopWakeupReason::Interval,
            ) => {
                state.timed_tick_pending = true;
                let deadline = next_prompt_loop_deadline(
                    state.next_tick_at,
                    *interval,
                    tokio::time::Instant::now(),
                );
                state.next_tick_at = Some(deadline);
                next_interval_deadline = Some(deadline);
                false
            }
            (
                PromptLoopKind::Timed {
                    interval: Some(interval),
                },
                _,
                PromptLoopWakeupReason::Interval,
            ) => {
                state.phase = PromptLoopPhase::Due;
                let deadline = next_prompt_loop_deadline(
                    state.next_tick_at,
                    *interval,
                    tokio::time::Instant::now(),
                );
                state.next_tick_at = Some(deadline);
                next_interval_deadline = Some(deadline);
                true
            }
            (PromptLoopKind::Timed { .. }, PromptLoopPhase::Due, PromptLoopWakeupReason::Retry)
            | (PromptLoopKind::Ralph { .. }, PromptLoopPhase::Due, PromptLoopWakeupReason::Retry) => {
                true
            }
            _ => false,
        };
        if let Some(deadline) = next_interval_deadline {
            self.schedule_prompt_loop_wakeup_at(
                thread_id,
                generation,
                deadline,
                PromptLoopWakeupReason::Interval,
            );
        }
        if should_try_submit {
            self.try_submit_due_prompt_loop();
        }
    }

    pub(super) fn on_prompt_loop_turn_complete(&mut self, response: &str, defer_submission: bool) {
        let Some(mut state) = self.prompt_loop.take() else {
            return;
        };
        let mut action = LoopTurnAction::None;

        match (&state.kind, state.phase) {
            (PromptLoopKind::Timed { interval: None }, PromptLoopPhase::Running) => {
                state.phase = PromptLoopPhase::Due;
                action = if defer_submission {
                    LoopTurnAction::Retry
                } else {
                    LoopTurnAction::Submit
                };
            }
            (PromptLoopKind::Timed { interval: Some(_) }, PromptLoopPhase::Running) => {
                if state.timed_tick_pending {
                    state.timed_tick_pending = false;
                    state.phase = PromptLoopPhase::Due;
                    if !defer_submission {
                        action = LoopTurnAction::Submit;
                    } else {
                        action = LoopTurnAction::Retry;
                    }
                } else {
                    state.phase = PromptLoopPhase::Waiting;
                }
            }
            (PromptLoopKind::Timed { .. }, PromptLoopPhase::Due) => {
                action = if defer_submission {
                    LoopTurnAction::Retry
                } else {
                    LoopTurnAction::Submit
                };
            }
            (
                PromptLoopKind::Ralph {
                    completion_promise,
                    max_iterations,
                },
                PromptLoopPhase::Running,
            ) => {
                if completion_promise
                    .as_deref()
                    .is_some_and(|promise| response_has_completion_promise(response, promise))
                {
                    action = LoopTurnAction::Stop(format!(
                        "RalphLoop completed after {} iteration{}.",
                        state.iteration,
                        if state.iteration == 1 { "" } else { "s" }
                    ));
                } else if let Some(limit) = *max_iterations
                    && state.iteration >= limit
                {
                    action = LoopTurnAction::Stop(format!(
                        "RalphLoop stopped after reaching the {limit}-iteration limit."
                    ));
                } else {
                    state.phase = PromptLoopPhase::Due;
                    if !defer_submission {
                        action = LoopTurnAction::Submit;
                    } else {
                        action = LoopTurnAction::Retry;
                    }
                }
            }
            (PromptLoopKind::Ralph { .. }, PromptLoopPhase::Due) => {
                action = if defer_submission {
                    LoopTurnAction::Retry
                } else {
                    LoopTurnAction::Submit
                };
            }
            _ => {}
        }

        self.prompt_loop = Some(state);
        match action {
            LoopTurnAction::None => {}
            LoopTurnAction::Submit => self.try_submit_due_prompt_loop(),
            LoopTurnAction::Retry => self.schedule_due_prompt_loop_retry(),
            LoopTurnAction::Stop(message) => {
                self.invalidate_prompt_loop();
                self.add_info_message(message, /*hint*/ None);
            }
        }
    }

    pub(super) fn on_prompt_loop_turn_failed(&mut self) {
        let Some(mut state) = self.prompt_loop.take() else {
            return;
        };
        let action = match (&state.kind, state.phase) {
            (PromptLoopKind::Timed { interval: None }, PromptLoopPhase::Running) => {
                state.phase = PromptLoopPhase::Due;
                LoopTurnAction::Retry
            }
            (PromptLoopKind::Timed { interval: Some(_) }, PromptLoopPhase::Running) => {
                if state.timed_tick_pending {
                    state.timed_tick_pending = false;
                    state.phase = PromptLoopPhase::Due;
                    LoopTurnAction::Retry
                } else {
                    state.phase = PromptLoopPhase::Waiting;
                    LoopTurnAction::None
                }
            }
            (PromptLoopKind::Timed { .. }, PromptLoopPhase::Due) => LoopTurnAction::Retry,
            (PromptLoopKind::Ralph { .. }, PromptLoopPhase::Running) => {
                state.iteration = state.iteration.saturating_sub(1);
                state.phase = PromptLoopPhase::Due;
                LoopTurnAction::Retry
            }
            (PromptLoopKind::Ralph { .. }, PromptLoopPhase::Due) => LoopTurnAction::Retry,
            _ => LoopTurnAction::None,
        };
        self.prompt_loop = Some(state);
        match action {
            LoopTurnAction::Submit => {
                self.try_submit_due_prompt_loop();
            }
            LoopTurnAction::Retry => self.schedule_due_prompt_loop_retry(),
            LoopTurnAction::Stop(message) => {
                self.invalidate_prompt_loop();
                self.add_info_message(message, /*hint*/ None);
            }
            LoopTurnAction::None => {}
        }
    }

    pub(super) fn cancel_prompt_loop_for_thread_change(&mut self) {
        self.invalidate_prompt_loop();
    }

    fn try_submit_due_prompt_loop(&mut self) {
        let Some(state) = self.prompt_loop.as_ref() else {
            return;
        };
        if state.phase != PromptLoopPhase::Due {
            return;
        }
        let thread_id = state.thread_id;
        let generation = state.generation;
        let goal_is_active = self
            .current_goal_status
            .as_ref()
            .is_some_and(GoalStatusState::is_active);
        if self.thread_id != Some(thread_id)
            || self.is_user_turn_pending_or_running()
            || goal_is_active
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            self.schedule_due_prompt_loop_retry();
            return;
        }

        let prompt = state.prompt.clone();
        let accepted = self
            .submit_user_message_with_shell_escape_policy(
                prompt.into(),
                ShellEscapePolicy::Disallow,
            )
            .is_some();
        if accepted {
            if let Some(state) = self.prompt_loop.as_mut()
                && state.thread_id == thread_id
                && state.generation == generation
                && state.phase == PromptLoopPhase::Due
            {
                state.phase = PromptLoopPhase::Running;
                state.iteration = state.iteration.saturating_add(1);
            }
        } else {
            self.schedule_due_prompt_loop_retry();
        }
    }

    fn schedule_due_prompt_loop_retry(&mut self) {
        let Some(state) = self.prompt_loop.as_mut() else {
            return;
        };
        if state.phase != PromptLoopPhase::Due || state.retry_scheduled {
            return;
        }
        state.retry_scheduled = true;
        let thread_id = state.thread_id;
        let generation = state.generation;
        self.schedule_prompt_loop_wakeup(
            thread_id,
            generation,
            BUSY_LOOP_RETRY,
            PromptLoopWakeupReason::Retry,
        );
    }

    fn schedule_prompt_loop_wakeup(
        &self,
        thread_id: ThreadId,
        generation: u64,
        delay: Duration,
        reason: PromptLoopWakeupReason,
    ) {
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            tx.send(AppEvent::PromptLoopWakeup {
                thread_id,
                generation,
                reason,
            });
        });
    }

    fn schedule_prompt_loop_wakeup_at(
        &self,
        thread_id: ThreadId,
        generation: u64,
        deadline: tokio::time::Instant,
        reason: PromptLoopWakeupReason,
    ) {
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            tx.send(AppEvent::PromptLoopWakeup {
                thread_id,
                generation,
                reason,
            });
        });
    }

    fn stop_prompt_loop(&mut self, ralph_only: bool) {
        let matches_requested_kind = self
            .prompt_loop
            .as_ref()
            .is_some_and(|state| matches!(&state.kind, PromptLoopKind::Ralph { .. }) == ralph_only);
        if !matches_requested_kind {
            let label = if ralph_only { "Ralph" } else { "timed" };
            self.add_info_message(format!("No {label} loop is active."), /*hint*/ None);
            return;
        }
        self.invalidate_prompt_loop();
        self.add_info_message("Prompt loop stopped.".to_string(), /*hint*/ None);
    }

    fn show_prompt_loop_status(&mut self, ralph_only: bool) {
        let Some(state) = self.prompt_loop.as_ref() else {
            self.add_info_message("No prompt loop is active.".to_string(), /*hint*/ None);
            return;
        };
        if matches!(&state.kind, PromptLoopKind::Ralph { .. }) != ralph_only {
            let active = if matches!(&state.kind, PromptLoopKind::Ralph { .. }) {
                "Ralph"
            } else {
                "timed"
            };
            self.add_info_message(
                format!("A {active} loop is active."),
                Some(if ralph_only {
                    "Use `/loop status` for details.".to_string()
                } else {
                    "Use `/ralphloop status` for details.".to_string()
                }),
            );
            return;
        }
        let details = match &state.kind {
            PromptLoopKind::Timed {
                interval: Some(interval),
            } => format!(
                "Timed loop: every {}; iteration {}; state {:?}.",
                format_interval(*interval),
                state.iteration,
                state.phase
            ),
            PromptLoopKind::Timed { interval: None } => format!(
                "Self-paced loop: iteration {}; state {:?}.",
                state.iteration, state.phase
            ),
            PromptLoopKind::Ralph {
                max_iterations,
                completion_promise,
            } => format!(
                "RalphLoop: iteration {}; max {}; completion promise {}; state {:?}.",
                state.iteration,
                max_iterations
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unlimited".to_string()),
                completion_promise.as_deref().unwrap_or("none"),
                state.phase
            ),
        };
        self.add_info_message(details, /*hint*/ None);
    }

    fn replace_prompt_loop_generation(&mut self) -> u64 {
        self.prompt_loop_generation = self.prompt_loop_generation.wrapping_add(1);
        self.prompt_loop_generation
    }

    fn invalidate_prompt_loop(&mut self) {
        self.prompt_loop_generation = self.prompt_loop_generation.wrapping_add(1);
        self.prompt_loop = None;
    }
}

fn is_stop_command(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "stop" | "cancel")
}

fn next_prompt_loop_deadline(
    previous: Option<tokio::time::Instant>,
    interval: Duration,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    let mut next = previous
        .and_then(|deadline| deadline.checked_add(interval))
        .unwrap_or_else(|| now + interval);
    while next <= now {
        let Some(advanced) = next.checked_add(interval) else {
            return now + interval;
        };
        next = advanced;
    }
    next
}

fn initial_prompt_loop_deadline(interval: Duration) -> Result<tokio::time::Instant, String> {
    tokio::time::Instant::now()
        .checked_add(interval)
        .ok_or_else(|| "Loop interval is too large.".to_string())
}

fn parse_timed_loop_args(value: &str) -> Result<TimedLoopArgs, String> {
    if value.is_empty() {
        return Err(LOOP_USAGE.to_string());
    }
    let mut parts = value.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    let (interval, prompt) = match parse_interval_token(first) {
        Some(Ok(interval)) => (Some(interval), rest),
        Some(Err(message)) => return Err(message),
        None => match parse_natural_interval_prefix(first, rest) {
            Some(Ok((interval, prompt))) => (Some(interval), prompt),
            Some(Err(message)) => return Err(message),
            None => (None, value),
        },
    };
    if prompt.is_empty() {
        return Err(LOOP_USAGE.to_string());
    }
    Ok(TimedLoopArgs {
        interval,
        prompt: prompt.to_string(),
    })
}

fn parse_natural_interval_prefix<'a>(
    amount: &str,
    remainder: &'a str,
) -> Option<Result<(Duration, &'a str), String>> {
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut parts = remainder.splitn(2, char::is_whitespace);
    let unit = parts
        .next()
        .unwrap_or_default()
        .trim_end_matches(|character: char| matches!(character, ',' | ';' | ':'));
    let prompt = parts.next().unwrap_or_default().trim();
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        _ => return None,
    };
    let interval = amount
        .parse::<u64>()
        .map_err(|_| "Loop interval is too large.".to_string())
        .and_then(|amount| {
            if amount == 0 {
                return Err(
                    "Loop interval must be greater than zero (for example `15 seconds`)."
                        .to_string(),
                );
            }
            amount
                .checked_mul(multiplier)
                .map(Duration::from_secs)
                .ok_or_else(|| "Loop interval is too large.".to_string())
        });
    Some(interval.map(|interval| (interval, prompt)))
}

fn parse_interval_token(value: &str) -> Option<Result<Duration, String>> {
    let (unit_index, unit) = value.char_indices().next_back()?;
    let number = &value[..unit_index];
    let multiplier = match unit.to_ascii_lowercase() {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return None,
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let amount = match number.parse::<u64>() {
        Ok(amount) if amount > 0 => amount,
        Ok(_) => {
            return Some(Err(
                "Loop interval must be greater than zero (for example `5m`).".to_string(),
            ));
        }
        Err(_) => return Some(Err("Loop interval is too large.".to_string())),
    };
    Some(
        amount
            .checked_mul(multiplier)
            .map(Duration::from_secs)
            .ok_or_else(|| "Loop interval is too large.".to_string()),
    )
}

fn parse_ralph_loop_args(value: &str) -> Result<RalphLoopArgs, String> {
    if value.is_empty() {
        return Err(RALPH_LOOP_USAGE.to_string());
    }
    let tokens = raw_argument_tokens(value)?;
    let option_start = tokens.iter().position(|token| {
        matches!(
            &value[token.start..token.end],
            "--max-iterations" | "--completion-promise"
        )
    });
    let Some(option_start) = option_start else {
        let natural_limit = parse_natural_ralph_iteration_limit(value, &tokens)?;
        let (prompt, max_iterations) = natural_limit
            .map(|(prompt, limit)| (prompt, Some(limit)))
            .unwrap_or((value, None));
        return Ok(RalphLoopArgs {
            prompt: prompt.to_string(),
            max_iterations,
            completion_promise: None,
        });
    };
    let mut prompt = value[..tokens[option_start].start].trim_end();
    if prompt.is_empty() {
        return Err(RALPH_LOOP_USAGE.to_string());
    }
    let mut max_iterations = None;
    let mut max_iterations_option_seen = false;
    let mut completion_promise = None;
    let mut index = option_start;
    while index < tokens.len() {
        let option = &value[tokens[index].start..tokens[index].end];
        match option {
            "--max-iterations" => {
                max_iterations_option_seen = true;
                let token = tokens.get(index + 1).ok_or_else(|| {
                    "--max-iterations requires a non-negative integer.".to_string()
                })?;
                let raw = decode_argument_token(&value[token.start..token.end])?;
                let parsed = raw.parse::<u64>().map_err(|_| {
                    format!("Invalid --max-iterations value `{raw}`; use 0 or a positive integer.")
                })?;
                max_iterations = (parsed > 0).then_some(parsed);
                index += 2;
            }
            "--completion-promise" => {
                let token = tokens.get(index + 1).ok_or_else(|| {
                    "--completion-promise requires text (quote multi-word values).".to_string()
                })?;
                let promise = decode_argument_token(&value[token.start..token.end])?;
                if promise.trim().is_empty() {
                    return Err("--completion-promise must not be empty.".to_string());
                }
                completion_promise = Some(promise);
                index += 2;
            }
            _ => {
                return Err(format!(
                    "Unexpected text `{option}` after /ralphloop options; put the complete prompt before the options."
                ));
            }
        }
    }
    if let Some((natural_prompt, limit)) =
        parse_natural_ralph_iteration_limit(value, &tokens[..option_start])?
    {
        if max_iterations_option_seen && max_iterations != Some(limit) {
            return Err(
                "Conflicting RalphLoop iteration limits; use either `for N iterations` or `--max-iterations N`."
                    .to_string(),
            );
        }
        prompt = natural_prompt;
        max_iterations = Some(limit);
    }
    Ok(RalphLoopArgs {
        prompt: prompt.to_string(),
        max_iterations,
        completion_promise,
    })
}

fn parse_natural_ralph_iteration_limit<'a>(
    value: &'a str,
    tokens: &[RawArgumentToken],
) -> Result<Option<(&'a str, u64)>, String> {
    if tokens.len() < 3 {
        return Ok(None);
    }
    let suffix_start = tokens.len() - 3;
    let for_token = tokens[suffix_start];
    let count_token = tokens[suffix_start + 1];
    let iterations_token = tokens[suffix_start + 2];
    let for_word = &value[for_token.start..for_token.end];
    let count_word = &value[count_token.start..count_token.end];
    let iterations_word = value[iterations_token.start..iterations_token.end]
        .trim_end_matches(|character: char| matches!(character, '.' | ',' | ';' | ':' | '!' | '?'));
    if !for_word.eq_ignore_ascii_case("for")
        || !matches!(
            iterations_word.to_ascii_lowercase().as_str(),
            "iteration" | "iterations"
        )
        || !count_word.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Ok(None);
    }
    let limit = count_word
        .parse::<u64>()
        .map_err(|_| "RalphLoop iteration limit is too large.".to_string())?;
    if limit == 0 {
        return Err("RalphLoop iteration limit must be greater than zero.".to_string());
    }
    let prompt = value[..for_token.start].trim_end();
    if prompt.is_empty() {
        return Err(RALPH_LOOP_USAGE.to_string());
    }
    Ok(Some((prompt, limit)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawArgumentToken {
    start: usize,
    end: usize,
}

fn raw_argument_tokens(value: &str) -> Result<Vec<RawArgumentToken>, String> {
    let mut chars = value.char_indices().peekable();
    let mut tokens = Vec::new();
    while chars.peek().is_some() {
        while chars
            .peek()
            .is_some_and(|(_, character)| character.is_whitespace())
        {
            chars.next();
        }
        let Some(&(start, _)) = chars.peek() else {
            break;
        };
        let mut quote = None;
        let mut escaped = false;
        while let Some(&(index, character)) = chars.peek() {
            if escaped {
                escaped = false;
                chars.next();
                continue;
            }
            match quote {
                Some('\'') => {
                    chars.next();
                    if character == '\'' {
                        quote = None;
                    }
                }
                Some('"') => {
                    chars.next();
                    if character == '"' {
                        quote = None;
                    } else if character == '\\' {
                        escaped = true;
                    }
                }
                Some(_) => unreachable!(),
                None if character.is_whitespace() => {
                    tokens.push(RawArgumentToken { start, end: index });
                    break;
                }
                None => {
                    chars.next();
                    match character {
                        '\'' | '"' => quote = Some(character),
                        '\\' => escaped = true,
                        _ => {}
                    }
                }
            }
        }
        if quote.is_some() || escaped {
            return Err("Unable to parse /ralphloop arguments; check the quoting.".to_string());
        }
        if chars.peek().is_none() {
            tokens.push(RawArgumentToken {
                start,
                end: value.len(),
            });
        }
    }
    Ok(tokens)
}

fn decode_argument_token(raw: &str) -> Result<String, String> {
    let decoded = shlex::split(raw)
        .ok_or_else(|| "Unable to parse /ralphloop arguments; check the quoting.".to_string())?;
    match decoded.as_slice() {
        [value] => Ok(value.clone()),
        _ => Err("Each /ralphloop option value must be one argument; quote spaces.".to_string()),
    }
}

fn ralph_submission_prompt(args: &RalphLoopArgs) -> String {
    let completion = args
        .completion_promise
        .as_deref()
        .map(|promise| {
            format!(
                "When every requirement is completely and verifiably true, end your response with the exact text `<promise>{promise}</promise>`. Never emit that promise merely to escape the loop."
            )
        })
        .unwrap_or_else(|| {
            "Continue making concrete progress on every iteration.".to_string()
        });
    let limit = args.max_iterations.map_or_else(
        || "No iteration limit is configured; the controller will continue until the user runs `/ralphloop stop` or the completion promise is satisfied.".to_string(),
        |limit| {
            format!(
                "The controller will stop after exactly {limit} completed iteration{} unless the completion promise is satisfied first or the user runs `/ralphloop stop`.",
                if limit == 1 { "" } else { "s" }
            )
        },
    );
    format!(
        "RalphLoop task:\n{}\n\nIteration contract: inspect the work already present in this same session, continue from it, test or verify the result, and do not repeat a failed approach unchanged. {completion} {limit}",
        args.prompt
    )
}

fn response_has_completion_promise(response: &str, promise: &str) -> bool {
    response
        .trim_end()
        .ends_with(&format!("<promise>{promise}</promise>"))
}

fn format_interval(interval: Duration) -> String {
    let seconds = interval.as_secs();
    for (unit_seconds, suffix) in [(86_400, "d"), (3_600, "h"), (60, "m")] {
        if seconds % unit_seconds == 0 {
            return format!("{}{suffix}", seconds / unit_seconds);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_loop_parses_optional_intervals() {
        assert_eq!(
            parse_timed_loop_args("5m check the deploy"),
            Ok(TimedLoopArgs {
                interval: Some(Duration::from_secs(300)),
                prompt: "check the deploy".to_string(),
            })
        );
        assert_eq!(
            parse_timed_loop_args("check the deploy"),
            Ok(TimedLoopArgs {
                interval: None,
                prompt: "check the deploy".to_string(),
            })
        );
        assert_eq!(
            parse_timed_loop_args("2h inspect status")
                .expect("valid interval")
                .interval,
            Some(Duration::from_secs(7_200))
        );
        assert_eq!(
            parse_timed_loop_args("15 seconds this is to test loop say hi"),
            Ok(TimedLoopArgs {
                interval: Some(Duration::from_secs(15)),
                prompt: "this is to test loop say hi".to_string(),
            })
        );
        assert_eq!(
            parse_timed_loop_args("2 hours inspect status")
                .expect("valid natural interval")
                .interval,
            Some(Duration::from_secs(7_200))
        );
        assert_eq!(
            parse_timed_loop_args("15 Seconds inspect status")
                .expect("case-insensitive natural interval")
                .interval,
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            parse_timed_loop_args("15 seconds, inspect status"),
            Ok(TimedLoopArgs {
                interval: Some(Duration::from_secs(15)),
                prompt: "inspect status".to_string(),
            })
        );
        assert_eq!(
            parse_timed_loop_args("1d inspect status")
                .expect("valid interval")
                .interval,
            Some(Duration::from_secs(86_400))
        );
        assert!(parse_timed_loop_args("0m never").is_err());
        assert!(parse_timed_loop_args("0 seconds never").is_err());
        assert!(parse_timed_loop_args("5m").is_err());
        assert!(parse_timed_loop_args("").is_err());
        assert_eq!(
            parse_timed_loop_args("部署 status")
                .expect("non-ASCII prompt")
                .prompt,
            "部署 status"
        );
    }

    #[test]
    fn ralph_loop_parses_options_without_losing_prompt() {
        assert_eq!(
            parse_ralph_loop_args("do a df-h command for 5 iterations"),
            Ok(RalphLoopArgs {
                prompt: "do a df-h command".to_string(),
                max_iterations: Some(5),
                completion_promise: None,
            })
        );
        assert_eq!(
            parse_ralph_loop_args("check once for 1 iteration"),
            Ok(RalphLoopArgs {
                prompt: "check once".to_string(),
                max_iterations: Some(1),
                completion_promise: None,
            })
        );
        assert_eq!(
            parse_ralph_loop_args("do a df-h command for 5 iterations."),
            Ok(RalphLoopArgs {
                prompt: "do a df-h command".to_string(),
                max_iterations: Some(5),
                completion_promise: None,
            })
        );
        let natural = parse_ralph_loop_args("do a df-h command for 5 iterations")
            .expect("natural iteration limit");
        assert!(ralph_submission_prompt(&natural).contains("exactly 5 completed iterations"));
        assert_eq!(
            parse_ralph_loop_args(
                "Build the API --completion-promise 'ALL DONE' --max-iterations 12"
            ),
            Ok(RalphLoopArgs {
                prompt: "Build the API".to_string(),
                max_iterations: Some(12),
                completion_promise: Some("ALL DONE".to_string()),
            })
        );
        assert_eq!(
            parse_ralph_loop_args("Fix tests --max-iterations 0")
                .expect("unlimited loop")
                .max_iterations,
            None
        );
        let preserved = parse_ralph_loop_args(
            r#"Keep  two spaces, "--max-iterations", and path\ name --max-iterations 2"#,
        )
        .expect("literal prompt syntax should be preserved");
        assert_eq!(
            preserved.prompt,
            r#"Keep  two spaces, "--max-iterations", and path\ name"#
        );
        assert_eq!(preserved.max_iterations, Some(2));
        assert_eq!(
            parse_ralph_loop_args("Explain --unknown literally")
                .expect("unknown prompt flags are prompt text")
                .prompt,
            "Explain --unknown literally"
        );
        assert!(parse_ralph_loop_args("Fix tests --max-iterations nope").is_err());
        assert!(parse_ralph_loop_args("Fix tests for 5 iterations --max-iterations 12").is_err());
        assert!(parse_ralph_loop_args("--completion-promise").is_err());
        assert!(parse_ralph_loop_args("").is_err());
    }

    #[test]
    fn ralph_completion_requires_exact_promise_tag() {
        assert!(response_has_completion_promise(
            "Verified. <promise>ALL DONE</promise>",
            "ALL DONE"
        ));
        assert!(!response_has_completion_promise(
            "Verified. ALL DONE",
            "ALL DONE"
        ));
        assert!(!response_has_completion_promise(
            "<promise>almost done</promise>",
            "ALL DONE"
        ));
        assert!(!response_has_completion_promise(
            "<promise>ALL DONE</promise> but work remains",
            "ALL DONE"
        ));
        assert!(response_has_completion_promise(
            "Verified. <promise>ALL DONE</promise>\n\n",
            "ALL DONE"
        ));
    }

    #[test]
    fn timed_loop_rejects_interval_beyond_instant_range() {
        assert!(initial_prompt_loop_deadline(Duration::from_secs(u64::MAX)).is_err());
    }

    #[test]
    fn timed_loop_deadline_keeps_cadence_after_missed_ticks() {
        let previous = tokio::time::Instant::now();
        let now = previous + Duration::from_secs(35);

        assert_eq!(
            next_prompt_loop_deadline(Some(previous), Duration::from_secs(10), now),
            previous + Duration::from_secs(40)
        );
    }

    #[tokio::test]
    async fn self_paced_command_submits_immediately_without_a_timer() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);

        chat.handle_loop_command_args("check the deploy".to_string());

        let state = chat.prompt_loop.as_ref().expect("loop is active");
        assert_eq!(state.kind, PromptLoopKind::Timed { interval: None });
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 1);
        assert_eq!(state.next_tick_at, None);
        let AppCommand::UserTurn { items, .. } =
            crate::chatwidget::tests::next_submit_op(&mut op_rx)
        else {
            panic!("expected immediate self-paced turn");
        };
        assert!(items.iter().any(
            |item| matches!(item, UserInput::Text { text, .. } if text == "check the deploy")
        ));
    }

    #[tokio::test]
    async fn idle_interval_wakeup_submits_model_prompt_and_coalesces_next_tick() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "!echo this remains a model prompt".to_string(),
            kind: PromptLoopKind::Timed {
                interval: Some(Duration::from_secs(300)),
            },
            phase: PromptLoopPhase::Waiting,
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at: Some(tokio::time::Instant::now() + Duration::from_secs(300)),
            retry_scheduled: false,
        });

        chat.on_prompt_loop_wakeup(thread_id, 1, PromptLoopWakeupReason::Interval);

        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 1);
        let op = crate::chatwidget::tests::next_submit_op(&mut op_rx);
        let AppCommand::UserTurn { items, .. } = op else {
            panic!("expected user turn");
        };
        assert!(items.iter().any(|item| {
            matches!(item, UserInput::Text { text, .. } if text == "!echo this remains a model prompt")
        }));

        chat.on_prompt_loop_wakeup(thread_id, 1, PromptLoopWakeupReason::Interval);
        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert!(state.timed_tick_pending);

        chat.on_prompt_loop_turn_complete("first result", /*follow_up_started*/ true);
        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Due);
        assert!(!state.timed_tick_pending);
        assert_eq!(state.iteration, 1);
    }

    #[tokio::test]
    async fn timed_loop_continues_after_second_iteration_until_stopped() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "repeat forever".to_string(),
            kind: PromptLoopKind::Timed {
                interval: Some(Duration::from_secs(300)),
            },
            phase: PromptLoopPhase::Running,
            iteration: 2,
            timed_tick_pending: false,
            next_tick_at: Some(tokio::time::Instant::now() + Duration::from_secs(300)),
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_complete("second result", /*defer_submission*/ false);
        assert_eq!(
            chat.prompt_loop
                .as_ref()
                .expect("loop remains active")
                .phase,
            PromptLoopPhase::Waiting
        );

        chat.on_prompt_loop_wakeup(thread_id, 1, PromptLoopWakeupReason::Interval);

        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 3);
        let AppCommand::UserTurn { items, .. } =
            crate::chatwidget::tests::next_submit_op(&mut op_rx)
        else {
            panic!("expected third loop iteration");
        };
        assert!(
            items.iter().any(
                |item| matches!(item, UserInput::Text { text, .. } if text == "repeat forever")
            )
        );
    }

    #[tokio::test]
    async fn self_paced_loop_continues_after_second_iteration_until_stopped() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "repeat without a timer".to_string(),
            kind: PromptLoopKind::Timed { interval: None },
            phase: PromptLoopPhase::Running,
            iteration: 2,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_complete("second result", /*defer_submission*/ false);

        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 3);
        let AppCommand::UserTurn { items, .. } =
            crate::chatwidget::tests::next_submit_op(&mut op_rx)
        else {
            panic!("expected third self-paced iteration");
        };
        assert!(items.iter().any(
            |item| matches!(item, UserInput::Text { text, .. } if text == "repeat without a timer")
        ));
    }

    #[tokio::test]
    async fn ralph_loop_stops_at_natural_iteration_limit_without_sixth_submit() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "do a df-h command".to_string(),
            kind: PromptLoopKind::Ralph {
                max_iterations: Some(5),
                completion_promise: None,
            },
            phase: PromptLoopPhase::Running,
            iteration: 5,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_complete("fifth result", /*defer_submission*/ false);

        assert!(chat.prompt_loop.is_none());
        while let Ok(op) = op_rx.try_recv() {
            assert!(
                !matches!(op, AppCommand::UserTurn { .. }),
                "iteration limit must prevent a sixth submission: {op:?}"
            );
        }
    }

    #[tokio::test]
    async fn failed_due_ralph_loop_retries_after_failure_cleanup() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "continue the task".to_string(),
            kind: PromptLoopKind::Ralph {
                max_iterations: Some(3),
                completion_promise: None,
            },
            phase: PromptLoopPhase::Due,
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_failed();

        let state = chat
            .prompt_loop
            .as_ref()
            .expect("Ralph loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Due);
        assert!(state.retry_scheduled);
        while let Ok(op) = op_rx.try_recv() {
            assert!(
                !matches!(op, AppCommand::UserTurn { .. }),
                "failure cleanup must not submit: {op:?}"
            );
        }

        chat.on_prompt_loop_wakeup(thread_id, 1, PromptLoopWakeupReason::Retry);

        let state = chat
            .prompt_loop
            .as_ref()
            .expect("Ralph loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 1);
        let _ = crate::chatwidget::tests::next_submit_op(&mut op_rx);
    }

    #[tokio::test]
    async fn failed_running_ralph_loop_retries_without_consuming_completed_iteration() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "continue the task".to_string(),
            kind: PromptLoopKind::Ralph {
                max_iterations: Some(5),
                completion_promise: None,
            },
            phase: PromptLoopPhase::Running,
            iteration: 3,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_failed();

        let state = chat.prompt_loop.as_ref().expect("RalphLoop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Due);
        assert_eq!(state.iteration, 2);
        assert!(state.retry_scheduled);
        chat.on_prompt_loop_wakeup(thread_id, 1, PromptLoopWakeupReason::Retry);
        let state = chat.prompt_loop.as_ref().expect("RalphLoop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 3);
        let _ = crate::chatwidget::tests::next_submit_op(&mut op_rx);
    }

    #[tokio::test]
    async fn failed_due_loop_leaves_queued_user_input_first() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "scheduled check".to_string(),
            kind: PromptLoopKind::Timed {
                interval: Some(Duration::from_secs(300)),
            },
            phase: PromptLoopPhase::Due,
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at: Some(tokio::time::Instant::now() + Duration::from_secs(300)),
            retry_scheduled: false,
        });
        chat.input_queue
            .queued_user_messages
            .push_back(UserMessage::from("user follow-up").into());
        chat.input_queue
            .queued_user_message_history_records
            .push_back(UserMessageHistoryRecord::UserMessageText);

        chat.on_prompt_loop_turn_failed();

        let state = chat.prompt_loop.as_ref().expect("loop remains active");
        assert_eq!(state.phase, PromptLoopPhase::Due);
        assert!(state.retry_scheduled);
        while let Ok(op) = op_rx.try_recv() {
            assert!(
                !matches!(op, AppCommand::UserTurn { .. }),
                "loop must not jump the queue: {op:?}"
            );
        }

        assert!(chat.maybe_send_next_queued_input());
        let AppCommand::UserTurn { items, .. } =
            crate::chatwidget::tests::next_submit_op(&mut op_rx)
        else {
            panic!("expected queued user turn");
        };
        assert!(
            items.iter().any(
                |item| matches!(item, UserInput::Text { text, .. } if text == "user follow-up")
            )
        );
        assert_eq!(
            chat.prompt_loop.as_ref().expect("loop remains due").phase,
            PromptLoopPhase::Due
        );
    }

    #[tokio::test]
    async fn due_ralph_loop_does_not_consume_preceding_turn_response() {
        let (mut chat, _event_rx, mut op_rx) =
            crate::chatwidget::tests::make_chatwidget_manual(/*model_override*/ None).await;
        let thread_id = ThreadId::new();
        chat.thread_id = Some(thread_id);
        chat.prompt_loop = Some(PromptLoopState {
            generation: 1,
            thread_id,
            prompt: "continue the task".to_string(),
            kind: PromptLoopKind::Ralph {
                max_iterations: Some(3),
                completion_promise: Some("DONE".to_string()),
            },
            phase: PromptLoopPhase::Due,
            iteration: 0,
            timed_tick_pending: false,
            next_tick_at: None,
            retry_scheduled: false,
        });

        chat.on_prompt_loop_turn_complete(
            "<promise>DONE</promise>",
            /*defer_submission*/ false,
        );

        let state = chat
            .prompt_loop
            .as_ref()
            .expect("preceding response must not stop Ralph loop");
        assert_eq!(state.phase, PromptLoopPhase::Running);
        assert_eq!(state.iteration, 1);
        let _ = crate::chatwidget::tests::next_submit_op(&mut op_rx);
    }
}
