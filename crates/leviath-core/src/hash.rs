//! Hashes whose value outlives the build that computed it.
//!
//! `std`'s `DefaultHasher` leaves its algorithm unspecified and free to change
//! between Rust releases. That is right for a `HashMap` and wrong for a value
//! that is stored, sent or compared across builds: a pipe name two binaries
//! must agree on, a cache key a provider remembers, a fingerprint written into
//! a run file. Those hash with [`stable_hasher`].

use std::hash::Hasher;

/// A hasher whose output depends only on what is fed to it.
///
/// SipHash-1-3 with zero keys, which is what `DefaultHasher::new()` computes
/// today, so a value an earlier build wrote still matches.
pub fn stable_hasher() -> impl Hasher {
    siphasher::sip::SipHasher13::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn hash<T: Hash + ?Sized>(value: &T) -> u64 {
        let mut hasher = stable_hasher();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// Pinned to what `DefaultHasher::new()` produced for the same input, so
    /// a pipe name, cache key or fingerprint written before keeps its value.
    #[test]
    fn known_answers_match_what_default_hasher_produced() {
        assert_eq!(stable_hasher().finish(), 0xd1fb_a762_150c_532c);
        assert_eq!(hash("leviath"), 0x1031_d28d_04d7_cd6c);
        assert_eq!(hash(&42u64), 0x7b3e_724b_36eb_df51);
        assert_eq!(hash(&("a", 1usize, vec![1u64, 2])), 0xdce1_e074_ead7_9a0e);
        assert_eq!(
            hash(std::path::Path::new("/home/user/.leviath")),
            0x2ded_9b90_4166_bf00
        );
    }
}
