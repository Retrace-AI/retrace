use std::collections::HashSet;

const FALLBACK_MODEL_METADATA_WARNING_PREFIX: &str = "Model metadata for `";
const FALLBACK_MODEL_METADATA_WARNING_SUFFIX: &str =
    "` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.";

#[derive(Default)]
pub(super) struct WarningDisplayState {
    fallback_model_metadata_slugs: HashSet<String>,
}

impl WarningDisplayState {
    pub(super) fn should_display(&mut self, message: &str) -> bool {
        fallback_model_metadata_warning_slug(message)
            .is_none_or(|slug| self.fallback_model_metadata_slugs.insert(slug.to_string()))
    }
}

fn fallback_model_metadata_warning_slug(message: &str) -> Option<&str> {
    message
        .strip_prefix(FALLBACK_MODEL_METADATA_WARNING_PREFIX)?
        .strip_suffix(FALLBACK_MODEL_METADATA_WARNING_SUFFIX)
}

/// Append an actionable hint to the fallback-metadata warning at display time.
/// The core message is left unchanged (so dedup matching still works); this only
/// affects what the user sees.
pub(super) fn augment_warning(message: String) -> String {
    if fallback_model_metadata_warning_slug(&message).is_some() {
        format!(
            "{message} Quit and relaunch this session to load the model's metadata."
        )
    } else {
        message
    }
}
