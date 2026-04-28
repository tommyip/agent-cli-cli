use std::{
    collections::HashMap,
    fs,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    Session, SessionProvider, TITLE_SEARCH_LIMIT, compact_title, display_path, limited_search_text,
    parse_timestamp,
};

pub(crate) fn load_sessions() -> Vec<Session> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude/projects");
    if !projects_dir.exists() {
        return Vec::new();
    }

    let index = load_indexes(&projects_dir);
    let mut sessions = Vec::new();

    for entry in WalkDir::new(projects_dir)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
    {
        let path = entry.path().to_path_buf();
        let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            continue;
        };

        if path
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new("subagents"))
        {
            continue;
        }

        let meta = index.get(&id).cloned().unwrap_or_default();
        let parsed = parse_session_file(&path);
        if meta.is_sidechain || parsed.is_sidechain {
            continue;
        }
        let is_command_only = parsed.is_command_only();
        let file_updated_at = file_modified_unix(&path);
        let cwd = meta
            .project_path
            .or_else(|| parsed.cwd.clone())
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("")).to_path_buf());
        let updated_at = meta
            .modified
            .or(file_updated_at)
            .or(parsed.updated_at)
            .unwrap_or_default();
        let created_at = meta.created.or(parsed.created_at);
        let first_prompt = meta.first_prompt.or_else(|| parsed.resumable_prompt());
        let session_name = parsed.slug.clone();
        let raw_title = meta
            .custom_title
            .or_else(|| parsed.custom_title.clone())
            .or(meta.ai_title)
            .or_else(|| parsed.ai_title.clone())
            .or(meta.last_prompt)
            .or_else(|| parsed.last_prompt.clone())
            .or(meta.summary)
            .or_else(|| parsed.summary.clone())
            .or_else(|| session_name.clone())
            .or_else(|| first_prompt.clone());
        let Some(raw_title) = raw_title else {
            continue;
        };
        let first_prompt = first_prompt.unwrap_or_default();
        if is_hidden_thread(&raw_title, &first_prompt, is_command_only) {
            continue;
        }
        let title = compact_title(&raw_title, &first_prompt);
        let title_search = limited_search_text(
            format!(
                "{title}\n{first_prompt}\n{}",
                session_name.as_deref().unwrap_or_default()
            ),
            TITLE_SEARCH_LIMIT,
        );

        sessions.push(Session {
            id,
            provider: SessionProvider::Claude,
            cwd: cwd.clone(),
            cwd_display: display_path(&cwd),
            title,
            title_search,
            message_search: String::new(),
            message_turns: Vec::new(),
            transcript_path: Some(path),
            created_at,
            updated_at,
            tokens: parsed.tokens,
        });
    }

    sessions
}

fn is_hidden_thread(raw_title: &str, first_prompt: &str, is_command_only: bool) -> bool {
    let raw_title = raw_title.trim();
    raw_title.starts_with("<command-") || is_command_only || is_command_prompt(first_prompt)
}

fn is_command_prompt(text: &str) -> bool {
    let text = text.trim_start();
    text.contains("<command-name>") || text.contains("<command-message>")
}

#[derive(Clone, Default)]
struct IndexEntry {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    summary: Option<String>,
    first_prompt: Option<String>,
    project_path: Option<PathBuf>,
    is_sidechain: bool,
    created: Option<i64>,
    modified: Option<i64>,
}

fn load_indexes(projects_dir: &Path) -> HashMap<String, IndexEntry> {
    let mut index = HashMap::new();
    for entry in WalkDir::new(projects_dir)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == "sessions-index.json")
    {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(entries) = value.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for item in entries {
            let Some(id) = item.get("sessionId").and_then(Value::as_str) else {
                continue;
            };
            index.insert(
                id.to_owned(),
                IndexEntry {
                    custom_title: item
                        .get("customTitle")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    ai_title: item
                        .get("aiTitle")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    last_prompt: item
                        .get("lastPrompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    summary: item
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    first_prompt: item
                        .get("firstPrompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    project_path: item
                        .get("projectPath")
                        .and_then(Value::as_str)
                        .map(PathBuf::from),
                    is_sidechain: item
                        .get("isSidechain")
                        .and_then(Value::as_bool)
                        .unwrap_or_default(),
                    created: item
                        .get("created")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp),
                    modified: item
                        .get("modified")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp),
                },
            );
        }
    }
    index
}

#[derive(Default)]
struct Head {
    cwd: Option<PathBuf>,
    first_prompt: Option<String>,
    command_fallback: Option<String>,
    slug: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    summary: Option<String>,
    is_sidechain: bool,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    tokens: Option<u64>,
}

impl Head {
    fn resumable_prompt(&self) -> Option<String> {
        self.first_prompt
            .clone()
            .or_else(|| self.command_fallback.clone())
    }

    fn is_command_only(&self) -> bool {
        self.first_prompt.is_none() && self.command_fallback.is_some()
    }
}

