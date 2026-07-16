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

fn startup_question_arguments() -> String {
    serde_json::json!({
        "questions": [
            {
                "id": "support_agents",
                "header": "Support agents",
                "question": "Enable optional support agents?",
                "options": [{
                    "label": "Both support agents (Recommended)",
                    "description": "Enable both advisory agents."
                }]
            },
            {
                "id": "verifier_pass_threshold",
                "header": "Verifier pass",
                "question": "What verification pass percentage counts as verified?",
                "options": [{"label": "80%", "description": "Require an 80% pass."}]
            },
            {
                "id": "verifier_max_failures",
                "header": "Verifier failures",
                "question": "After how many failed verification rounds should I flag you?",
                "options": [{"label": "3", "description": "Stop after three failures."}]
            }
        ]
    })
    .to_string()
}

fn startup_answers(support_agents: &str, pass_threshold: &str, max_failures: &str) -> String {
    serde_json::json!({
        "answers": {
            "support_agents": {"answers": [support_agents]},
            "verifier_pass_threshold": {"answers": [pass_threshold]},
            "verifier_max_failures": {"answers": [max_failures]}
        }
    })
    .to_string()
}

fn observe_startup_questions(
    guard: &mut RampageStartupGuard,
    arguments: &str,
    output_body: &str,
    success: Option<bool>,
) {
    let identity = tool_identity("request_user_input");
    guard.observe_tool_call(&identity, Some(arguments));
    guard.observe_tool_output(&ResponseItem::FunctionCallOutput {
        call_id: identity.call_id,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output_body.to_string()),
            success,
        },
    });
}

fn observe_rampage_start_output(
    guard: &mut RampageStartupGuard,
    arguments: &str,
    output_body: &str,
    success: Option<bool>,
) {
    let identity = tool_identity("rampage_control");
    guard.observe_tool_call(&identity, Some(arguments));
    guard.observe_tool_output(&ResponseItem::FunctionCallOutput {
        call_id: identity.call_id,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output_body.to_string()),
            success,
        },
    });
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
async fn rampage_keeps_mission_control_coordination_only_after_support_agents_spawn() {
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
            "tasks": [{
                "id": "task-new-ideas",
                "mission_id": "mission-1",
                "parent_task_id": null,
                "worker_session_id": "/root/new_ideas_agent",
                "status": "running",
                "kind": "research",
                "role": "New Ideas Agent",
                "title": "New Ideas Agent - worker review",
                "instructions": "Review named non-support workers.",
                "dependencies": null,
                "model": null,
                "result": null,
                "confidence": null,
                "error": null,
                "time_created": 2,
                "time_started": 2,
                "time_finished": null
            }, {
                "id": "task-efficiency",
                "mission_id": "mission-1",
                "parent_task_id": null,
                "worker_session_id": "/root/efficiency_monitoring_agent",
                "status": "running",
                "kind": "review",
                "role": "Efficiency Monitoring Agent",
                "title": "Efficiency Monitoring Agent - worker review",
                "instructions": "Review named non-support workers.",
                "dependencies": null,
                "model": null,
                "result": null,
                "confidence": null,
                "error": null,
                "time_created": 2,
                "time_started": 2,
                "time_finished": null
            }],
            "board_items": [],
            "briefs": [],
            "events": []
        }))
        .expect("serialize state"),
    )
    .expect("write rampage state");

    let message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("exec_command"),
        false,
    )
    .await
    .expect("normal tool should be blocked");

    assert!(message.contains("Mission Control is coordination-only"));
    assert!(message.contains("focused non-support worker"));
    assert!(message.contains("rampage_spawn"));
    assert!(!message.contains("Selected support agents are also missing"));

    for allowed_tool in [
        "request_user_input",
        "rampage_control",
        "rampage_board",
        "rampage_compact",
        "rampage_spawn",
        "update_plan",
        "list_agents",
        "wait_agent",
        "send_message",
        "interrupt_agent",
    ] {
        assert!(
            rampage_runtime_tool_block_message(
                &session,
                &turn_context,
                &tool_identity(allowed_tool),
                false,
            )
            .await
            .is_none(),
            "coordination tool {allowed_tool} should remain available"
        );
    }

    turn_context.collaboration_mode.mode = ModeKind::ReadonlyResearch;
    let readonly_message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("web_search"),
        false,
    )
    .await
    .expect("readonly Mission Control should also remain coordination-only");
    assert!(readonly_message.contains("Readonly Research"));
    assert!(readonly_message.contains("Mission Control is coordination-only"));

    let state_path = rampage_dir.join(format!("{thread_id}.json"));
    let paused_state = std::fs::read_to_string(&state_path)
        .expect("read state")
        .replacen("\"status\": \"running\"", "\"status\": \"paused\"", 1);
    std::fs::write(&state_path, paused_state).expect("pause mission");
    let paused_status =
        incomplete_mission_status_for_thread(turn_context.config.codex_home.as_path(), &thread_id)
            .await
            .expect("read paused mission");
    assert_eq!(
        paused_status.as_ref().map(|status| status.status.as_str()),
        Some("paused")
    );
    let paused_turn_guard = RampageStartupGuard::new(
        ModeKind::ReadonlyResearch,
        &[user_text_turn_input("continue the paused research mission")],
        /*mission_already_active*/ paused_status.is_some(),
        /*preexisting_mission_id*/ Some("mission-1".to_string()),
        /*controller_turn*/ true,
    );
    assert!(
        !paused_turn_guard.is_required(),
        "a paused mission must resume without a second startup sequence"
    );
    let paused_message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("exec_command"),
        false,
    )
    .await
    .expect("paused mission must not unlock direct Mission Control work");
    assert!(paused_message.contains("Mission Control is coordination-only"));
}

