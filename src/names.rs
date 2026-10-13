//! Session names: chosen when creating a session, or two words derived from
//! its id ("quiet-otter"), so sessions are easier to tell apart than by UUID.

use uuid::Uuid;

const ADJECTIVES: [&str; 32] = [
    "amber", "brave", "calm", "clever", "cosmic", "crisp", "dapper", "eager", "fuzzy", "gentle",
    "golden", "happy", "jolly", "keen", "lively", "lucky", "mellow", "misty", "nimble", "noble",
    "plucky", "quiet", "rapid", "rosy", "rusty", "shiny", "snowy", "spry", "sunny", "swift",
    "tidy", "witty",
];

const NOUNS: [&str; 32] = [
    "badger", "beaver", "bison", "comet", "crane", "falcon", "ferret", "fox", "gecko", "heron",
    "ibis", "koala", "lemur", "lynx", "marten", "moose", "newt", "ocelot", "otter", "owl", "panda",
    "puffin", "quokka", "raven", "robin", "seal", "sparrow", "tapir", "toucan", "walrus", "wombat",
    "yak",
];

pub fn generate(id: Uuid) -> String {
    let b = id.as_bytes();
    format!(
        "{}-{}",
        ADJECTIVES[b[0] as usize % 32],
        NOUNS[b[1] as usize % 32]
    )
}

/// A user-given name: trimmed, at most 64 characters, None if empty.
pub fn clean(name: &str) -> Option<String> {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_stable() {
        let id = Uuid::new_v4();
        assert_eq!(generate(id), generate(id));
        assert!(generate(id).contains('-'));
    }

    #[test]
    fn clean_trims_and_rejects_empty() {
        assert_eq!(clean("  Drive upload \n").as_deref(), Some("Drive upload"));
        assert_eq!(clean("   "), None);
        assert_eq!(clean(&"x".repeat(100)).map(|n| n.len()), Some(64));
    }
}