fn parse_session_file(path: &Path) -> Head {
    let Ok(file) = File::open(path) else {
        return Head::default();
    };
    let mut head = Head::default();
    let mut tokens = 0;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if head.cwd.is_none() {
            head.cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
        }
        fill_string_once(&mut head.slug, &value, "slug");
        head.is_sidechain |= value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        fill_string_latest(&mut head.custom_title, &value, "customTitle");
        fill_string_latest(&mut head.ai_title, &value, "aiTitle");
        fill_string_latest(&mut head.last_prompt, &value, "lastPrompt");
        fill_string_latest(&mut head.summary, &value, "summary");
        if let Some(ts) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        {
            head.created_at.get_or_insert(ts);
            head.updated_at = Some(ts);
        }
        tokens += sum_token_fields(&value);

        if head.first_prompt.is_none() {
            match scan_claude_prompt(&value, &mut head.command_fallback) {
                PromptScan::Prompt(prompt) => head.first_prompt = Some(prompt),
                PromptScan::Ignored => {}
            }
        }
    }

    if tokens > 0 {
        head.tokens = Some(tokens);
    }
    head
}

fn fill_string_once(target: &mut Option<String>, value: &Value, key: &str) {
    if target.is_none() {
        *target = value.get(key).and_then(Value::as_str).map(str::to_owned);
    }
}

fn fill_string_latest(target: &mut Option<String>, value: &Value, key: &str) {
    if let Some(text) = value.get(key).and_then(Value::as_str) {
        *target = Some(text.to_owned());
    }
}

enum PromptScan {
    Prompt(String),
    Ignored,
}

fn scan_claude_prompt(value: &Value, command_fallback: &mut Option<String>) -> PromptScan {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return PromptScan::Ignored;
    }
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or_default()
        || value
            .get("isCompactSummary")
            .and_then(Value::as_bool)
            .unwrap_or_default()
    {
        return PromptScan::Ignored;
    }

    let Some(content) = value.pointer("/message/content") else {
        return PromptScan::Ignored;
    };
    let Some(parts) = claude_prompt_text_parts(content) else {
        return PromptScan::Ignored;
    };
    for part in parts {
        let normalized = part.replace('\n', " ");
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(name) = tag_body(trimmed, "command-name") {
            if command_fallback.is_none() {
                *command_fallback = Some(name.to_owned());
            }
            continue;
        }
        if let Some(command) = tag_body(trimmed, "bash-input") {
            return PromptScan::Prompt(format!("! {}", command.trim()));
        }
        if starts_with_structured_marker(trimmed) {
            continue;
        }
        return PromptScan::Prompt(compact_line_ascii(trimmed, 200));
    }

    PromptScan::Ignored
}

fn claude_prompt_text_parts(content: &Value) -> Option<Vec<String>> {
    match content {
        Value::String(text) => Some(vec![text.clone()]),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    return None;
                }
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                    }
                }
            }
            Some(parts)
        }
        _ => None,
    }
}

fn tag_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn starts_with_structured_marker(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("[Request interrupted by user")
        || text
            .strip_prefix('<')
            .is_some_and(|rest| match rest.as_bytes().first() {
                Some(first) if first.is_ascii_lowercase() => rest
                    .find(|ch: char| ch == '>' || ch.is_whitespace())
                    .is_some(),
                _ => false,
            })
}

fn compact_line_ascii(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if text.chars().count() > max_chars {
        out.truncate(out.trim_end().len());
        out.push_str("...");
    }
    out
}

fn sum_token_fields(value: &Value) -> u64 {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                let here = if key.contains("tokens") {
                    value.as_u64().unwrap_or_default()
                } else {
                    0
                };
                here + sum_token_fields(value)
            })
            .sum(),
        Value::Array(items) => items.iter().map(sum_token_fields).sum(),
        _ => 0,
    }
}

fn file_modified_unix(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let datetime: DateTime<Utc> = modified.into();
    Some(datetime.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hides_generated_command_threads() {
        assert!(is_hidden_thread(
            "",
            "<command-message>simplify</command-message>\n<command-name>/simplify</command-name>",
            false
        ));
        assert!(is_hidden_thread(
            "<command-name>/theme</command-name>",
            "",
            false
        ));
        assert!(is_hidden_thread("theme", "theme", true));
        assert!(!is_hidden_thread(
            "fix ci",
            "Fix CI failures on the backend job",
            false
        ));
    }

    #[test]
    fn command_wrapper_does_not_block_later_real_prompt() {
        let mut command_fallback = None;
        let command = json!({
            "type": "user",
            "message": {
                "content": "<command-name>/theme</command-name>\n<command-message>theme</command-message>"
            }
        });
        assert!(matches!(
            scan_claude_prompt(&command, &mut command_fallback),
            PromptScan::Ignored
        ));
        assert_eq!(command_fallback.as_deref(), Some("/theme"));

        let real_prompt = json!({
            "type": "user",
            "message": {
                "content": "Current state: fix the Live2D UI"
            }
        });
        let PromptScan::Prompt(prompt) = scan_claude_prompt(&real_prompt, &mut command_fallback)
        else {
            panic!("expected later real prompt");
        };
        assert_eq!(prompt, "Current state: fix the Live2D UI");
        assert!(!is_hidden_thread("Live2D UI fix", &prompt, false));
    }

    #[test]
    fn latest_claude_title_metadata_wins() {
        let mut title = None;
        fill_string_latest(
            &mut title,
            &json!({"type": "custom-title", "customTitle": "set-dark-theme"}),
            "customTitle",
        );
        fill_string_latest(
            &mut title,
            &json!({"type": "custom-title", "customTitle": "Live2D UI fix"}),
            "customTitle",
        );
        assert_eq!(title.as_deref(), Some("Live2D UI fix"));
    }
}
