//! Canonical-history helpers: turn splitting, transcript keys, history preamble.

use sha2::Digest;
use sha2::Sha256;

use super::types::CanonMessage;
use super::types::ToolSpec;
use super::types::UserPart;

/// Split the history into `(prefix, tail)` where `tail` is the trailing run of
/// user / tool-result messages that forms the new turn input (mirrors
/// pi-claude-bridge's `turnStart`: one index is the single source of truth).
pub fn split_turn(history: &[CanonMessage]) -> (&[CanonMessage], &[CanonMessage]) {
    let mut start = history.len();
    while start > 0 {
        match &history[start - 1] {
            CanonMessage::User(_) | CanonMessage::ToolResult(_) => start -= 1,
            CanonMessage::Assistant { .. } => break,
        }
    }
    history.split_at(start)
}

pub fn hash_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub fn tools_key(tools: &[ToolSpec]) -> String {
    let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut s = String::new();
    for t in sorted {
        s.push_str(&t.name);
        s.push('\u{1}');
        s.push_str(&t.description);
        s.push('\u{1}');
        s.push_str(&t.raw.to_string());
        s.push('\u{2}');
    }
    hash_hex(&s)
}

fn canon_text(s: &str) -> &str {
    s.trim()
}

fn push_message(buf: &mut String, m: &CanonMessage) {
    match m {
        CanonMessage::User(parts) => {
            buf.push_str("U:");
            for p in parts {
                match p {
                    UserPart::Text(t) => {
                        buf.push_str(canon_text(t));
                    }
                    UserPart::Image { url, .. } => {
                        buf.push_str("[image:");
                        buf.push_str(&hash_hex(url)[..16]);
                        buf.push(']');
                    }
                }
                buf.push('\u{1}');
            }
        }
        CanonMessage::Assistant { text, tool_calls } => {
            buf.push_str("A:");
            buf.push_str(canon_text(text));
            for c in tool_calls {
                buf.push('\u{1}');
                buf.push_str(&c.call_id);
                buf.push('\u{1}');
                buf.push_str(&c.name);
                buf.push('\u{1}');
                buf.push_str(canon_text(&c.arguments));
            }
        }
        CanonMessage::ToolResult(r) => {
            buf.push_str("T:");
            buf.push_str(&r.call_id);
            buf.push('\u{1}');
            buf.push_str(canon_text(&r.output));
        }
    }
    buf.push('\u{3}');
}

/// Key identifying "this exact conversation state" for stateless clients.
pub fn transcript_key(instructions: &str, tools_key: &str, messages: &[CanonMessage]) -> String {
    let mut buf = String::new();
    buf.push_str(canon_text(instructions));
    buf.push('\u{4}');
    buf.push_str(tools_key);
    buf.push('\u{4}');
    for m in messages {
        push_message(&mut buf, m);
    }
    hash_hex(&buf)
}

/// Render prior turns as a text block for the first user message of a fresh
/// thread (used when Codex is unpatched and cannot take `initialHistory`).
pub fn history_preamble(prefix: &[CanonMessage]) -> String {
    let mut out = String::from(
        "<conversation_history>\nThe following is the earlier part of this conversation, provided verbatim so you can continue it.\n",
    );
    for m in prefix {
        match m {
            CanonMessage::User(parts) => {
                out.push_str("\n[user]\n");
                for p in parts {
                    match p {
                        UserPart::Text(t) => out.push_str(t),
                        UserPart::Image { .. } => out.push_str("[image attachment omitted]"),
                    }
                    out.push('\n');
                }
            }
            CanonMessage::Assistant { text, tool_calls } => {
                out.push_str("\n[assistant]\n");
                if !text.is_empty() {
                    out.push_str(text);
                    out.push('\n');
                }
                for c in tool_calls {
                    out.push_str(&format!(
                        "[tool_call id={} name={}]\n{}\n",
                        c.call_id, c.name, c.arguments
                    ));
                }
            }
            CanonMessage::ToolResult(r) => {
                out.push_str(&format!(
                    "\n[tool_result id={} success={}]\n{}\n",
                    r.call_id, r.success, r.output
                ));
            }
        }
    }
    out.push_str("</conversation_history>\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::types::ToolCallRecord;
    use crate::bridge::types::ToolOutput;

    fn user(t: &str) -> CanonMessage {
        CanonMessage::User(vec![UserPart::Text(t.to_string())])
    }

    fn assistant(t: &str) -> CanonMessage {
        CanonMessage::Assistant {
            text: t.to_string(),
            tool_calls: Vec::new(),
        }
    }

    #[test]
    fn split_turn_takes_trailing_user_and_tool_messages() {
        let history = vec![
            user("hi"),
            assistant("hello"),
            CanonMessage::ToolResult(ToolOutput {
                call_id: "c1".into(),
                output: "x".into(),
                success: true,
                images: Vec::new(),
            }),
            user("more"),
        ];
        let (prefix, tail) = split_turn(&history);
        assert_eq!(prefix.len(), 2);
        assert_eq!(tail.len(), 2);
        let (prefix, tail) = split_turn(&history[..1]);
        assert!(prefix.is_empty());
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn transcript_key_is_stable_under_whitespace_and_tool_order() {
        let a = transcript_key("sys", "tk", &[user("hi "), assistant(" hello")]);
        let b = transcript_key("sys ", "tk", &[user("hi"), assistant("hello")]);
        assert_eq!(a, b);
        let c = transcript_key("sys", "tk", &[user("hi"), assistant("hello!")]);
        assert_ne!(a, c);
        let t1 = vec![
            ToolSpec::from_parts("b".into(), &serde_json::json!({}), "function"),
            ToolSpec::from_parts("a".into(), &serde_json::json!({}), "function"),
        ];
        let t2 = vec![t1[1].clone(), t1[0].clone()];
        assert_eq!(tools_key(&t1), tools_key(&t2));
    }

    #[test]
    fn assistant_tool_calls_participate_in_key() {
        let with = CanonMessage::Assistant {
            text: String::new(),
            tool_calls: vec![ToolCallRecord {
                call_id: "c1".into(),
                name: "f".into(),
                arguments: "{}".into(),
            }],
        };
        let a = transcript_key("", "", &[user("q"), with]);
        let b = transcript_key("", "", &[user("q"), assistant("")]);
        assert_ne!(a, b);
    }
}
