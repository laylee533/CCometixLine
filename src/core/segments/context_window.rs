use super::{Segment, SegmentData};
use crate::config::{Config, InputData, ModelConfig, SegmentId, TranscriptEntry};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
struct ContextWindowDisplayOptions {
    use_progress_bar: bool,
    progress_bar_width: usize,
}

impl Default for ContextWindowDisplayOptions {
    fn default() -> Self {
        Self {
            use_progress_bar: false,
            progress_bar_width: 10,
        }
    }
}

#[derive(Default)]
pub struct ContextWindowSegment;

impl ContextWindowSegment {
    pub fn new() -> Self {
        Self
    }

    /// Get context limit for the specified model
    fn get_context_limit_for_model(model_id: &str) -> u32 {
        let model_config = ModelConfig::load();
        model_config.get_context_limit(model_id)
    }
}

fn load_display_options() -> ContextWindowDisplayOptions {
    let default = ContextWindowDisplayOptions::default();
    let config = match Config::load() {
        Ok(cfg) => cfg,
        Err(_) => return default,
    };

    let context_segment = match config.segments.iter().find(|s| s.id == SegmentId::ContextWindow) {
        Some(segment) => segment,
        None => return default,
    };

    let use_progress_bar = context_segment
        .options
        .get("use_progress_bar")
        .and_then(|v| v.as_bool())
        .unwrap_or(default.use_progress_bar);

    let progress_bar_width = context_segment
        .options
        .get("progress_bar_width")
        .and_then(|v| v.as_u64())
        .map(|v| v.clamp(5, 40) as usize)
        .unwrap_or(default.progress_bar_width);

    ContextWindowDisplayOptions {
        use_progress_bar,
        progress_bar_width,
    }
}

fn format_token_count(token_count: u32) -> String {
    if token_count >= 1000 {
        let k_value = token_count as f64 / 1000.0;
        if k_value.fract() == 0.0 {
            format!("{}k", k_value as u32)
        } else {
            format!("{:.1}k", k_value)
        }
    } else {
        token_count.to_string()
    }
}

fn format_percentage(rate: f64) -> String {
    if rate.fract() == 0.0 {
        format!("{:.0}%", rate)
    } else {
        format!("{:.1}%", rate)
    }
}

fn build_progress_bar(rate: f64, width: usize) -> String {
    let normalized = rate.clamp(0.0, 100.0);
    let filled_len = ((normalized / 100.0) * width as f64).round() as usize;
    let filled_len = filled_len.min(width);
    let empty_len = width.saturating_sub(filled_len);

    format!("{}{}", "█".repeat(filled_len), "░".repeat(empty_len))
}

fn render_context_primary(
    context_used_token_opt: Option<u32>,
    context_limit: u32,
    display_options: &ContextWindowDisplayOptions,
) -> String {
    let context_used_token = match context_used_token_opt {
        Some(token) => token,
        None => return "- · - tokens".to_string(),
    };

    let context_used_rate = (context_used_token as f64 / context_limit as f64) * 100.0;
    let percentage_display = format_percentage(context_used_rate);
    let tokens_display = format_token_count(context_used_token);

    if display_options.use_progress_bar {
        let bar = build_progress_bar(context_used_rate, display_options.progress_bar_width);
        let limit_display = format_token_count(context_limit);
        format!(
            "{} {} ({}/{})",
            bar, percentage_display, tokens_display, limit_display
        )
    } else {
        format!("{} · {} tokens", percentage_display, tokens_display)
    }
}

impl Segment for ContextWindowSegment {
    fn collect(&self, input: &InputData) -> Option<SegmentData> {
        // Dynamically determine context limit based on current model ID
        let context_limit = Self::get_context_limit_for_model(&input.model.id);
        let display_options = load_display_options();

        let context_used_token_opt = parse_transcript_usage(&input.transcript_path);

        let mut metadata = HashMap::new();
        match context_used_token_opt {
            Some(context_used_token) => {
                let context_used_rate = (context_used_token as f64 / context_limit as f64) * 100.0;
                metadata.insert("tokens".to_string(), context_used_token.to_string());
                metadata.insert("percentage".to_string(), context_used_rate.to_string());
            }
            None => {
                metadata.insert("tokens".to_string(), "-".to_string());
                metadata.insert("percentage".to_string(), "-".to_string());
            }
        }
        metadata.insert("limit".to_string(), context_limit.to_string());
        metadata.insert("model".to_string(), input.model.id.clone());

        Some(SegmentData {
            primary: render_context_primary(context_used_token_opt, context_limit, &display_options),
            secondary: String::new(),
            metadata,
        })
    }

    fn id(&self) -> SegmentId {
        SegmentId::ContextWindow
    }
}

