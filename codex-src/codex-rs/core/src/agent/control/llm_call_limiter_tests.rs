use super::MAX_CONCURRENT_LLM_CALLS;
use crate::agent::AgentControl;
use std::time::Duration;

#[test]
fn root_and_subagents_share_the_same_two_slots() {
    // Sub-agents are created by cloning the root's `AgentControl`, so the clone
    // must contend for the *same* pool of LLM-call slots rather than getting a
    // fresh allocation.
    let root = AgentControl::default();
    let subagent = root.clone();

    assert_eq!(root.available_llm_call_slots(), MAX_CONCURRENT_LLM_CALLS);
    assert_eq!(
        subagent.available_llm_call_slots(),
        MAX_CONCURRENT_LLM_CALLS
    );

    let root_permit = root
        .try_acquire_llm_call_slot()
        .expect("first slot should be free");
    // The clone observes the slot the root just took.
    assert_eq!(
        subagent.available_llm_call_slots(),
        MAX_CONCURRENT_LLM_CALLS - 1
    );

    let subagent_permit = subagent
        .try_acquire_llm_call_slot()
        .expect("second slot should be free");
    assert_eq!(root.available_llm_call_slots(), 0);

    // With both shared slots taken, neither the root nor a third sub-agent can
    // grab a slot without waiting.
    assert!(root.try_acquire_llm_call_slot().is_none());
    assert!(subagent.try_acquire_llm_call_slot().is_none());

    drop(root_permit);
    assert_eq!(root.available_llm_call_slots(), 1);
    let reacquired = subagent
        .try_acquire_llm_call_slot()
        .expect("freed slot should be reusable by any agent");

    drop(subagent_permit);
    drop(reacquired);
    assert_eq!(root.available_llm_call_slots(), MAX_CONCURRENT_LLM_CALLS);
}

#[tokio::test]
async fn waiter_proceeds_after_a_slot_frees() {
    let control = AgentControl::default();
    let mut held = Vec::new();
    for _ in 0..MAX_CONCURRENT_LLM_CALLS {
        held.push(
            control
                .try_acquire_llm_call_slot()
                .expect("initial slots are free"),
        );
    }

    // A waiter cannot proceed while all slots are held.
    let waiter = {
        let control = control.clone();
        tokio::spawn(async move { control.acquire_llm_call_slot().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !waiter.is_finished(),
        "waiter must block while slots are full"
    );

    // Freeing a slot lets the queued waiter proceed.
    held.pop();
    let permit = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter should wake once a slot frees")
        .expect("waiter task should not panic");
    drop(permit);
    drop(held);
    assert_eq!(control.available_llm_call_slots(), MAX_CONCURRENT_LLM_CALLS);
}
