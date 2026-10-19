const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;
/// Hashed between consecutive parts. It never occurs in UTF-8, so text parts cannot alias.
const PART_SEPARATOR: u8 = 0xff;

/// Stable 64-bit FNV-1a (used for HunkId and content hashes; must not change between releases).
///
/// The parts are hashed in order with a `0xff` byte between consecutive parts, so
/// `["ab", "c"]` and `["a", "bc"]` hash differently. A single part hashes exactly like plain
/// FNV-1a 64 of its bytes, and no parts at all hash to the offset basis.
pub fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let step = |h: u64, b: u8| (h ^ u64::from(b)).wrapping_mul(PRIME);
    let mut h = OFFSET_BASIS;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h = step(h, PART_SEPARATOR);
        }
        h = part.iter().fold(h, |h, &b| step(h, b));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(fnv1a64(&[b""]), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(&[]), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(&[b"a"]), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(&[b"foobar"]), 0x85944171f73967e8);
    }

    #[test]
    fn parts_are_separated() {
        assert_eq!(fnv1a64(&[b"ab", b"c"]), 0xfdf3a083ef9bc024);
        assert_eq!(fnv1a64(&[b"a", b"bc"]), 0xd2b3f5819298d9d6);
        assert_ne!(fnv1a64(&[b"abc"]), fnv1a64(&[b"ab", b"c"]));
        assert_ne!(fnv1a64(&[b"x", b""]), fnv1a64(&[b"", b"x"]));
    }
}