fn parse_transcript_usage<P: AsRef<Path>>(transcript_path: P) -> Option<u32> {
    let path = transcript_path.as_ref();

    // Try to parse from current transcript file
    if let Some(usage) = try_parse_transcript_file(path) {
        return Some(usage);
    }

    // If file doesn't exist, try to find usage from project history
    if !path.exists() {
        if let Some(usage) = try_find_usage_from_project_history(path) {
            return Some(usage);
        }
    }

    None
}

fn try_parse_transcript_file(path: &Path) -> Option<u32> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();

    if lines.is_empty() {
        return None;
    }

    // Check if the last line is a summary
    let last_line = lines.last()?.trim();
    if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(last_line) {
        if entry.r#type.as_deref() == Some("summary") {
            // Handle summary case: find usage by leafUuid
            if let Some(leaf_uuid) = &entry.leaf_uuid {
                let project_dir = path.parent()?;
                return find_usage_by_leaf_uuid(leaf_uuid, project_dir);
            }
        }
    }

    // Normal case: find the last assistant message in current file
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            if entry.r#type.as_deref() == Some("assistant") {
                if let Some(message) = &entry.message {
                    if let Some(raw_usage) = &message.usage {
                        let normalized = raw_usage.clone().normalize();
                        return Some(normalized.display_tokens());
                    }
                }
            }
        }
    }

    None
}

fn find_usage_by_leaf_uuid(leaf_uuid: &str, project_dir: &Path) -> Option<u32> {
    // Search for the leafUuid across all session files in the project directory
    let entries = fs::read_dir(project_dir).ok()?;

    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        if let Some(usage) = search_uuid_in_file(&path, leaf_uuid) {
            return Some(usage);
        }
    }

    None
}

fn search_uuid_in_file(path: &Path, target_uuid: &str) -> Option<u32> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();

    // Find the message with target_uuid
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            if let Some(uuid) = &entry.uuid {
                if uuid == target_uuid {
                    // Found the target message, check its type
                    if entry.r#type.as_deref() == Some("assistant") {
                        // Direct assistant message with usage
                        if let Some(message) = &entry.message {
                            if let Some(raw_usage) = &message.usage {
                                let normalized = raw_usage.clone().normalize();
                                return Some(normalized.display_tokens());
                            }
                        }
                    } else if entry.r#type.as_deref() == Some("user") {
                        // User message, need to find the parent assistant message
                        if let Some(parent_uuid) = &entry.parent_uuid {
                            return find_assistant_message_by_uuid(&lines, parent_uuid);
                        }
                    }
                    break;
                }
            }
        }
    }

    None
}

fn find_assistant_message_by_uuid(lines: &[String], target_uuid: &str) -> Option<u32> {
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
            if let Some(uuid) = &entry.uuid {
                if uuid == target_uuid && entry.r#type.as_deref() == Some("assistant") {
                    if let Some(message) = &entry.message {
                        if let Some(raw_usage) = &message.usage {
                            let normalized = raw_usage.clone().normalize();
                            return Some(normalized.display_tokens());
                        }
                    }
                }
            }
        }
    }

    None
}

fn try_find_usage_from_project_history(transcript_path: &Path) -> Option<u32> {
    let project_dir = transcript_path.parent()?;

    // Find the most recent session file in the project directory
    let mut session_files: Vec<PathBuf> = Vec::new();
    let entries = fs::read_dir(project_dir).ok()?;

    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            session_files.push(path);
        }
    }

    if session_files.is_empty() {
        return None;
    }

    // Sort by modification time (most recent first)
    session_files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    session_files.reverse();

    // Try to find usage from the most recent session
    for session_path in &session_files {
        if let Some(usage) = try_parse_transcript_file(session_path) {
            return Some(usage);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_render_progress_bar_when_enabled() {
        let options = ContextWindowDisplayOptions {
            use_progress_bar: true,
            progress_bar_width: 10,
        };

        let rendered = render_context_primary(Some(74_000), 200_000, &options);

        assert_eq!(rendered, "████░░░░░░ 37% (74k/200k)");
    }

    #[test]
    fn should_keep_compact_format_when_progress_bar_disabled() {
        let options = ContextWindowDisplayOptions {
            use_progress_bar: false,
            progress_bar_width: 10,
        };

        let rendered = render_context_primary(Some(74_000), 200_000, &options);

        assert_eq!(rendered, "37% · 74k tokens");
    }

    #[test]
    fn should_clamp_progress_bar_for_out_of_range_values() {
        let options = ContextWindowDisplayOptions {
            use_progress_bar: true,
            progress_bar_width: 10,
        };

        let rendered = render_context_primary(Some(300_000), 200_000, &options);

        assert_eq!(rendered, "██████████ 150% (300k/200k)");
    }
}
