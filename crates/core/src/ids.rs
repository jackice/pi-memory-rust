use sha2::{Digest, Sha256};
use unicode_categories::UnicodeCategories;

pub fn stable_id(namespace: &str, kind: &str, parts: &[&str]) -> String {
    let mut input = Vec::new();
    encode(&mut input, namespace);
    encode(&mut input, kind);
    for part in parts {
        let normalized = normalize(part);
        encode(&mut input, &normalized);
    }
    let digest = Sha256::digest(input);
    let hash = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{namespace}:{kind}:{hash}")
}

fn encode(input: &mut Vec<u8>, value: &str) {
    input.extend_from_slice(&(value.len() as u64).to_be_bytes());
    input.extend_from_slice(value.as_bytes());
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_matches(is_boundary_punctuation)
        .to_string()
}

fn is_boundary_punctuation(character: char) -> bool {
    character != '#' && character.is_punctuation()
}

#[cfg(test)]
mod tests {
    use super::stable_id;

    #[test]
    fn punctuation_in_technical_terms_does_not_collapse_ids() {
        let cpp = stable_id("t", "fact", &["C++"]);
        let c = stable_id("t", "fact", &["C"]);
        let csharp = stable_id("t", "fact", &["C#"]);
        assert_ne!(cpp, c);
        assert_ne!(cpp, csharp);
        assert_ne!(c, csharp);
        assert_ne!(
            stable_id("t", "fact", &["$HOME"]),
            stable_id("t", "fact", &["HOME"])
        );
    }

    #[test]
    fn normalization_is_stable_for_case_whitespace_and_cjk() {
        assert_eq!(
            stable_id("t", "fact", &["  Rust\t language  "]),
            stable_id("t", "fact", &["rust language"]),
        );
        assert_eq!(
            stable_id("t", "fact", &[" 中文 记忆 "]),
            stable_id("t", "fact", &["中文 记忆"]),
        );
        assert_eq!(
            stable_id("t", "fact", &["Rust."]),
            stable_id("t", "fact", &["Rust"])
        );
    }

    #[test]
    fn normalization_trims_common_ascii_and_unicode_boundary_punctuation() {
        assert_eq!(
            stable_id("t", "fact", &["  \"(Rust.)\"  "]),
            stable_id("t", "fact", &["Rust"]),
        );
        assert_eq!(
            stable_id("t", "fact", &["—中文，"]),
            stable_id("t", "fact", &["中文"]),
        );
        assert_ne!(
            stable_id("t", "fact", &["don't"]),
            stable_id("t", "fact", &["dont"]),
            "punctuation inside content remains significant"
        );
    }

    #[test]
    fn parts_are_unambiguously_encoded() {
        assert_ne!(
            stable_id("t", "fact", &["ab\0c", "d"]),
            stable_id("t", "fact", &["ab", "c\0d"]),
        );
    }

    #[test]
    fn namespace_and_kind_are_part_of_the_hash_input() {
        assert_ne!(
            stable_id("one", "fact", &["same"]),
            stable_id("two", "fact", &["same"]),
        );
        assert_ne!(
            stable_id("one", "fact", &["same"]),
            stable_id("one", "timeline", &["same"]),
        );
    }

    #[test]
    fn hash_uses_128_bits_and_is_repeatable() {
        let first = stable_id("t", "fact", &["Rust"]);
        assert_eq!(first, stable_id("t", "fact", &["Rust"]));
        assert_eq!(first.rsplit(':').next().unwrap().len(), 32);
    }
}
