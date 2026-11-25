//! Context budgeting: keeps a session's history within the model's window.

use arustex_llm::{Message, Role, estimate_tokens};

/// Approximate token size of a history plus fixed overhead.
pub fn history_tokens(system: &str, tools_chars: usize, history: &[Message]) -> u64 {
    estimate_tokens(
        system.len() + tools_chars + history.iter().map(Message::approx_chars).sum::<usize>(),
    )
}

/// Drops the oldest complete turns (an assistant message and the tool results that
/// follow it) until the history fits `budget_tokens`. The first message (the task
/// brief) and the most recent `keep_recent` turns are always kept, so tool calls are
/// never separated from their results. Returns the number of dropped messages.
pub fn compact(
    history: &mut Vec<Message>,
    fixed_tokens: u64,
    budget_tokens: u64,
    keep_recent: usize,
) -> usize {
    let size =
        |h: &[Message]| fixed_tokens + estimate_tokens(h.iter().map(Message::approx_chars).sum());
    let mut dropped = 0;
    while size(history) > budget_tokens {
        let turn_starts: Vec<usize> = history
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, m)| m.role == Role::Assistant)
            .map(|(i, _)| i)
            .collect();
        if turn_starts.len() <= keep_recent {
            break;
        }
        let start = turn_starts[0];
        let end = turn_starts[1];
        history.drain(start..end);
        dropped += end - start;
    }
    if dropped > 0 {
        mark_compacted(history, dropped);
    }
    dropped
}

const NOTE_PREFIX: &str = "[Context note: ";

fn mark_compacted(history: &mut [Message], dropped: usize) {
    let Some(first) = history.first_mut() else {
        return;
    };
    let previous = first
        .content
        .iter()
        .find_map(|p| match p {
            arustex_llm::ContentPart::Text { text } if text.starts_with(NOTE_PREFIX) => text
                [NOTE_PREFIX.len()..]
                .split(' ')
                .next()
                .and_then(|n| n.parse::<usize>().ok()),
            _ => None,
        })
        .unwrap_or(0);
    first.content.retain(
        |p| !matches!(p, arustex_llm::ContentPart::Text { text } if text.starts_with(NOTE_PREFIX)),
    );
    first.content.push(arustex_llm::ContentPart::Text {
        text: format!(
            "{NOTE_PREFIX}{} earlier messages were removed to fit the context window. Re-read records if needed.]",
            previous + dropped
        ),
    });
}

#[cfg(test)]
mod tests {
    use arustex_llm::ContentPart;
    use serde_json::json;

    use super::*;

    fn turn(i: usize) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: format!("c{i}"),
                    name: "read_records".into(),
                    input: json!({}),
                }],
            },
            Message::tool_result(format!("c{i}"), "x".repeat(4000), false),
        ]
    }

    #[test]
    fn compaction_drops_whole_old_turns_and_keeps_brief() {
        let mut h = vec![Message::user("brief")];
        for i in 0..10 {
            h.extend(turn(i));
        }
        let dropped = compact(&mut h, 0, 3000, 2);
        assert!(dropped > 0 && dropped.is_multiple_of(2));
        assert!(h[0].text().starts_with("brief"));
        assert!(h[0].text().contains(&format!("{dropped} earlier messages")));
        // Every remaining tool result follows its call.
        for w in h[1..].chunks(2) {
            assert_eq!(w[0].role, Role::Assistant);
            assert_eq!(w[1].role, Role::Tool);
        }
        assert!(h.len() >= 5);
        let again = compact(&mut h, 0, 10, 2);
        assert_eq!(h.len(), 5);
        assert!(
            h[0].text()
                .contains(&format!("{} earlier messages", dropped + again))
        );
    }
}
