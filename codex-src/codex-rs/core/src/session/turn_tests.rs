use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

struct RewriteAgentMessageContributor;

#[async_trait::async_trait]
impl TurnItemContributor for RewriteAgentMessageContributor {
    async fn contribute(
        &self,
        _thread_store: &ExtensionData,
        _turn_store: &ExtensionData,
        item: &mut TurnItem,
    ) -> Result<(), String> {
        if let TurnItem::AgentMessage(agent_message) = item {
            agent_message.content = vec![AgentMessageContent::Text {
                text: "plan contributed assistant text".to_string(),
            }];
        }
        Ok(())
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn response_item_text(item: &ResponseItem) -> String {
    let ResponseItem::Message { content, .. } = item else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_text_turn_input(text: &str) -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }
}

fn tool_identity(name: &str) -> ToolCallIdentity {
    ToolCallIdentity {
        namespace: None,
        name: name.to_string(),
        call_id: format!("{name}-call"),
        output_kind: ToolCallOutputKind::Function,
    }
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

#[test]
fn mid_task_status_continues_after_progress_intent() {
    assert!(message_needs_mid_task_continuation(
        "Now let me look at the existing benchmark script to understand how the previous runs were done."
    ));
    assert!(message_needs_mid_task_continuation(
        "I'll start by connecting to the remote server."
    ));
    assert!(message_needs_mid_task_continuation(
        "Q4_6GPU still loading (model tensors on GPUs 0-5). This is the last and largest run. Waiting for it to complete."
    ));
    assert!(message_needs_mid_task_continuation(
        "The benchmark is still running for 9 minutes 30 seconds. Let me wait a bit more."
    ));
}

#[test]
fn mid_task_status_does_not_continue_after_final_or_question() {
    assert!(!message_needs_mid_task_continuation(
        "Done. I verified the installed retrace binary."
    ));
    assert!(!message_needs_mid_task_continuation(
        "Do you want me to remove the provider?"
    ));
    assert!(!message_needs_mid_task_continuation(
        "Let me know which model you want."
    ));
}

#[tokio::test]
async fn build_mode_forces_mid_task_status_continuation() {
    let (_session, turn_context) = crate::session::tests::make_session_and_context().await;

    assert!(
        build_mid_task_status_continuation_message(
            &turn_context,
            Some("Let me wait longer and check again."),
            0,
        )
        .is_some()
    );
}

#[tokio::test]
async fn absolute_rampage_forces_mid_task_status_continuation() {
    let (_session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;

    assert!(
        build_mid_task_status_continuation_message(
            &turn_context,
            Some("Let me wait longer and check again."),
            0,
        )
        .is_some()
    );
}

#[tokio::test]
async fn absolute_rampage_blocks_completion_with_active_durable_mission() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;
    let thread_id = session.thread_id.to_string();
    let rampage_dir = turn_context.config.codex_home.as_path().join("rampage");
    std::fs::create_dir_all(&rampage_dir).expect("create rampage dir");
    std::fs::write(
        rampage_dir.join(format!("{thread_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "root_thread_id": thread_id,
            "active_mission_id": "mission-1",
            "missions": [{
                "id": "mission-1",
                "root_thread_id": session.thread_id.to_string(),
                "status": "running",
                "title": "Durable work",
                "goal": "Finish the task",
                "success_criteria": "Verifier passed",
                "phase": "workers",
                "controller_agent": "absolute-rampage",
                "support_agents": "both",
                "verifier_status": null,
                "verifier_notes": null,
                "latest_brief_id": null,
                "time_created": 1,
                "time_updated": 1,
                "time_completed": null
            }],
            "tasks": [{
                "id": "task-1",
                "mission_id": "mission-1",
                "parent_task_id": null,
                "worker_session_id": "worker-1",
                "status": "running",
                "kind": "work",
                "role": "system exploration agent",
                "title": "Inspect system",
                "instructions": "Inspect and report.",
                "dependencies": null,
                "model": null,
                "result": null,
                "confidence": null,
                "error": null,
                "time_created": 1,
                "time_started": 1,
                "time_finished": null
            }],
            "board_items": [],
            "briefs": [],
            "events": []
        }))
        .expect("serialize state"),
    )
    .expect("write rampage state");

    let prompt = build_rampage_active_mission_continuation_message(
        &session,
        &turn_context,
        Some("Done. I verified the work."),
        0,
    )
    .await
    .expect("active mission should block completion");
    let prompt_text = response_item_text(&prompt);

    assert!(prompt_text.contains("cannot complete while the durable mission is still active"));
    assert!(prompt_text.contains("rampage_control"));
    assert!(prompt_text.contains("action=complete"));
}

#[tokio::test]
async fn absolute_rampage_blocks_normal_tools_until_selected_support_agents_spawn() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;
    let thread_id = session.thread_id.to_string();
    let rampage_dir = turn_context.config.codex_home.as_path().join("rampage");
    std::fs::create_dir_all(&rampage_dir).expect("create rampage dir");
    std::fs::write(
        rampage_dir.join(format!("{thread_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "root_thread_id": thread_id,
            "active_mission_id": "mission-1",
            "missions": [{
                "id": "mission-1",
                "root_thread_id": session.thread_id.to_string(),
                "status": "running",
                "title": "Durable work",
                "goal": "Finish the task",
                "success_criteria": "Verifier passed",
                "phase": "startup",
                "controller_agent": "absolute-rampage",
                "support_agents": "both",
                "verifier_status": null,
                "verifier_notes": null,
                "latest_brief_id": null,
                "time_created": 1,
                "time_updated": 1,
                "time_completed": null
            }],
            "tasks": [],
            "board_items": [],
            "briefs": [],
            "events": []
        }))
        .expect("serialize state"),
    )
    .expect("write rampage state");

    let message =
        rampage_runtime_tool_block_message(&session, &turn_context, &tool_identity("exec_command"))
            .await
            .expect("normal tool should be blocked");

    assert!(message.contains("selected support agents were spawned"));
    assert!(message.contains("New Ideas Agent"));
    assert!(message.contains("Efficiency Monitoring Agent"));
    assert!(message.contains("rampage_spawn"));
}

