pub const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// Hash a string using a simple hash algorithm (DJB2)
/// This is used to create unique identifiers for webhook users
pub fn hash_string(s: &str) -> u32 {
    let mut hash: u32 = 5381;
    for ch in s.chars() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_add(hash)
            .wrapping_add(ch as u32);
    }
    hash
}

/// Truncate a message to Discord's character limit
pub fn truncate_message(msg: &str, limit: usize) -> String {
    if msg.len() <= limit {
        msg.to_string()
    } else {
        let mut truncated = msg.chars().take(limit - 1).collect::<String>();
        truncated.push('…');
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_string() {
        let hash1 = hash_string("test");
        let hash2 = hash_string("test");
        let hash3 = hash_string("different");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_truncate_message() {
        let short = "Hello";
        assert_eq!(truncate_message(short, 10), "Hello");

        let long = "Hello world this is a very long message";
        let truncated = truncate_message(long, 10);
        assert_eq!(truncated.len(), 10);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn test_truncate_unicode() {
        let emoji = "Hello 👋🏼 World";
        let truncated = truncate_message(emoji, 10);
        assert!(truncated.len() <= 10);
    }
}
