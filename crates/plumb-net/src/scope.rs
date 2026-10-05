//! Which nodes a network search asks: only the nodes this node trusts,
//! those and the nodes they trust ("friends of friends", the default), or
//! any node (Liz, 2026-10-05).
//!
//! A node learns what its trusted nodes trust by asking each of them on
//! `/plumb/trust/1` when it connects. The lists are kept in
//! `DIR/friends.json`, so a restarted node searches the same circle before
//! its trusted nodes are back. Only one hop: a friend of a friend's
//! friends are not asked.
//!
//! The scope applies to every bucket request this node sends for a search:
//! its own searches, its background rounds and the sealed requests it
//! relays for private search in the browser. It does not change whose
//! crawls are taken in (see `crate::agree`).

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

/// Most node ids a node hands out on `/plumb/trust/1`, and keeps from one
/// trusted node.
pub const MAX_SHARED_TRUST: usize = 256;

/// Which nodes a network search asks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// Only the nodes this node trusts.
    Trusted,
    /// The nodes this node trusts and the nodes they trust.
    #[default]
    FriendsOfFriends,
    /// Any node in the network.
    Anyone,
}

impl SearchScope {
    pub const ALL: [SearchScope; 3] = [
        SearchScope::Trusted,
        SearchScope::FriendsOfFriends,
        SearchScope::Anyone,
    ];

    /// The value in forms, flags and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchScope::Trusted => "trusted",
            SearchScope::FriendsOfFriends => "friends_of_friends",
            SearchScope::Anyone => "anyone",
        }
    }

    /// A few words for a settings page.
    pub fn label(self) -> &'static str {
        match self {
            SearchScope::Trusted => "Trusted nodes only",
            SearchScope::FriendsOfFriends => "Friends of friends",
            SearchScope::Anyone => "Anyone",
        }
    }

    /// Which nodes are asked, in a sentence.
    pub fn explain(self) -> &'static str {
        match self {
            SearchScope::Trusted => "Network searches ask only the nodes this node trusts.",
            SearchScope::FriendsOfFriends => {
                "Network searches ask the nodes this node trusts and the nodes they trust."
            }
            SearchScope::Anyone => "Network searches ask any Plumb node.",
        }
    }
}

impl fmt::Display for SearchScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SearchScope {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "trusted" | "trusted_only" => Ok(SearchScope::Trusted),
            "friends" | "friends_of_friends" | "fof" => Ok(SearchScope::FriendsOfFriends),
            "anyone" | "any" | "all" => Ok(SearchScope::Anyone),
            _ => anyhow::bail!(
                "unknown search scope {text:?}: use trusted, friends-of-friends or anyone"
            ),
        }
    }
}

/// What each trusted node said it trusts.
#[derive(Debug, Default)]
pub struct Friends {
    path: Option<PathBuf>,
    lists: BTreeMap<PeerId, Vec<PeerId>>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    lists: BTreeMap<String, Vec<String>>,
}

