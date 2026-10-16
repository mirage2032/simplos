//! Random bytes for programs: what `getrandom`, /dev/random and /dev/urandom
//! give them, and the 16 behind each new program's `AT_RANDOM`, which seed
//! its stack canaries and hash maps.
//!
//! shortcut: RDRAND when the CPU has it, which QEMU's default CPU hasn't;
//! else the timestamp counter, stirred so that no two words come out the
//! same, which is guessable. Take entropy from somewhere better once there
//! is somewhere.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::random::RdRand;

/// Moved on by every word stirred, so no two are.
static STATE: AtomicU64 = AtomicU64::new(0);

/// Fill `bytes` with random ones.
pub fn fill(bytes: &mut [u8]) {
    let rdrand = RdRand::new();
    for chunk in bytes.chunks_mut(8) {
        let word = rdrand.and_then(|rdrand| rdrand.get_u64()).unwrap_or_else(stirred);
        chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
    }
}

/// The timestamp counter, mixed into the state's next value by SplitMix64's
/// finalizer.
fn stirred() -> u64 {
    const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
    // Safety: rdtsc reads a counter, nothing more.
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let mut z = STATE.fetch_add(GOLDEN, Ordering::Relaxed).wrapping_add(GOLDEN) ^ tsc;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test_case]
fn no_two_words_are_the_same() {
    let mut words = [0u8; 64];
    fill(&mut words);
    let words: alloc::vec::Vec<&[u8]> = words.chunks(8).collect();
    for (at, word) in words.iter().enumerate() {
        assert!(!words[..at].contains(word), "a word came out twice: {word:?}");
    }
    // An odd length is filled to its end.
    let mut odd = [0u8; 13];
    fill(&mut odd);
    assert!(odd[8..].iter().any(|&byte| byte != 0));
}
