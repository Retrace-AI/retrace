//! Model, collaboration, and reasoning popups for `ChatWidget`.
//!
//! These surfaces are tightly related because changing one often redirects
//! into another, especially while Plan mode is active.

use super::*;

impl ChatWidget {
    /// Open a popup to choose a quick auto model. Selecting "All models"
    /// opens the full picker with every available preset.
    pub(crate) fn open_model_popup(&mut self) {
        if !self.is_session_configured() {
            self.add_info_message(
                "Model selection is disabled until startup completes.".to_string(),
                /*hint*/ None,
            );
            return;
        }

        let presets: Vec<ModelPreset> = match self.model_catalog.try_list_models() {
            Ok(models) => models,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try /model again in a moment.".to_string(),
                    /*hint*/ None,
                );
                return;
            }
        };
        self.open_model_popup_with_presets(presets);
    }

    fn model_menu_header(&self, title: &str, subtitle: &str) -> Box<dyn Renderable> {
        let title = title.to_string();
        let subtitle = subtitle.to_string();
        let mut header = ColumnRenderable::new();
        header.push(Line::from(title.bold()));
        header.push(Line::from(subtitle.dim()));
        if let Some(warning) = self.model_menu_warning_line() {
            header.push(warning);
        }
        Box::new(header)
    }

    fn model_menu_warning_line(&self) -> Option<Line<'static>> {
        let base_url = self.custom_openai_base_url()?;
        let warning = format!(
            "Warning: Retrace base URL is overridden to {base_url}. Selecting models may not be supported or work properly."
        );
        Some(Line::from(warning.red()))
    }

    fn custom_openai_base_url(&self) -> Option<String> {
        if !self.config.model_provider.is_openai() {
            return None;
        }

        let base_url = self.config.model_provider.base_url.as_ref()?;
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            return None;
        }

        let normalized = trimmed.trim_end_matches('/');
        if normalized == DEFAULT_OPENAI_BASE_URL {
            return None;
        }

        Some(trimmed.to_string())
    }

    pub(crate) fn open_model_popup_with_presets(&mut self, presets: Vec<ModelPreset>) {
        let presets: Vec<ModelPreset> = presets
            .into_iter()
            .filter(|preset| preset.show_in_picker)
            .collect();

        let current_model = self.current_model();
        let current_label = presets
            .iter()
            .find(|preset| preset.model.as_str() == current_model)
            .map(|preset| preset.model.to_string())
            .unwrap_or_else(|| self.model_display_name().to_string());

        let (mut auto_presets, other_presets): (Vec<ModelPreset>, Vec<ModelPreset>) = presets
            .into_iter()
            .partition(|preset| Self::is_auto_model(&preset.model));

        if auto_presets.is_empty() {
            self.open_all_models_popup(other_presets);
            return;
        }

        auto_presets.sort_by_key(|preset| Self::auto_model_order(&preset.model));
        let mut items: Vec<SelectionItem> = auto_presets
            .into_iter()
            .map(|preset| {
                let description =
                    (!preset.description.is_empty()).then_some(preset.description.clone());
                let model = preset.model.clone();
                let should_prompt_plan_mode_scope = self.should_prompt_plan_mode_reasoning_scope(
                    model.as_str(),
                    Some(preset.default_reasoning_effort.clone()),
                );
                let actions = Self::model_selection_actions(
                    model.clone(),
                    Some(preset.default_reasoning_effort.clone()),
                    should_prompt_plan_mode_scope,
                );
                SelectionItem {
                    name: model.clone(),
                    description,
                    is_current: model.as_str() == current_model,
                    is_default: preset.is_default,
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        if !other_presets.is_empty() {
            let all_models = other_presets;
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenAllModelsPopup {
                    models: all_models.clone(),
                });
            })];

            let is_current = !items.iter().any(|item| item.is_current);
            let description = Some(format!(
                "Choose a specific model and reasoning level (current: {current_label})"
            ));

            items.push(SelectionItem {
                name: "All models".to_string(),
                description,
                is_current,
                actions,
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let header = self.model_menu_header(
            "Select Model",
            "Pick a quick auto mode or browse all models.",
        );
        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header,
            ..Default::default()
        });
    }

    fn is_auto_model(model: &str) -> bool {
        model.starts_with("codex-auto-")
    }

    fn auto_model_order(model: &str) -> usize {
        match model {
            "codex-auto-fast" => 0,
            "codex-auto-balanced" => 1,
            "codex-auto-thorough" => 2,
            _ => 3,
        }
    }

    pub(crate) fn open_all_models_popup(&mut self, presets: Vec<ModelPreset>) {
        if presets.is_empty() {
            self.add_info_message(
                "No additional models are available right now.".to_string(),
                /*hint*/ None,
            );
            return;
        }

        let mut items: Vec<SelectionItem> = Vec::new();
        // Entry point for enabling or connecting models without leaving /model.
        // Keep this routed through the same surface as `/model add`; the add
        // picker refreshes provider catalogs, lists every available model, and
        // still includes its own custom-provider sentinel.
        items.push(SelectionItem {
            name: "Add custom model".to_string(),
            description: Some(
                "Open /model add: refresh provider models, enable models, or connect a new provider"
                    .to_string(),
            ),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenModelAddPicker);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });
        for preset in presets.into_iter() {
            let description =
                (!preset.description.is_empty()).then_some(preset.description.to_string());
            let is_current = preset.model.as_str() == self.current_model();
            let single_supported_effort = preset.supported_reasoning_efforts.len() == 1;
            let preset_for_action = preset.clone();
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                let preset_for_event = preset_for_action.clone();
                tx.send(AppEvent::OpenReasoningPopup {
                    model: preset_for_event,
                });
            })];
            items.push(SelectionItem {
                name: preset.model.clone(),
                description,
                is_current,
                is_default: preset.is_default,
                actions,
                dismiss_on_select: single_supported_effort,
                dismiss_parent_on_child_accept: !single_supported_effort,
                ..Default::default()
            });
        }

        let header = self.model_menu_header(
            "Select Model and Effort",
            "/model add enables more models from your providers; /model probe re-detects capabilities",
        );
        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(self.bottom_pane.standard_popup_hint_line()),
            items,
            header,
            ..Default::default()
        });
    }

    fn model_selection_actions(
        model_for_action: String,
        effort_for_action: Option<ReasoningEffortConfig>,
        should_prompt_plan_mode_scope: bool,
    ) -> Vec<SelectionAction> {
        vec![Box::new(move |tx| {
            if should_prompt_plan_mode_scope {
                tx.send(AppEvent::OpenPlanReasoningScopePrompt {
                    model: model_for_action.clone(),
                    effort: effort_for_action.clone(),
                });
                return;
            }

            tx.send(AppEvent::UpdateModel(model_for_action.clone()));
            tx.send(AppEvent::UpdateReasoningEffort(effort_for_action.clone()));
            tx.send(AppEvent::PersistModelSelection {
                model: model_for_action.clone(),
                effort: effort_for_action.clone(),
            });
        })]
    }

    fn should_prompt_plan_mode_reasoning_scope(
        &self,
        selected_model: &str,
        selected_effort: Option<ReasoningEffortConfig>,
    ) -> bool {
        if !self.collaboration_modes_enabled()
            || self.active_mode_kind() != ModeKind::Plan
            || selected_model != self.current_model()
        {
            return false;
        }

        // Prompt whenever the selection is not a true no-op for both:
        // 1) the active Plan-mode effective reasoning, and
        // 2) the stored global defaults that would be updated by the fallback path.
        selected_effort != self.effective_reasoning_effort()
            || selected_model != self.current_collaboration_mode.model()
            || selected_effort != self.current_collaboration_mode.reasoning_effort()
    }

    pub(crate) fn open_plan_reasoning_scope_prompt(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) {
        let reasoning_phrase = match effort.as_ref() {
            Some(ReasoningEffortConfig::None) => "no reasoning".to_string(),
            Some(selected_effort) => {
                format!(
                    "{} reasoning",
                    Self::reasoning_effort_sentence_label(selected_effort)
                )
            }
            None => "the selected reasoning".to_string(),
        };
        let plan_only_description = format!("Always use {reasoning_phrase} in Plan mode.");
        let plan_reasoning_source = if let Some(plan_override) =
            self.config.plan_mode_reasoning_effort.as_ref()
        {
            format!(
                "user-chosen Plan override ({})",
                Self::reasoning_effort_sentence_label(plan_override)
            )
        } else if let Some(plan_mask) = collaboration_modes::plan_mask(self.model_catalog.as_ref())
        {
            match plan_mask
                .reasoning_effort
                .as_ref()
                .and_then(|effort| effort.as_ref())
            {
                Some(plan_effort) => format!(
                    "built-in Plan default ({})",
                    Self::reasoning_effort_sentence_label(plan_effort)
                ),
                None => "built-in Plan default (no reasoning)".to_string(),
            }
        } else {
            "built-in Plan default".to_string()
        };
        let all_modes_description = format!(
            "Set the global default reasoning level and the Plan mode override. This replaces the current {plan_reasoning_source}."
        );
        let subtitle = format!("Choose where to apply {reasoning_phrase}.");

        let plan_only_actions: Vec<SelectionAction> = vec![Box::new({
            let model = model.clone();
            let effort = effort.clone();
            move |tx| {
                tx.send(AppEvent::UpdateModel(model.clone()));
                tx.send(AppEvent::UpdatePlanModeReasoningEffort(effort.clone()));
                tx.send(AppEvent::PersistPlanModeReasoningEffort(effort.clone()));
            }
        })];
        let all_modes_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::UpdateModel(model.clone()));
            tx.send(AppEvent::UpdateReasoningEffort(effort.clone()));
            tx.send(AppEvent::UpdatePlanModeReasoningEffort(effort.clone()));
            tx.send(AppEvent::PersistPlanModeReasoningEffort(effort.clone()));
            tx.send(AppEvent::PersistModelSelection {
                model: model.clone(),
                effort: effort.clone(),
            });
        })];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(PLAN_MODE_REASONING_SCOPE_TITLE.to_string()),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            items: vec![
                SelectionItem {
                    name: PLAN_MODE_REASONING_SCOPE_PLAN_ONLY.to_string(),
                    description: Some(plan_only_description),
                    actions: plan_only_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: PLAN_MODE_REASONING_SCOPE_ALL_MODES.to_string(),
                    description: Some(all_modes_description),
                    actions: all_modes_actions,
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        self.notify(Notification::PlanModePrompt {
            title: PLAN_MODE_REASONING_SCOPE_TITLE.to_string(),
        });
    }

    /// Open a popup to choose the reasoning effort (stage 2) for the given model.
    pub(crate) fn open_reasoning_popup(&mut self, preset: ModelPreset) {
        let default_effort = preset.default_reasoning_effort;
        let supported = preset.supported_reasoning_efforts;
        let in_plan_mode =
            self.collaboration_modes_enabled() && self.active_mode_kind() == ModeKind::Plan;

        let warn_effort = if supported
            .iter()
            .any(|option| option.effort == ReasoningEffortConfig::XHigh)
        {
            Some(ReasoningEffortConfig::XHigh)
        } else if supported
            .iter()
            .any(|option| option.effort == ReasoningEffortConfig::High)
        {
            Some(ReasoningEffortConfig::High)
        } else {
            None
        };
        let warning_text = warn_effort.as_ref().map(|effort| {
            let effort_label = Self::reasoning_effort_label(effort);
            format!("⚠ {effort_label} reasoning effort can quickly consume Plus plan rate limits.")
        });
        let warn_for_model = preset.model.starts_with("gpt-5.1-codex")
            || preset.model.starts_with("gpt-5.1-codex-max")
            || preset.model.starts_with("gpt-5.2");

        let mut choices: Vec<ReasoningEffortConfig> = supported
            .iter()
            .map(|option| option.effort.clone())
            .collect();
        if choices.is_empty() {
            choices.push(default_effort.clone());
        }

        if choices.len() == 1 {
            let selected_effort = choices.first().cloned();
            let selected_model = preset.model;
            if self
                .should_prompt_plan_mode_reasoning_scope(&selected_model, selected_effort.clone())
            {
                self.app_event_tx
                    .send(AppEvent::OpenPlanReasoningScopePrompt {
                        model: selected_model,
                        effort: selected_effort,
                    });
            } else {
                // Only one variant, so it is chosen implicitly — still size this
                // model before applying, same as the multi-variant path.
                self.app_event_tx.send(AppEvent::OpenModelContextPopup {
                    model: selected_model,
                    effort: selected_effort,
                });
            }
            return;
        }

        let default_choice = choices
            .contains(&default_effort)
            .then(|| default_effort.clone())
            .or_else(|| choices.first().cloned())
            .or(Some(default_effort));

        let model_slug = preset.model.to_string();
        let is_current_model = self.current_model() == preset.model.as_str();
        let highlight_choice = if is_current_model {
            if in_plan_mode {
                self.config
                    .plan_mode_reasoning_effort
                    .clone()
                    .or_else(|| self.effective_reasoning_effort())
            } else {
                self.effective_reasoning_effort()
            }
        } else {
            default_choice.clone()
        };
        let selection_choice = highlight_choice.clone().or_else(|| default_choice.clone());
        let initial_selected_idx = choices
            .iter()
            .position(|choice| Some(choice) == selection_choice.as_ref());
        let mut items: Vec<SelectionItem> = Vec::new();
        for choice in choices.iter() {
            let effort = choice.clone();
            let mut effort_label = Self::reasoning_effort_label(&effort);
            if Some(choice) == default_choice.as_ref() {
                effort_label.push_str(" (default)");
            }

            let description = supported
                .iter()
                .find(|option| option.effort == effort)
                .map(|option| option.description.to_string())
                .filter(|text| !text.is_empty());

            let show_warning = warn_for_model && warn_effort.as_ref() == Some(&effort);
            let selected_description = if show_warning {
                warning_text.as_ref().map(|warning_message| {
                    description.as_ref().map_or_else(
                        || warning_message.clone(),
                        |d| format!("{d}\n{warning_message}"),
                    )
                })
            } else {
                None
            };

            let model_for_action = model_slug.clone();
            let choice_effort = Some(effort);
            let should_prompt_plan_mode_scope = self.should_prompt_plan_mode_reasoning_scope(
                model_slug.as_str(),
                choice_effort.clone(),
            );
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                if should_prompt_plan_mode_scope {
                    tx.send(AppEvent::OpenPlanReasoningScopePrompt {
                        model: model_for_action.clone(),
                        effort: choice_effort.clone(),
                    });
                } else {
                    // Variant chosen; now size THIS model. The limits are
                    // per-model (each has its own real window), so the context
                    // and output steps carry this one model only.
                    tx.send(AppEvent::OpenModelContextPopup {
                        model: model_for_action.clone(),
                        effort: choice_effort.clone(),
                    });
                }
            })];

            items.push(SelectionItem {
                name: effort_label,
                description,
                selected_description,
                is_current: is_current_model && Some(choice) == highlight_choice.as_ref(),
                actions,
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let mut header = ColumnRenderable::new();
        header.push(Line::from(
            format!("Select Reasoning Level for {model_slug}").bold(),
        ));

        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            ..Default::default()
        });
    }

    /// `/model` step 3: the context window for THIS model.
    ///
    /// Asked rather than probed: providers commonly do not publish a context
    /// window (z.ai's /models returns none), so detection silently falls back to
    /// a default and reports a number that is not the model's real window. The
    /// limit is per-model, so this only ever writes the one model named here.
    /// Begin the `/model` sizing flow for `model`: prefetch its saved
    /// context/output limits in the background, then open the context popup
    /// pre-selected to the saved value. Falls back to opening the popup
    /// immediately if the async result never arrives (the popup still works,
    /// just without a saved default).
    pub(crate) fn begin_model_sizing(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) {
        let tx = self.app_event_tx.clone();
        let model_for_task = model.clone();
        tokio::spawn(async move {
            let limits =
                super::slash_dispatch::fetch_saved_model_limits(model_for_task.clone()).await;
            tx.send(AppEvent::ModelSavedLimitsLoaded {
                model: model_for_task,
                effort,
                limits,
            });
        });
    }

    /// Saved limits finished loading: stash them so the popups can pre-select
    /// them, then open the context popup.
    pub(crate) fn on_model_saved_limits_loaded(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        limits: super::slash_dispatch::SavedModelLimits,
    ) {
        self.pending_model_sizing_limits
            .insert(model.clone(), limits);
        self.open_model_context_popup(model, effort);
    }

    pub(crate) fn open_model_context_popup(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) {
        // Kick off the sizing flow by prefetching this model's saved limits, then
        // opening the context popup pre-selected to the saved value. This is the
        // real entry point used by the `/model` flow via `begin_model_sizing`;
        // the direct method is kept for callers/tests that open the popup without
        // a prefetch.
        const PRESETS: [(&str, i64); 5] = [
            ("96k", 98_304),
            ("128k", 131_072),
            ("256k", 262_144),
            ("512k", 524_288),
            // A round 1,000,000 — deliberately not 1048576, which would overstate
            // a 1M model and cause context-overflow rejections.
            ("1M", 1_000_000),
        ];
        // Pre-select the value the user chose last time for this model (if any)
        // so they can just press Enter to keep it. Falls back to the current
        // in-memory context window when we are re-sizing the active model.
        let saved_context = self
            .pending_model_sizing_limits
            .get(&model)
            .and_then(|limits| limits.context_window)
            .or_else(|| {
                (self.current_model() == model.as_str())
                    .then_some(self.config.model_context_window)
                    .flatten()
            });
        let mut items: Vec<SelectionItem> = Vec::new();
        let mut matched_saved_preset = false;
        for (label, tokens) in PRESETS {
            let model = model.clone();
            let effort = effort.clone();
            let is_current = saved_context == Some(tokens);
            if is_current {
                matched_saved_preset = true;
            }
            items.push(SelectionItem {
                name: label.to_string(),
                description: Some(format!("{tokens} tokens")),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::ModelContextSelected {
                        model: model.clone(),
                        effort: effort.clone(),
                        context_window: Some(tokens),
                    });
                })],
                dismiss_on_select: true,
                is_current,
                ..Default::default()
            });
        }
        let custom_model = model.clone();
        let custom_effort = effort.clone();
        // If the saved value is not one of the presets (a custom number), focus
        // the Custom row and note the saved value so the user can keep it.
        let custom_is_current = saved_context.is_some() && !matched_saved_preset;
        let custom_description = match saved_context.filter(|_| custom_is_current) {
            Some(tokens) => format!("Enter an exact number of tokens (currently {tokens})"),
            None => "Enter an exact number of tokens".to_string(),
        };
        items.push(SelectionItem {
            name: "Custom\u{2026}".to_string(),
            description: Some(custom_description),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::ModelContextSelected {
                    model: custom_model.clone(),
                    effort: custom_effort.clone(),
                    context_window: None,
                });
            })],
            dismiss_on_select: true,
            is_current: custom_is_current,
            ..Default::default()
        });

        let mut header = ColumnRenderable::new();
        header.push(Line::from(
            format!("Select Context Window for {model}").bold(),
        ));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn on_model_context_selected(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        context_window: Option<i64>,
    ) {
        match context_window {
            Some(context_window) => self.open_model_output_popup(model, effort, context_window),
            None => {
                let tx = self.app_event_tx.clone();
                let view = CustomPromptView::new(
                    format!("Context window for {model}"),
                    "Enter context window in tokens, for example 1000000".to_string(),
                    String::new(),
                    Some("Custom context window".to_string()),
                    Box::new(move |value: String| {
                        match super::slash_dispatch::parse_token_count(&value) {
                        Some(context_window) => tx.send(AppEvent::ModelContextSelected {
                            model: model.clone(),
                            effort: effort.clone(),
                            context_window: Some(context_window),
                        }),
                        None => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            crate::history_cell::new_error_event(
                                "Context window must be a whole number of tokens (at least 1024)."
                                    .to_string(),
                            ),
                        ))),
                    }
                    }),
                );
                self.bottom_pane.show_view(Box::new(view));
                self.request_redraw();
            }
        }
    }

    /// `/model` step 4: the output limit for THIS model.
    pub(crate) fn open_model_output_popup(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        context_window: i64,
    ) {
        const PRESETS: [(&str, i64); 3] = [("8k", 8_192), ("16k", 16_384), ("32k", 32_768)];
        // Pre-select the output limit the user chose last time for this model
        // so they can just press Enter to keep it.
        let saved_output = self
            .pending_model_sizing_limits
            .get(&model)
            .and_then(|limits| limits.output_tokens);
        let mut items: Vec<SelectionItem> = Vec::new();
        let mut matched_saved_preset = false;
        for (label, tokens) in PRESETS {
            let model = model.clone();
            let effort = effort.clone();
            let is_current = saved_output == Some(tokens);
            if is_current {
                matched_saved_preset = true;
            }
            items.push(SelectionItem {
                name: label.to_string(),
                description: Some(format!("{tokens} tokens")),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::ModelOutputSelected {
                        model: model.clone(),
                        effort: effort.clone(),
                        context_window,
                        output_tokens: Some(tokens),
                    });
                })],
                dismiss_on_select: true,
                is_current,
                ..Default::default()
            });
        }
        let custom_model = model.clone();
        let custom_effort = effort.clone();
        let custom_is_current = saved_output.is_some() && !matched_saved_preset;
        let custom_description = match saved_output.filter(|_| custom_is_current) {
            Some(tokens) => format!("Enter an exact number of tokens (currently {tokens})"),
            None => "Enter an exact number of tokens".to_string(),
        };
        items.push(SelectionItem {
            name: "Custom\u{2026}".to_string(),
            description: Some(custom_description),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::ModelOutputSelected {
                    model: custom_model.clone(),
                    effort: custom_effort.clone(),
                    context_window,
                    output_tokens: None,
                });
            })],
            dismiss_on_select: true,
            is_current: custom_is_current,
            ..Default::default()
        });

        let mut header = ColumnRenderable::new();
        header.push(Line::from(
            format!("Select Output Tokens for {model}").bold(),
        ));
        let mut sub = ColumnRenderable::new();
        sub.push(Line::from(format!(
            "Context window: {context_window} tokens"
        )));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            subtitle: Some(format!("Context window: {context_window} tokens")),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn on_model_output_selected(
        &mut self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
        context_window: i64,
        output_tokens: Option<i64>,
    ) {
        let Some(output_tokens) = output_tokens else {
            let tx = self.app_event_tx.clone();
            let view = CustomPromptView::new(
                format!("Output tokens for {model}"),
                "Enter output limit in tokens, for example 8192".to_string(),
                String::new(),
                Some("Custom output limit".to_string()),
                Box::new(move |value: String| {
                    match super::slash_dispatch::parse_token_count(&value) {
                        Some(output_tokens) => tx.send(AppEvent::ModelOutputSelected {
                            model: model.clone(),
                            effort: effort.clone(),
                            context_window,
                            output_tokens: Some(output_tokens),
                        }),
                        None => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            crate::history_cell::new_error_event(
                                "Output limit must be a whole number of tokens (at least 1024)."
                                    .to_string(),
                            ),
                        ))),
                    }
                }),
            );
            self.bottom_pane.show_view(Box::new(view));
            self.request_redraw();
            return;
        };

        // Persist this one model's limits, then apply the model/effort selection.
        let model_for_apply = model.clone();
        let effort_for_apply = effort.clone();
        // The sizing flow for this model is complete; drop its prefetched saved
        // limits so a later `/model` run re-reads fresh values.
        self.pending_model_sizing_limits.remove(&model);
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let args = vec![
                "models".to_string(),
                "set".to_string(),
                model.clone(),
                "--context".to_string(),
                context_window.to_string(),
                "--output".to_string(),
                output_tokens.to_string(),
            ];
            let command = super::slash_dispatch::codexos_admin_command(args);
            match super::slash_dispatch::run_codexos_local_command_checked(command).await {
                Ok(_) => {
                    tx.send(AppEvent::UpdateModel(model_for_apply.clone()));
                    tx.send(AppEvent::UpdateReasoningEffort(effort_for_apply.clone()));
                    tx.send(AppEvent::PersistModelSelection {
                        model: model_for_apply,
                        effort: effort_for_apply,
                    });
                }
                Err(message) => {
                    tx.send(AppEvent::InsertHistoryCell(Box::new(
                        crate::history_cell::new_error_event(format!(
                            "Could not set limits for {model}: {message}"
                        )),
                    )));
                }
            }
        });
    }

    pub(super) fn reasoning_effort_label(effort: &ReasoningEffortConfig) -> String {
        match effort {
            ReasoningEffortConfig::None => "None".to_string(),
            ReasoningEffortConfig::Minimal => "Minimal".to_string(),
            ReasoningEffortConfig::Low => "Low".to_string(),
            ReasoningEffortConfig::Medium => "Medium".to_string(),
            ReasoningEffortConfig::High => "High".to_string(),
            ReasoningEffortConfig::XHigh => "Extra high".to_string(),
            ReasoningEffortConfig::Custom(value) => value.clone(),
        }
    }

    pub(super) fn reasoning_effort_sentence_label(effort: &ReasoningEffortConfig) -> String {
        match effort {
            ReasoningEffortConfig::Custom(value) => value.clone(),
            effort => Self::reasoning_effort_label(effort).to_lowercase(),
        }
    }

    pub(super) fn apply_model_and_effort_without_persist(
        &self,
        model: String,
        effort: Option<ReasoningEffortConfig>,
    ) {
        self.app_event_tx.send(AppEvent::UpdateModel(model));
        self.app_event_tx
            .send(AppEvent::UpdateReasoningEffort(effort));
    }

    fn apply_model_and_effort(&self, model: String, effort: Option<ReasoningEffortConfig>) {
        self.apply_model_and_effort_without_persist(model.clone(), effort.clone());
        self.app_event_tx
            .send(AppEvent::PersistModelSelection { model, effort });
    }
}
