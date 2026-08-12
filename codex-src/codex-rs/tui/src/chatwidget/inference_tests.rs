use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::InferenceTracker;

#[test]
fn usage_updates_finalize_each_internal_call_without_counting_tool_gaps() {
    let start = Instant::now();
    let mut tracker = InferenceTracker::default();
    tracker.reset(start, 0, Some(100));

    tracker.record_delta("abcdefgh", start + Duration::from_millis(100));
    tracker.record_delta("ijklmnop", start + Duration::from_millis(200));
    tracker.record_usage(140, 40, start + Duration::from_millis(250));

    // The five-second tool gap is outside both calls because the usage update
    // reset the per-call stream clock.
    tracker.record_delta("qrstuvwx", start + Duration::from_secs(5));
    tracker.record_delta("yzabcdef", start + Duration::from_millis(5_100));
    tracker.record_usage(200, 60, start + Duration::from_millis(5_150));

    assert_eq!(
        (
            tracker.session_tokens,
            tracker.session_decode,
            tracker.last_call_tokens,
            tracker.last_call_decode,
            tracker.output_chars,
        ),
        (
            100.0,
            Duration::from_millis(300),
            60.0,
            Duration::from_millis(150),
            0,
        )
    );
}

#[test]
fn repeated_usage_notification_does_not_double_count_a_call() {
    let start = Instant::now();
    let mut tracker = InferenceTracker::default();
    tracker.reset(start, 0, Some(10));
    tracker.record_delta("abcdefgh", start + Duration::from_millis(100));
    tracker.record_delta("ijklmnop", start + Duration::from_millis(200));
    tracker.record_usage(50, 40, start + Duration::from_millis(250));

    let before = (tracker.session_tokens, tracker.session_decode);
    tracker.record_usage(50, 40, start + Duration::from_millis(300));

    assert_eq!((tracker.session_tokens, tracker.session_decode), before);
}

#[test]
fn reset_cumulative_usage_falls_back_to_the_last_call_output() {
    let start = Instant::now();
    let mut tracker = InferenceTracker::default();
    tracker.reset(start, 0, Some(1_000));
    tracker.record_delta("abcdefgh", start + Duration::from_millis(100));
    tracker.record_delta("ijklmnop", start + Duration::from_millis(200));
    tracker.record_usage(30, 30, start + Duration::from_millis(250));

    assert_eq!(
        (tracker.session_tokens, tracker.output_token_baseline),
        (30.0, Some(30))
    );
}

#[test]
fn completed_call_span_includes_unstreamed_tool_arguments() {
    let start = Instant::now();
    let mut tracker = InferenceTracker::default();
    tracker.reset(start, 0, Some(0));

    tracker.record_delta("visible reasoning", start + Duration::from_millis(100));
    // No client delta represents the long function-argument tail, but the usage
    // event still closes the same model call and supplies its exact token count.
    tracker.record_usage(400, 400, start + Duration::from_millis(3_100));

    assert_eq!(
        (tracker.session_tokens, tracker.session_decode),
        (400.0, Duration::from_secs(3))
    );
}
