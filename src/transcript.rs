use std::io::BufRead;

use serde::Deserialize;

const MAX_CONTENT_LEN: usize = 50_000;
const MAX_SUMMARY_LEN: usize = 2_000;

pub struct ParsedTranscript {
    pub content: String,
    pub cwd: String,
    pub project: String,
    pub session_id: String,
    pub summary: String,
}

#[derive(Deserialize)]
struct Entry {
    cwd: Option<String>,
    message: Option<Message>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "type")]
    type_: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: MessageContent,
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        #[allow(dead_code)]
        thinking: String,
    },
    #[serde(other)]
    Other,
}

pub fn parse_jsonl(reader: impl BufRead) -> Option<ParsedTranscript> {
    let mut content = String::new();
    let mut cwd = String::new();
    let mut session_id = String::new();
    let mut user_prompts = Vec::new();

    for line in reader.lines() {
        let line = line.ok()?;
        if line.is_empty() {
            continue;
        }
        let entry: Entry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Extract cwd and session_id from first entry that has them
        if cwd.is_empty()
            && let Some(ref c) = entry.cwd
        {
            cwd.clone_from(c);
        }
        if session_id.is_empty()
            && let Some(ref s) = entry.session_id
        {
            session_id.clone_from(s);
        }

        let type_ = entry.type_.as_deref().unwrap_or("");
        if type_ != "user" && type_ != "assistant" {
            continue;
        }

        let Some(message) = entry.message else {
            continue;
        };

        let role = message.role.as_deref().unwrap_or(type_);
        let label = if role == "user" { "User" } else { "Assistant" };

        let texts = extract_texts(&message.content);
        if texts.is_empty() {
            continue;
        }

        let joined = texts.join("\n");
        if role == "user" {
            user_prompts.push(joined.clone());
        }

        content.push_str(label);
        content.push_str(": ");
        content.push_str(&joined);
        content.push('\n');
    }

    if content.is_empty() || session_id.is_empty() {
        return None;
    }

    let project = cwd
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned();

    let mut summary = user_prompts.join(" | ");
    truncate_to_char_boundary(&mut summary, MAX_SUMMARY_LEN);
    truncate_to_char_boundary(&mut content, MAX_CONTENT_LEN);

    Some(ParsedTranscript {
        content,
        cwd,
        project,
        session_id,
        summary,
    })
}

fn extract_texts(content: &MessageContent) -> Vec<String> {
    match content {
        MessageContent::Text(s) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![s.clone()]
            }
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect(),
    }
}

pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let len = text.len();

    while start < len {
        let mut end = (start + chunk_size).min(len);
        // Snap to char boundary
        while end < len && !text.is_char_boundary(end) {
            end += 1;
        }

        chunks.push(text[start..end].to_owned());

        if end >= len {
            break;
        }

        let advance = if chunk_size > overlap {
            chunk_size - overlap
        } else {
            chunk_size
        };
        let mut next = start + advance;
        // Snap to char boundary
        while next < len && !text.is_char_boundary(next) {
            next += 1;
        }
        start = next;
    }

    chunks
}

