pub fn hash_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::hash_hex;

    #[test]
    fn matches_fnv1a_64_vectors() {
        assert_eq!(hash_hex(b""), "cbf29ce484222325");
        assert_eq!(hash_hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(hash_hex(b"foobar"), "85944171f73967e8");
    }
}
