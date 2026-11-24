//! Search text normalization and query analysis shared by FTS indexing and lookup.

use std::collections::HashSet;

const MAX_TERMS: usize = 32;
const FTS_STOP_WORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "is", "are", "was", "were", "be", "been", "being", "do", "does",
    "did", "have", "has", "had", "can", "could", "should", "would", "what", "which", "who", "how",
    "when", "where", "why", "to", "of", "for", "in", "on", "at", "with", "from", "by", "as", "it",
    "this", "that", "we", "you", "they", "their", "our", "your", "its",
];

pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| match character {
            '\u{3000}' => ' ',
            '\u{ff01}'..='\u{ff5e}' => {
                char::from_u32(character as u32 - 0xfee0).unwrap_or(character)
            }
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn terms(input: &str) -> Vec<String> {
    terms_with_limit(input, MAX_TERMS)
}

fn terms_with_limit(input: &str, limit: usize) -> Vec<String> {
    let normalized = normalize(input);
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    let mut word = String::new();
    let mut cjk = Vec::new();
    let flush_word = |word: &mut String, terms: &mut Vec<String>, seen: &mut HashSet<String>| {
        if !word.is_empty() {
            if seen.insert(word.clone()) {
                terms.push(std::mem::take(word));
            } else {
                word.clear();
            }
        }
    };
    let flush_cjk = |cjk: &mut Vec<char>, terms: &mut Vec<String>, seen: &mut HashSet<String>| {
        if cjk.len() == 1 {
            let term = cjk.iter().collect::<String>();
            if seen.insert(term.clone()) {
                terms.push(term);
            }
        } else {
            for pair in cjk.windows(2) {
                let term = pair.iter().collect::<String>();
                if seen.insert(term.clone()) {
                    terms.push(term);
                }
            }
        }
        cjk.clear();
    };
    for character in normalized.chars() {
        if is_cjk(character) {
            flush_word(&mut word, &mut terms, &mut seen);
            cjk.push(character);
        } else if character.is_alphanumeric() || character == '_' {
            flush_cjk(&mut cjk, &mut terms, &mut seen);
            word.push(character);
        } else {
            flush_word(&mut word, &mut terms, &mut seen);
            flush_cjk(&mut cjk, &mut terms, &mut seen);
        }
    }
    flush_word(&mut word, &mut terms, &mut seen);
    flush_cjk(&mut cjk, &mut terms, &mut seen);
    terms.truncate(limit);
    terms
}

/// A bounded, noise-filtered query vocabulary for structured LIKE searches. Structured tables
/// do not use the FTS index, so keeping only the most informative three terms bounds SQL work.
pub fn recall_terms(input: &str) -> Vec<String> {
    recall_terms_with_limit(input, 3)
}

pub fn structured_term_limit() -> usize {
    std::env::var("PI_MEMORY_STRUCTURED_TERM_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2)
        .min(3)
}

pub fn structured_recall_terms(input: &str) -> Vec<String> {
    recall_terms_with_limit(input, structured_term_limit())
}

fn recall_terms_with_limit(input: &str, limit: usize) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "如何", "怎么", "怎样", "什么", "哪些", "避免", "多个", "which", "what", "how", "does",
        "the", "and", "for", "with", "about",
    ];
    // FTS only needs the first bounded vocabulary. Structured recall also samples the
    // prompt tail because long prompts often end with the preference or constraint to retain.
    let normalized = normalize(input);
    let mut candidates = terms(input);
    let tail = normalized
        .chars()
        .rev()
        .take(512)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    for term in terms_with_limit(&tail, 512) {
        if !candidates.contains(&term) {
            candidates.push(term);
        }
    }
    let mut terms = candidates
        .into_iter()
        .enumerate()
        .filter(|(_, term)| !STOP_WORDS.contains(&term.as_str()))
        .collect::<Vec<_>>();
    terms.sort_by(|(left_position, left), (right_position, right)| {
        right
            .chars()
            .count()
            .cmp(&left.chars().count())
            // CJK terms are mostly equal-length bigrams. Prefer terms nearer the end
            // of the prompt, where users commonly put the actual constraint or request.
            .then_with(|| right_position.cmp(left_position))
    });
    terms.truncate(limit.clamp(1, 3));
    terms.into_iter().map(|(_, term)| term).collect()
}

const RECALL_ACKNOWLEDGEMENTS: &[&str] = &[
    "好",
    "好的",
    "嗯",
    "嗯嗯",
    "收到",
    "明白",
    "继续",
    "继续吧",
    "好的继续",
    "请继续",
    "再继续",
    "继续一下",
    "接着",
    "往下",
    "谢谢",
    "可以",
    "行",
    "ok",
    "okay",
    "yes",
    "yep",
    "sure",
    "continue",
    "next",
    "please",
    "thanks",
];

pub fn has_informative_recall_query(input: &str) -> bool {
    normalize(input)
        .split(|character: char| !character.is_alphanumeric())
        .any(|part| {
            !RECALL_ACKNOWLEDGEMENTS.contains(&part) && part.chars().any(char::is_alphabetic)
        })
}

pub fn build_search_text(input: &str) -> String {
    terms(input).join(" ")
}

pub fn to_fts_query(input: &str) -> String {
    terms(input)
        .iter()
        .filter(|term| !FTS_STOP_WORDS.contains(&term.as_str()))
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff |
        0x3040..=0x30ff | 0xac00..=0xd7af)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_skips_english_stop_words() {
        assert_eq!(
            to_fts_query("How can the local service work?"),
            "\"local\" OR \"service\" OR \"work\""
        );
        assert!(to_fts_query("How can it be?").is_empty());
    }

    #[test]
    fn fts_query_keeps_chinese_bigrams() {
        assert_eq!(to_fts_query("向量检索"), "\"向量\" OR \"量检\" OR \"检索\"");
    }

    #[test]
    fn structured_term_limit_is_bounded_and_applied_after_ranking() {
        let prompt = "怎样避免多个智能体同时修改代码冲突，回复使用中文";
        let default_terms = structured_recall_terms(prompt);
        assert_eq!(default_terms.len(), 2);
        assert_eq!(
            recall_terms_with_limit(prompt, 2),
            default_terms[..2].to_vec()
        );
        assert_eq!(recall_terms_with_limit(prompt, 99).len(), 3);
    }
    #[test]
    fn recall_query_gate_rejects_acknowledgements_and_numeric_replies() {
        for prompt in [
            "继续",
            "继续吧。",
            "好的，继续",
            "好的继续",
            "请继续",
            "好的",
            "1",
            "ok",
            "yes",
            "continue",
            "please continue",
        ] {
            assert!(!has_informative_recall_query(prompt), "{prompt}");
        }
    }

    #[test]
    fn recall_query_gate_keeps_searchable_topics() {
        for prompt in [
            "Rust",
            "Go",
            "继续修复 Go",
            "帮我修复 recall 注入",
            "今天天气不错",
        ] {
            assert!(has_informative_recall_query(prompt), "{prompt}");
        }
    }
}