fn truncate_to_char_boundary(s: &mut String, max_len: usize) {
    if s.len() <= max_len {
        return;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn jsonl(lines: &[&str]) -> Cursor<Vec<u8>> {
        let data = lines.join("\n");
        Cursor::new(data.into_bytes())
    }

    #[test]
    fn parse_minimal_transcript() {
        let lines = &[
            r#"{"type":"user","cwd":"/home/user/myproject","sessionId":"sess-1","message":{"role":"user","content":"Hello world"}}"#,
            r#"{"type":"assistant","cwd":"/home/user/myproject","sessionId":"sess-1","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}]}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert_eq!(result.session_id, "sess-1");
        assert_eq!(result.cwd, "/home/user/myproject");
        assert_eq!(result.project, "myproject");
        assert!(result.content.contains("User: Hello world"));
        assert!(result.content.contains("Assistant: Hi there!"));
        assert_eq!(result.summary, "Hello world");
    }

    #[test]
    fn parse_empty_transcript() {
        let lines: &[&str] = &[];
        assert!(parse_jsonl(jsonl(lines)).is_none());
    }

    #[test]
    fn parse_non_message_entries_skipped() {
        let lines = &[
            r#"{"type":"file-history-snapshot","messageId":"abc"}"#,
            r#"{"type":"system","message":{"role":"system","content":"system prompt"}}"#,
            r#"{"type":"user","cwd":"/home/user/proj","sessionId":"sess-2","message":{"role":"user","content":"question"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert_eq!(result.session_id, "sess-2");
        assert!(result.content.contains("User: question"));
        assert!(!result.content.contains("system"));
    }

    #[test]
    fn project_derived_from_cwd() {
        let lines = &[
            r#"{"type":"user","cwd":"/var/home/stintel/memory-server","sessionId":"s1","message":{"role":"user","content":"test"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert_eq!(result.project, "memory-server");
    }

    #[test]
    fn project_from_trailing_slash() {
        let lines = &[
            r#"{"type":"user","cwd":"/home/user/proj/","sessionId":"s1","message":{"role":"user","content":"test"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert_eq!(result.project, "proj");
    }

    #[test]
    fn tool_use_blocks_skipped() {
        let lines = &[
            r#"{"type":"assistant","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}},{"type":"text","text":"Result here"}]}}"#,
            r#"{"type":"user","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"user","content":"ok"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert!(result.content.contains("Result here"));
        assert!(!result.content.contains("tool_use"));
    }

    #[test]
    fn summary_truncation() {
        let long_prompt = "x".repeat(3000);
        let entry = serde_json::json!({
            "type": "user",
            "cwd": "/home/user/proj",
            "sessionId": "s1",
            "message": {"role": "user", "content": long_prompt}
        });
        let line = serde_json::to_string(&entry).unwrap();
        let result = parse_jsonl(jsonl(&[&line])).unwrap();
        assert!(result.summary.len() <= MAX_SUMMARY_LEN);
    }

    #[test]
    fn content_truncation() {
        let mut lines = Vec::new();
        for i in 0..1000 {
            let msg = format!("msg{i} ").repeat(20);
            let entry = serde_json::json!({
                "type": "user",
                "cwd": "/home/user/proj",
                "sessionId": "s1",
                "message": {"role": "user", "content": msg}
            });
            lines.push(serde_json::to_string(&entry).unwrap());
        }
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let result = parse_jsonl(jsonl(&line_refs)).unwrap();
        assert!(result.content.len() <= MAX_CONTENT_LEN);
    }

    #[test]
    fn multiple_user_prompts_in_summary() {
        let lines = &[
            r#"{"type":"user","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"user","content":"first question"}}"#,
            r#"{"type":"assistant","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"assistant","content":"answer"}}"#,
            r#"{"type":"user","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"user","content":"second question"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert_eq!(result.summary, "first question | second question");
    }

    #[test]
    fn no_session_id_returns_none() {
        let lines = &[
            r#"{"type":"user","cwd":"/home/user/proj","message":{"role":"user","content":"hello"}}"#,
        ];
        assert!(parse_jsonl(jsonl(lines)).is_none());
    }

    #[test]
    fn thinking_blocks_skipped() {
        let lines = &[
            r#"{"type":"assistant","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"internal thought"},{"type":"text","text":"visible response"}]}}"#,
            r#"{"type":"user","cwd":"/home/user/proj","sessionId":"s1","message":{"role":"user","content":"ok"}}"#,
        ];
        let result = parse_jsonl(jsonl(lines)).unwrap();
        assert!(result.content.contains("visible response"));
        assert!(!result.content.contains("internal thought"));
    }

    #[test]
    fn chunk_text_empty() {
        let chunks = chunk_text("", 100, 20);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn chunk_text_short() {
        let chunks = chunk_text("hello", 100, 20);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_text_exact_boundary() {
        let chunks = chunk_text("abcde", 5, 2);
        assert_eq!(chunks, vec!["abcde"]);
    }

    #[test]
    fn chunk_text_multi_chunk_overlap() {
        // 10 chars, chunk_size=4, overlap=2 → advance by 2 each time
        let chunks = chunk_text("abcdefghij", 4, 2);
        assert_eq!(chunks[0], "abcd");
        assert_eq!(chunks[1], "cdef");
        assert_eq!(chunks[2], "efgh");
        assert_eq!(chunks[3], "ghij");
        assert_eq!(chunks.len(), 4);
    }

    #[test]
    fn chunk_text_unicode_safety() {
        // "héllo" — é is 2 bytes, total 6 bytes
        let text = "héllo world";
        let chunks = chunk_text(text, 5, 1);
        // Every chunk must be valid UTF-8 (enforced by &str return)
        for chunk in &chunks {
            assert!(chunk.len() <= 6); // may snap past 5 to char boundary
        }
        // Reconstruct: all chars present in at least one chunk
        for c in text.chars() {
            assert!(
                chunks.iter().any(|ch| ch.contains(c)),
                "char {c:?} missing from chunks"
            );
        }
    }

    #[test]
    fn chunk_text_no_overlap() {
        let chunks = chunk_text("abcdefghij", 4, 0);
        assert_eq!(chunks[0], "abcd");
        assert_eq!(chunks[1], "efgh");
        assert_eq!(chunks[2], "ij");
        assert_eq!(chunks.len(), 3);
    }
}