#[tokio::test]
async fn absolute_rampage_blocks_raw_spawn_agent_even_after_startup_gate() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;

    let message =
        rampage_runtime_tool_block_message(&session, &turn_context, &tool_identity("spawn_agent"))
            .await
            .expect("raw spawn_agent should be blocked");

    assert!(message.contains("blocks raw"));
    assert!(message.contains("rampage_spawn"));
    assert!(message.contains("Questboard"));
}

#[tokio::test]
async fn mid_task_status_continuation_has_no_false_completion_cap() {
    let (_session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;

    assert!(
        build_mid_task_status_continuation_message(
            &turn_context,
            Some("Let me wait longer and check again."),
            10_000,
        )
        .is_some()
    );
}

#[test]
fn rampage_startup_guard_requires_question_and_mission_start_for_nontrivial_task() {
    let input = vec![user_text_turn_input(
        "evaluate the models on the remote server and benchmark all GPU combinations",
    )];
    let mut guard = RampageStartupGuard::new(ModeKind::AbsoluteRampage, &input, /*mission_already_active*/ false);

    assert!(guard.is_required());
    assert!(guard.should_block_tool_call(&tool_identity("exec_command")));
    assert!(guard.should_block_tool_call(&tool_identity("spawn_agent")));
    assert!(!guard.should_block_tool_call(&tool_identity("request_user_input")));
    assert!(!guard.should_block_tool_call(&tool_identity("rampage_control")));

    // Asking only the support-agent question is not enough; the verifier-config
    // question is also mandatory.
    guard.observe_tool_call(
        &tool_identity("request_user_input"),
        Some(r#"{"question":"Enable optional support agents?"}"#),
    );
    assert!(guard.should_block_tool_call(&tool_identity("exec_command")));

    guard.observe_tool_call(
        &tool_identity("request_user_input"),
        Some(r#"{"question":"After how many verification failures should I flag you, and what pass percentage counts as verified?"}"#),
    );
    assert!(guard.should_block_tool_call(&tool_identity("exec_command")));

    guard.observe_tool_call(&tool_identity("rampage_control"), None);
    assert!(!guard.should_block_tool_call(&tool_identity("exec_command")));
    assert!(guard.should_block_tool_call(&tool_identity("spawn_agent")));
    assert!(!guard.should_block_tool_call(&tool_identity("rampage_spawn")));
}

#[test]
fn rampage_startup_guard_allows_real_question_blocker() {
    let input = vec![user_text_turn_input(
        "build the provider workflow and verify it with the live models",
    )];
    let mut guard = RampageStartupGuard::new(ModeKind::ReadonlyResearch, &input, /*mission_already_active*/ false);

    assert!(guard.should_block_tool_call(&tool_identity("exec_command")));
    guard.observe_tool_call(
        &tool_identity("request_user_input"),
        Some(r#"{"question":"Enable optional support agents? Also, after how many verification failures should I flag you and what pass percentage is verified?"}"#),
    );
    assert!(guard.should_block_tool_call(&tool_identity("exec_command")));
    guard.observe_tool_call(&tool_identity("rampage_control"), None);
    assert!(!guard.should_block_tool_call(&tool_identity("exec_command")));
}

#[test]
fn rampage_startup_guard_skips_trivial_messages() {
    let input = vec![user_text_turn_input("hi")];
    let guard = RampageStartupGuard::new(ModeKind::AbsoluteRampage, &input, /*mission_already_active*/ false);

    assert!(!guard.is_required());
    assert!(!guard.should_block_tool_call(&tool_identity("exec_command")));
}
