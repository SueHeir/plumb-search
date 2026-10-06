//! Where to get help now, for searches by someone who may be in crisis:
//! "depression help", "i want to die", "suicide hotline". The lines are
//! free and confidential; the answer comes first, above every result.

use crate::{simplify, Answer, Kind};

/// Words that alone ask for crisis help.
const CRISIS_WORDS: &[&str] = &["suicide", "suicidal", "selfharm", "self-harm", "988"];

/// Phrases that ask for crisis help wherever they appear.
const CRISIS_PHRASES: &[&str] = &[
    "kill myself",
    "killing myself",
    "end my life",
    "want to die",
    "self harm",
    "hurt myself",
    "hurting myself",
    "crisis line",
    "crisis hotline",
    "crisis text line",
    "depression help",
    "help with depression",
    "help for depression",
    "depressed and need help",
    "feeling hopeless",
    "mental health crisis",
    "mental health hotline",
    "mental health help",
];

/// The answer for `query` when it asks for crisis help.
pub(crate) fn answer(query: &str) -> Option<Answer> {
    let query = simplify(query);
    let padded = format!(" {query} ");
    let asks = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .any(|word| CRISIS_WORDS.contains(&word))
        || CRISIS_PHRASES
            .iter()
            .any(|phrase| padded.contains(&format!(" {phrase} ")));
    if !asks {
        return None;
    }
    Some(Answer {
        kind: Kind::Help,
        question: "Help is available, free and confidential".to_string(),
        answer: "Call or text 988 in the US and Canada. Call 116 123 in the UK and Ireland."
            .to_string(),
        note: Some(
            "Elsewhere, findahelpline.com lists free lines by country. If you are in danger \
             now, call your local emergency number."
                .to_string(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crisis_searches_get_the_lines() {
        for query in [
            "depression help",
            "suicide hotline",
            "I want to die",
            "how to stop self harm",
            "Suicidal thoughts?",
        ] {
            let found = answer(query).unwrap_or_else(|| panic!("{query}"));
            assert!(found.answer.contains("988"), "{query}");
        }
        for query in ["depression", "die hard", "help desk"] {
            assert_eq!(answer(query), None, "{query}");
        }
        // A title with the word still gets the lines: better shown once
        // too often than missed.
        assert!(answer("suicide squad cast").is_some());
    }
}
