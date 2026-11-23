//! Emoji choices for the chat picker.

use std::collections::BTreeMap;

/// How many most-used emoji the picker shows first.
pub const TOP_COUNT: usize = 5;

/// One section of the picker.
pub struct Category {
    /// Emoji shown next to the section name.
    pub icon: &'static str,
    /// Section heading.
    pub name: &'static str,
    /// Emoji in this section.
    pub emoji: &'static [&'static str],
}

/// The picker's own copy of `emoji`, so a stored string can be offered as a `&'static str`.
fn known(emoji: &str) -> Option<&'static str> {
    CATEGORIES
        .iter()
        .flat_map(|c| c.emoji.iter().copied())
        .find(|e| *e == emoji)
}

/// The up to [`TOP_COUNT`] most used emoji, most used first. Ties keep a stable order.
pub fn top(usage: &BTreeMap<String, u32>) -> Vec<&'static str> {
    let mut counted: Vec<(&'static str, u32)> = usage
        .iter()
        .filter(|(_, count)| **count > 0)
        .filter_map(|(emoji, count)| known(emoji).map(|e| (e, *count)))
        .collect();
    counted.sort_by_key(|c| std::cmp::Reverse(c.1));
    counted
        .into_iter()
        .take(TOP_COUNT)
        .map(|(e, _)| e)
        .collect()
}

/// Most results a search or `:` completion returns.
pub const MAX_RESULTS: usize = 48;