impl Friends {
    /// The lists kept in `path`, none when it is missing or unreadable.
    pub fn open(path: &Path) -> Friends {
        let lists = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .map(|saved| {
                saved
                    .lists
                    .into_iter()
                    .filter_map(|(peer, list)| {
                        let peer = peer.parse().ok()?;
                        Some((peer, parse_ids(list.iter().map(String::as_str))))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Friends {
            path: Some(path.to_path_buf()),
            lists,
        }
    }

    /// Keeps what `peer` trusts, at most [`MAX_SHARED_TRUST`] of `list`.
    /// Returns whether it changed.
    pub fn set(&mut self, peer: PeerId, list: &[String]) -> bool {
        let list = parse_ids(list.iter().map(String::as_str));
        if self.lists.get(&peer) == Some(&list) {
            return false;
        }
        self.lists.insert(peer, list);
        true
    }

    /// Writes the lists of the nodes still in `trusted` to the file, and
    /// forgets the others.
    pub fn save(&mut self, trusted: &[PeerId]) -> Result<()> {
        self.lists.retain(|peer, _| trusted.contains(peer));
        let Some(path) = &self.path else {
            return Ok(());
        };
        let saved = Saved {
            lists: self
                .lists
                .iter()
                .map(|(peer, list)| {
                    (
                        peer.to_string(),
                        list.iter().map(ToString::to_string).collect(),
                    )
                })
                .collect(),
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)
            .and_then(|()| std::fs::rename(&tmp, path))
            .with_context(|| format!("writing {}", path.display()))
    }

    /// The nodes trusted by the nodes in `trusted`, besides `me` and
    /// `trusted` themselves.
    pub fn of(&self, me: &PeerId, trusted: &[PeerId]) -> HashSet<PeerId> {
        trusted
            .iter()
            .filter_map(|peer| self.lists.get(peer))
            .flatten()
            .filter(|peer| *peer != me && !trusted.contains(peer))
            .copied()
            .collect()
    }

    /// Whether a search under `scope` may ask `peer`.
    pub fn allows(
        &self,
        scope: SearchScope,
        me: &PeerId,
        trusted: &[PeerId],
        peer: &PeerId,
    ) -> bool {
        match scope {
            SearchScope::Anyone => true,
            SearchScope::Trusted => trusted.contains(peer),
            SearchScope::FriendsOfFriends => {
                trusted.contains(peer)
                    || trusted
                        .iter()
                        .filter_map(|t| self.lists.get(t))
                        .any(|list| list.contains(peer) && peer != me)
            }
        }
    }
}

fn parse_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<PeerId> {
    let mut out: Vec<PeerId> = Vec::new();
    for id in ids {
        if out.len() == MAX_SHARED_TRUST {
            break;
        }
        if let Ok(peer) = id.parse() {
            if !out.contains(&peer) {
                out.push(peer);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerId {
        PeerId::random()
    }

    #[test]
    fn scopes_read_from_flags_and_forms() {
        for scope in SearchScope::ALL {
            assert_eq!(scope.as_str().parse::<SearchScope>().unwrap(), scope);
        }
        assert_eq!(
            "friends-of-friends".parse::<SearchScope>().unwrap(),
            SearchScope::FriendsOfFriends
        );
        assert_eq!(
            "Friends".parse::<SearchScope>().unwrap(),
            SearchScope::FriendsOfFriends
        );
        assert!("strangers".parse::<SearchScope>().is_err());
        assert_eq!(SearchScope::default(), SearchScope::FriendsOfFriends);
        assert_eq!(
            serde_json::to_string(&SearchScope::FriendsOfFriends).unwrap(),
            "\"friends_of_friends\""
        );
    }

    #[test]
    fn friends_of_friends_is_one_hop() {
        let (me, friend, friend_of_friend, further, stranger) =
            (peer(), peer(), peer(), peer(), peer());
        let mut friends = Friends::default();
        assert!(friends.set(
            friend,
            &[friend_of_friend.to_string(), me.to_string(), "junk".into()]
        ));
        assert!(!friends.set(friend, &[friend_of_friend.to_string(), me.to_string()]));
        // A node this one does not trust: what it trusts does not count.
        friends.set(stranger, &[further.to_string()]);
        friends.set(friend_of_friend, &[further.to_string()]);
        let trusted = [friend];

        let allows = |scope, peer: &PeerId| friends.allows(scope, &me, &trusted, peer);
        assert!(allows(SearchScope::Trusted, &friend));
        assert!(!allows(SearchScope::Trusted, &friend_of_friend));
        assert!(allows(SearchScope::FriendsOfFriends, &friend));
        assert!(allows(SearchScope::FriendsOfFriends, &friend_of_friend));
        assert!(!allows(SearchScope::FriendsOfFriends, &further));
        assert!(!allows(SearchScope::FriendsOfFriends, &stranger));
        assert!(allows(SearchScope::Anyone, &stranger));
        assert_eq!(friends.of(&me, &trusted), HashSet::from([friend_of_friend]));
    }

    #[test]
    fn lists_survive_a_restart_for_trusted_nodes_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friends.json");
        let (me, friend, gone, fof) = (peer(), peer(), peer(), peer());
        let mut friends = Friends::open(&path);
        friends.set(friend, &[fof.to_string()]);
        friends.set(gone, &[peer().to_string()]);
        friends.save(&[friend]).unwrap();
        let friends = Friends::open(&path);
        assert_eq!(friends.of(&me, &[friend, gone]), HashSet::from([fof]));

        let many: Vec<String> = (0..MAX_SHARED_TRUST + 10)
            .map(|_| peer().to_string())
            .collect();
        let mut friends = Friends::default();
        friends.set(friend, &many);
        assert_eq!(friends.of(&me, &[friend]).len(), MAX_SHARED_TRUST);
    }
}
