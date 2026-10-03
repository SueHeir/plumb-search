//! Which sites a node may crawl for the network, and when.
//!
//! Time is cut into epochs of a day. In each epoch every node is assigned a
//! random-looking share of all sites, picked by hashing the epoch, the
//! node's id and the domain. The assignment changes every day, nobody
//! chooses their own sites, and any node can check, from a batch header
//! alone, that a crawler only sent results for sites it was assigned. With
//! [`MAX_SHARE_PPM`] at one eighth, a key can write at most an eighth of the
//! sites a day, and a site is crawled by about one in eight nodes a day, so
//! with dozens of nodes most sites have several crawlers to compare.
//!
//! The epoch is public and known in advance, so someone who wants one
//! particular site can make keys until one is assigned to it. Assignment
//! alone therefore spreads the work and caps how much one key can write; it
//! does not stop a targeted attack. That needs cross-checks (several
//! crawlers per site, spot-check re-fetches) and, later, an unpredictable
//! epoch seed from a public randomness beacon. See `docs/network.md`.

use libp2p::PeerId;

use crate::hash::Hash;

/// Seconds in an epoch.
pub const EPOCH_SECS: u64 = 24 * 60 * 60;

/// The largest share of all sites a node may be assigned in an epoch, in
/// parts per million.
pub const MAX_SHARE_PPM: u32 = 125_000;

/// The epoch that Unix time `unix_secs` falls in.
pub fn epoch_of(unix_secs: u64) -> u64 {
    unix_secs / EPOCH_SECS
}

/// Whether `peer` may crawl `domain` in `epoch`, given its share of all
/// sites in parts per million (capped at [`MAX_SHARE_PPM`]).
pub fn is_assigned(epoch: u64, peer: &PeerId, domain: &str, share_ppm: u32) -> bool {
    let share_ppm = share_ppm.min(MAX_SHARE_PPM);
    let hash = Hash::of(&[
        b"plumb-assign-v1\0",
        &epoch.to_be_bytes(),
        &peer.to_bytes(),
        b"\0",
        domain.as_bytes(),
    ]);
    // hash < share * 2^64, scaled to avoid floating point.
    let threshold = (u128::from(share_ppm) << 64) / 1_000_000;
    u128::from(hash.prefix_u64()) < threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_the_share_of_sites_is_assigned_and_it_changes_every_epoch() {
        let peer = PeerId::random();
        let domains: Vec<String> = (0..20_000).map(|i| format!("site{i}.com")).collect();
        let assigned = |epoch| {
            domains
                .iter()
                .filter(|d| is_assigned(epoch, &peer, d, MAX_SHARE_PPM))
                .cloned()
                .collect::<Vec<_>>()
        };
        let today = assigned(20_000);
        let tomorrow = assigned(20_001);
        // An eighth of 20,000 is 2,500.
        assert!((2_200..2_800).contains(&today.len()), "{}", today.len());
        let overlap = today.iter().filter(|d| tomorrow.contains(d)).count();
        // Independent picks share about an eighth.
        assert!(overlap < 500, "{overlap}");
    }

    #[test]
    fn shares_above_the_cap_are_capped_and_zero_assigns_nothing() {
        let peer = PeerId::random();
        let count = |ppm| {
            (0..10_000)
                .filter(|i| is_assigned(7, &peer, &format!("d{i}.org"), ppm))
                .count()
        };
        assert_eq!(count(1_000_000), count(MAX_SHARE_PPM));
        assert_eq!(count(0), 0);
    }

    #[test]
    fn different_nodes_get_different_sites() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let both = (0..10_000)
            .map(|i| format!("d{i}.net"))
            .filter(|d| {
                is_assigned(9, &a, d, MAX_SHARE_PPM) && is_assigned(9, &b, d, MAX_SHARE_PPM)
            })
            .count();
        // About 1/64 of the sites.
        assert!(both < 300, "{both}");
    }
}
