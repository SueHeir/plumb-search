//! SimHash fingerprints of a homepage's text, for telling near-copies
//! apart from sites that only share a few words ("Detecting Near-Duplicates
//! for Web Crawling", Manku, Jain and Das Sarma, Google, WWW 2007).
//!
//! Each run of [`SHINGLE_WORDS`] words is hashed to 64 bits, and bit `i` of
//! the fingerprint is set when more of the runs have bit `i` set than not.
//! Texts that share most of their runs get fingerprints that differ in few
//! bits; Google's crawl took 3 bits of 64 as "the same page".

/// Words in each run hashed.
pub const SHINGLE_WORDS: usize = 2;

/// Fewest words a text needs for a fingerprint: shorter texts ("Welcome",
/// a one-line splash) are alike by chance.
pub const MIN_FINGERPRINT_WORDS: usize = 24;

/// Most differing bits between the fingerprints of two near-copies: about
/// three words changed in fifty.
pub const NEAR_COPY_BITS: u32 = 6;

/// The fingerprint of `text`, or `None` when it has fewer than
/// [`MIN_FINGERPRINT_WORDS`] words. Words are compared lowercased, without
/// punctuation.
pub fn fingerprint(text: &str) -> Option<u64> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.len() < MIN_FINGERPRINT_WORDS {
        return None;
    }
    let mut votes = [0i32; 64];
    for word in &words {
        let hash = hash64(word.as_bytes());
        for (bit, vote) in votes.iter_mut().enumerate() {
            *vote += if hash >> bit & 1 == 1 { 1 } else { -1 };
        }
    }
    Some(
        votes
            .iter()
            .enumerate()
            .filter(|(_, vote)| **vote > 0)
            .fold(0, |print, (bit, _)| print | 1 << bit),
    )
}

/// Whether two fingerprints are of near-copies: at most [`NEAR_COPY_BITS`]
/// bits apart.
pub fn near_copies(a: u64, b: u64) -> bool {
    (a ^ b).count_ones() <= NEAR_COPY_BITS
}

/// FNV-1a, then the splitmix64 finalizer, so every bit is well mixed.
fn hash64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOP: &str = "Welcome to our store. We sell handmade leather shoes, boots and \
        sandals, cut and stitched by hand in our workshop since 1952. Free shipping on \
        orders over fifty dollars, and free returns within thirty days of delivery. \
        Sign up for our newsletter to hear about new styles first.";

    #[test]
    fn a_copy_with_a_few_words_changed_is_a_near_copy() {
        let copy = SHOP
            .replace("1952", "1987")
            .replace("fifty", "sixty")
            .replace("Welcome", "WELCOME!")
            .replace("store", "STORE");
        let a = fingerprint(SHOP).unwrap();
        assert_eq!(
            fingerprint(&copy.to_uppercase()),
            Some(fingerprint(&copy).unwrap())
        );
        assert!(near_copies(a, fingerprint(&copy).unwrap()));
        assert!(near_copies(a, a));
    }

    #[test]
    fn different_texts_are_not() {
        let other = "The city council meets on the first Tuesday of every month in the \
            town hall. Agendas and minutes of past meetings are posted here, along with \
            notices of public hearings, road closures and the schedule for collecting \
            recycling and yard waste.";
        let a = fingerprint(SHOP).unwrap();
        let b = fingerprint(other).unwrap();
        assert!(!near_copies(a, b));
        // Half the page shared is still not a copy.
        let half: String = SHOP.split(". ").take(2).collect::<Vec<_>>().join(". ");
        let mixed = format!("{half}. {other}");
        assert!(!near_copies(a, fingerprint(&mixed).unwrap()));
    }

    #[test]
    fn short_texts_have_none() {
        assert_eq!(fingerprint("Welcome to our store"), None);
        assert_eq!(fingerprint(""), None);
    }
}
