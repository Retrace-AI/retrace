use super::*;
use crate::app_event::CodexOsProviderModelRow;
use crate::app_event::CodexOsProviderRemoveSnapshot;
use crate::app_event::CodexOsProviderRow;

fn provider_model_row(id: &str, enabled: bool) -> CodexOsProviderModelRow {
    CodexOsProviderModelRow {
        id: id.to_string(),
        enabled,
        provider: "openai-com".to_string(),
        upstream_model: id.to_string(),
        context: Some(98304),
        output: "default".to_string(),
        thinking: "medium".to_string(),
        cap_thinking: false,
        usable: true,
        thinking_levels: Vec::new(),
        thinking_method: None,
    }
}

fn model_picker_snapshot(chat: &ChatWidget) -> String {
    normalize_snapshot_paths(strip_osc8_for_snapshot(&render_bottom_popup(
        chat, /*width*/ 100,
    )))
}

#[tokio::test]
async fn provider_models_picker_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_codexos_provider_connected(
        "openai-com".to_string(),
        Ok(vec![
            provider_model_row("gpt-5.5", false),
            provider_model_row("gpt-5.5-pro", true),
            provider_model_row("chat-latest", false),
        ]),
    );

    assert_chatwidget_snapshot!("provider_models_picker", model_picker_snapshot(&chat));
}

#[tokio::test]
async fn model_add_picker_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.on_codexos_model_add_list_loaded(Ok(vec![
        provider_model_row("gpt-5.5", true),
        provider_model_row("glm-5", false),
    ]));

    assert_chatwidget_snapshot!("model_add_picker", model_picker_snapshot(&chat));
}

#[tokio::test]
async fn provider_remove_models_picker_snapshot() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let snapshot = CodexOsProviderRemoveSnapshot {
        providers: vec![CodexOsProviderRow {
            id: "openai-com".to_string(),
            name: "OpenAI".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            key: "sk-test".to_string(),
        }],
        models: vec![
            provider_model_row("gpt-5.5", true),
            provider_model_row("gpt-5.5-pro", true),
        ],
    };
    chat.open_codexos_provider_remove_models_picker(snapshot, "openai-com".to_string());

    assert_chatwidget_snapshot!(
        "provider_remove_models_picker",
        model_picker_snapshot(&chat)
    );
}