#[tokio::test]
async fn rampage_blocks_work_when_attempted_start_created_no_durable_mission() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;

    let message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("exec_command"),
        true,
    )
    .await
    .expect("missing durable startup state must block direct work");

    assert!(message.contains("startup has not created durable mission state"));
    assert!(message.contains("successful `rampage_control` action=start"));

    let short_request_message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("exec_command"),
        false,
    )
    .await
    .expect("short requests must not unlock direct controller work");
    assert!(short_request_message.contains("no incomplete durable mission exists"));
    assert!(short_request_message.contains("coordination-only"));
}

#[tokio::test]
async fn rampage_worker_can_do_assigned_work_but_cannot_spawn_children() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;
    turn_context.session_source = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::Other("rampage-worker".to_string()),
    );

    assert!(
        rampage_runtime_tool_block_message(
            &session,
            &turn_context,
            &tool_identity("exec_command"),
            false,
        )
        .await
        .is_none(),
        "worker execution tools must remain available"
    );
    let spawn_message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("spawn_agent"),
        false,
    )
    .await
    .expect("worker must not create child workers");
    assert!(spawn_message.contains("complete its assigned task"));

    let input = vec![user_text_turn_input(
        "inspect the requested implementation and return structured evidence to Mission Control",
    )];
    let guard = RampageStartupGuard::new(
        ModeKind::AbsoluteRampage,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ false,
    );
    assert!(!guard.is_required());
}