/// Emoji whose name or `:shortcode:` matches `query`; prefix matches come first.
pub fn search(query: &str, limit: usize) -> Vec<&'static emojis::Emoji> {
    let query = query
        .trim()
        .trim_matches(':')
        .to_lowercase()
        .replace(' ', "_");
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, &'static emojis::Emoji)> = emojis::iter()
        .filter_map(|emoji| {
            let name = emoji.name().to_lowercase().replace(' ', "_");
            let mut rank = None;
            for code in emoji.shortcodes() {
                if code.starts_with(&query) {
                    rank = Some(0);
                    break;
                }
                if code.contains(&query) {
                    rank = rank.or(Some(1));
                }
            }
            if rank.is_none() {
                if name.starts_with(&query) {
                    rank = Some(2);
                } else if name.contains(&query) {
                    rank = Some(3);
                }
            }
            rank.map(|r| (r, emoji))
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// `:word` being typed at the end of `input`: the byte offset of the colon and the word.
/// The colon must start the text or follow whitespace, and the word needs two characters.
pub fn pending_query(input: &str) -> Option<(usize, &str)> {
    let colon = input.rfind(':')?;
    let word = &input[colon + 1..];
    let starts_word = input[..colon]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace);
    (starts_word && word.chars().count() >= 2 && !word.contains(char::is_whitespace))
        .then_some((colon, word))
}

/// Sections of the picker, in display order.
pub const CATEGORIES: [Category; 6] = [
    Category {
        icon: "😀",
        name: "Smileys",
        emoji: &[
            "😀", "😃", "😄", "😁", "😆", "😅", "🤣", "😂", "💩", "🙂", "🙃", "😉", "😊", "😇",
            "🥰", "😍", "🤩", "😘", "😋", "😛", "😜", "🤪", "😝", "🤗", "🤭", "🤔", "🤨", "😐",
            "😑", "😶", "🙄", "😏", "😒", "😬", "😮", "😯", "😲", "😳", "🥺", "😢", "😭", "😤",
            "😠", "😡", "🤬", "😱", "😨", "😰", "😥", "😓", "😴", "🤤", "😎", "🤓", "🧐", "🥳",
            "🥹",
        ],
    },
    Category {
        icon: "🤡",
        name: "Funny",
        emoji: &[
            "🤡", "💩", "👻", "💀", "☠️", "👽", "👾", "🤖", "🎃", "😹", "🙈", "🙉", "🙊", "🐵",
            "🦄", "🐸", "🐔", "🐧", "🦆", "🦖", "🐌", "🐙", "🦑", "🤯", "🥴", "🤢", "🤮", "🤧",
            "🥶", "🥵", "🤠", "🧌", "🫠", "🫡", "🫣", "🤌", "🧠", "🫦", "🪩", "🎭", "🎪", "🛸",
            "🗿", "🪦", "🧻", "🍌", "🥔", "🌭", "🌮", "🍕", "🍔", "🍟", "🍩", "🧀", "🥑", "🥦",
        ],
    },
    Category {
        icon: "😈",
        name: "Naughty",
        emoji: &[
            "😈", "👿", "😏", "😜", "🥵", "🤤", "😩", "🫦", "💋", "🍆", "🍑", "🍒", "🍌", "🌶️",
            "💦", "🍑", "👅", "👄", "🔞", "🖕", "🍺", "🍻", "🥃", "🍷", "🍸", "🍹", "🚬", "💊",
            "🍄", "🌿", "🔥", "😻", "🙈", "💃", "🕺", "👙", "🩲", "🛏️", "🧴", "⛓️", "🪢", "🎀",
            "🌈", "💅", "🤫", "🫣", "🥒", "🌽", "🥜", "🌭", "🍩", "🥛", "💩", "🎰", "💸", "🤑",
        ],
    },
    Category {
        icon: "👍",
        name: "Gestures",
        emoji: &[
            "👍", "👎", "👌", "🤌", "🤏", "✌️", "🤞", "🤟", "🤘", "🤙", "👈", "👉", "👆", "👇",
            "☝️", "✋", "🖐️", "🖖", "👋", "🤚", "👏", "🙌", "👐", "🤲", "🤝", "🙏", "💪", "🫶",
            "🫰", "🫵", "🫳", "🫴", "✊", "👊", "🤛", "🤜", "🖕", "🤷", "🤦", "🙋", "🙆", "🙅",
            "💁", "🙇", "🧎", "🏃", "💃", "🕺", "🧘", "🤸", "🏄", "🚶", "🫂", "👀", "👁️", "👃",
        ],
    },
    Category {
        icon: "❤️",
        name: "Hearts and symbols",
        emoji: &[
            "❤️",
            "🧡",
            "💛",
            "💚",
            "💙",
            "💜",
            "🖤",
            "🤍",
            "🤎",
            "💔",
            "❤️‍🔥",
            "💕",
            "💞",
            "💓",
            "💗",
            "💖",
            "💘",
            "💝",
            "💯",
            "💢",
            "💥",
            "💫",
            "💤",
            "💨",
            "🔥",
            "✨",
            "⭐",
            "🌟",
            "⚡",
            "🎉",
            "🎊",
            "🏆",
            "🥇",
            "🎯",
            "✅",
            "❌",
            "❗",
            "❓",
            "‼️",
            "⁉️",
            "🚫",
            "⚠️",
            "🔴",
            "🟢",
            "🔵",
            "🟡",
            "🟣",
            "⚫",
            "⚪",
            "🟥",
            "🟩",
            "🟦",
            "🆗",
            "🆒",
            "🆘",
            "🔝",
        ],
    },
    Category {
        icon: "🎮",
        name: "Things",
        emoji: &[
            "🎮", "🕹️", "🎧", "🎤", "🎸", "🎹", "🥁", "🎬", "📷", "💻", "🖥️", "⌨️", "🖱️", "📱",
            "☕", "🍵", "🥤", "🍫", "🍿", "🍪", "🎂", "🍰", "🍦", "🥂", "🎁", "🎈", "🎲", "🧩",
            "🚗", "🚀", "✈️", "🚲", "🏠", "🌍", "🌙", "☀️", "⛅", "🌧️", "❄️", "🌈", "🐶", "🐱",
            "🐭", "🐻", "🐼", "🦊", "🦁", "🐯", "🐷", "🐮", "🐴", "🦉", "🦋", "🐢", "🐍", "🦈",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_orders_by_use_and_caps_at_five() {
        let usage: BTreeMap<String, u32> = [
            ("😀", 1),
            ("💩", 9),
            ("🍆", 4),
            ("🔥", 4),
            ("👍", 2),
            ("😂", 3),
            ("🎮", 7),
            ("not an emoji", 99),
            ("😎", 0),
        ]
        .into_iter()
        .map(|(e, n)| (e.to_owned(), n))
        .collect();
        let top = top(&usage);
        assert_eq!(top.len(), TOP_COUNT);
        assert_eq!(&top[..2], ["💩", "🎮"]);
        assert!(!top.contains(&"😎"));
        assert!(super::top(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn search_finds_by_shortcode_and_name() {
        let hits = search("poo", 10);
        assert!(hits.iter().any(|e| e.as_str() == "💩"));
        assert!(search("thumbs up", 10).iter().any(|e| e.as_str() == "👍"));
        assert!(search("", 10).is_empty());
        assert!(search("zzzzzzzz", 10).is_empty());
    }

    #[test]
    fn pending_query_needs_colon_at_word_start() {
        assert_eq!(pending_query("hi :smi"), Some((3, "smi")));
        assert_eq!(pending_query(":sm"), Some((0, "sm")));
        assert_eq!(pending_query("hi :s"), None);
        assert_eq!(pending_query("http://x"), None);
        assert_eq!(pending_query("hi :smile now"), None);
        assert_eq!(pending_query("hi"), None);
    }

    #[test]
    fn every_section_has_emoji_and_none_is_blank() {
        for category in &CATEGORIES {
            assert!(!category.emoji.is_empty(), "{}", category.name);
            assert!(category.emoji.iter().all(|e| !e.trim().is_empty()));
        }
    }

    #[test]
    fn emoji_pass_chat_validation() {
        for category in &CATEGORIES {
            for emoji in category.emoji {
                assert!(chattercrab_protocol::validate_chat_text(emoji).is_ok());
            }
        }
    }
}
