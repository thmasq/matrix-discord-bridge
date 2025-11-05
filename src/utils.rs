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
}