#[tokio::test]
async fn absolute_rampage_blocks_raw_spawn_agent_even_after_startup_gate() {
    let (session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    turn_context.collaboration_mode.mode = ModeKind::AbsoluteRampage;

    let message = rampage_runtime_tool_block_message(
        &session,
        &turn_context,
        &tool_identity("spawn_agent"),
        false,
    )
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
fn rampage_startup_guard_accepts_matching_answers_only_after_successful_start_output() {
    let input = vec![user_text_turn_input(
        "evaluate the models on the remote server and benchmark all GPU combinations",
    )];
    let mut guard = RampageStartupGuard::new(
        ModeKind::AbsoluteRampage,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );

    assert!(guard.is_required());
    assert!(guard.should_block_tool_call(&tool_identity("exec_command"), None));
    assert!(guard.should_block_tool_call(&tool_identity("spawn_agent"), None));
    assert!(!guard.should_block_tool_call(&tool_identity("request_user_input"), None));
    assert!(!guard.should_block_tool_call(&tool_identity("rampage_control"), None));
    assert!(guard.should_block_tool_call(
        &tool_identity("rampage_control"),
        Some(r#"{"action":"start"}"#),
    ));

    observe_startup_questions(
        &mut guard,
        &startup_question_arguments(),
        &startup_answers("Both support agents (Recommended)", "80%", "3"),
        Some(true),
    );
    assert!(guard.should_block_tool_call(&tool_identity("exec_command"), None));

    guard.observe_tool_call(
        &tool_identity("rampage_control"),
        Some(r#"{"action":"status"}"#),
    );
    assert!(guard.should_block_tool_call(&tool_identity("exec_command"), None));

    let start_arguments = r#"{"action":"start","support_agents":"both","verifier_pass_threshold":80,"verifier_max_failures":3}"#;
    assert!(
        !guard.should_block_tool_call(&tool_identity("rampage_control"), Some(start_arguments),)
    );
    let start_identity = tool_identity("rampage_control");
    guard.observe_tool_call(&start_identity, Some(start_arguments));
    assert!(
        guard.should_block_tool_call(&tool_identity("exec_command"), None),
        "the start call alone must not satisfy startup"
    );
    guard.observe_tool_output(&ResponseItem::FunctionCallOutput {
        call_id: start_identity.call_id,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(r#"{"ok":true}"#.to_string()),
            success: Some(true),
        },
    });
    assert!(!guard.should_block_tool_call(&tool_identity("exec_command"), None));
    assert!(guard.should_block_tool_call(&tool_identity("spawn_agent"), None));
    assert!(!guard.should_block_tool_call(&tool_identity("rampage_spawn"), None));
}

#[test]
fn rampage_startup_guard_blocks_start_arguments_that_mismatch_user_answers() {
    let input = vec![user_text_turn_input(
        "build the provider workflow and verify it with the live models",
    )];
    let mut guard = RampageStartupGuard::new(
        ModeKind::ReadonlyResearch,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );

    observe_startup_questions(
        &mut guard,
        &startup_question_arguments(),
        &startup_answers("New Ideas only", "90%", "infinite"),
        Some(true),
    );

    for mismatched in [
        r#"{"action":"start","support_agents":"both","verifier_pass_threshold":90,"verifier_max_failures":"infinite"}"#,
        r#"{"action":"start","support_agents":"new_ideas_only","verifier_pass_threshold":80,"verifier_max_failures":"infinite"}"#,
        r#"{"action":"start","support_agents":"new_ideas_only","verifier_pass_threshold":90,"verifier_max_failures":3}"#,
    ] {
        assert!(guard.should_block_tool_call(&tool_identity("rampage_control"), Some(mismatched),));
    }
    assert!(!guard.should_block_tool_call(
        &tool_identity("rampage_control"),
        Some(
            r#"{"action":"start","support_agents":"new_ideas_only","verifier_pass_threshold":90,"verifier_max_failures":"infinite"}"#,
        ),
    ));
    assert!(guard.should_block_tool_call(&tool_identity("exec_command"), None));
}

#[test]
fn rampage_startup_guard_rejects_malformed_missing_and_non_distinct_answers() {
    let input = vec![user_text_turn_input(
        "inspect the implementation and verify the result",
    )];
    let mut guard = RampageStartupGuard::new(
        ModeKind::AbsoluteRampage,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );
    let questions = startup_question_arguments();

    observe_startup_questions(
        &mut guard,
        &questions,
        &startup_answers("both", "80%", "3"),
        Some(false),
    );
    assert!(guard.support_agents_answer.is_none());

    observe_startup_questions(
        &mut guard,
        &questions,
        r#"{"answers":{"support_agents":{"answers":["both"]},"verifier_pass_threshold":{"answers":["eighty"]}}}"#,
        Some(true),
    );
    assert!(guard.support_agents_answer.is_some());
    assert!(guard.verifier_pass_threshold_answer.is_none());
    assert!(guard.verifier_max_failures_answer.is_none());

    observe_startup_questions(&mut guard, &questions, "not json", Some(true));
    assert!(guard.should_block_tool_call(
        &tool_identity("rampage_control"),
        Some(
            r#"{"action":"start","support_agents":"both","verifier_pass_threshold":80,"verifier_max_failures":3}"#,
        ),
    ));

    for (header, question, answer) in [
        ("Support agents", "Enable optional support agents?", "both"),
        (
            "Verifier pass",
            "What verification pass percentage should apply?",
            "80%",
        ),
        (
            "Verifier failures",
            "What maximum failed verification rounds should apply?",
            "3",
        ),
    ] {
        let reused_id_question = serde_json::json!({
            "questions": [{
                "id": "shared",
                "header": header,
                "question": question,
                "options": [{"label": answer, "description": "Startup selection."}]
            }]
        })
        .to_string();
        observe_startup_questions(
            &mut guard,
            &reused_id_question,
            &serde_json::json!({"answers": {"shared": {"answers": [answer]}}}).to_string(),
            Some(true),
        );
    }
    assert!(guard.support_agents_answer.is_some());
    assert!(guard.verifier_pass_threshold_answer.is_some());
    assert!(guard.verifier_max_failures_answer.is_some());
    assert!(!guard.has_distinct_startup_answers());
    assert!(guard.should_block_tool_call(
        &tool_identity("rampage_control"),
        Some(
            r#"{"action":"start","support_agents":"both","verifier_pass_threshold":80,"verifier_max_failures":3}"#,
        ),
    ));
}

#[test]
fn rampage_startup_guard_rejects_failed_cancelled_or_negative_start_outputs() {
    let input = vec![user_text_turn_input(
        "build the provider workflow and verify it with the live models",
    )];
    let mut guard = RampageStartupGuard::new(
        ModeKind::ReadonlyResearch,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );
    observe_startup_questions(
        &mut guard,
        &startup_question_arguments(),
        &startup_answers("No support agents", "100%", "1"),
        Some(true),
    );
    let start_arguments = r#"{"action":"start","support_agents":"none","verifier_pass_threshold":100,"verifier_max_failures":1}"#;

    observe_rampage_start_output(&mut guard, start_arguments, r#"{"ok":true}"#, Some(false));
    assert!(!guard.is_satisfied());
    observe_rampage_start_output(&mut guard, start_arguments, r#"{"ok":false}"#, Some(true));
    assert!(!guard.is_satisfied());
    observe_rampage_start_output(
        &mut guard,
        start_arguments,
        "rampage_control was cancelled",
        None,
    );
    assert!(!guard.is_satisfied());
}

#[test]
fn rampage_startup_guard_skips_questions_for_trivial_messages() {
    let input = vec![user_text_turn_input("hi")];
    let guard = RampageStartupGuard::new(
        ModeKind::AbsoluteRampage,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );

    assert!(!guard.is_required());
    // Pure chat does not need durable startup. Any attempted work tool is still
    // rejected by `rampage_runtime_tool_block_message`.
    assert!(!guard.should_block_tool_call(&tool_identity("exec_command"), None));
}

#[test]
fn rampage_startup_guard_requires_durable_start_for_short_real_task() {
    let input = vec![user_text_turn_input(
        "summarize this repository architecture",
    )];
    let guard = RampageStartupGuard::new(
        ModeKind::AbsoluteRampage,
        &input,
        /*mission_already_active*/ false,
        /*preexisting_mission_id*/ None,
        /*controller_turn*/ true,
    );

    assert!(guard.is_required());
    assert!(guard.should_block_tool_call(&tool_identity("exec_command"), None));
}
