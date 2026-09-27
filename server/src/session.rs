pub use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::Role;
use marshaling_protocol::CompactionState;

/// A single entry in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    pub fn new(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            timestamp: Utc::now(),
        }
    }
}

/// Frontmatter metadata written at the top of each session file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub model_provider: String,
    pub model_id: String,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub version: String,
    /// Short summary of the conversation (first user message or auto-generated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Compacted context for older conversation turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionState>,
}

pub(crate) fn summary_from_user_content(content: &str) -> Option<String> {
    let trimmed: String = content.trim().replace('\n', " ");
    let words: Vec<&str> = trimmed
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let take_n = 8usize;
    Some(if words.len() > take_n {
        format!("{}...", words[..take_n].join(" "))
    } else {
        words.join(" ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_from_user_content_takes_first_words() {
        assert_eq!(
            summary_from_user_content("first line\nsecond line words here")
                .as_deref(),
            Some("first line second line words here")
        );
        assert_eq!(
            summary_from_user_content(
                "one two three four five six seven eight nine"
            )
            .as_deref(),
            Some("one two three four five six seven eight...")
        );
        assert_eq!(summary_from_user_content("   "), None);
    }
}
