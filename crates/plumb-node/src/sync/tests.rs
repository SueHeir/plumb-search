use super::*;
use crate::history::{new_profile, Opened, PastSearch};
use crate::learn::{Block, BlockCount, SiteVerdict, Taste, Trait, Verdict};

/// A node reached through its folder, as `from` would reach it.
struct Folder<'a> {
    dir: &'a Path,
    from: PeerId,
}

impl Asker for Folder<'_> {
    async fn ask(&self, _peer: PeerId, request: ProfileRequest) -> Result<ProfileResponse> {
        Ok(answer(self.dir, self.from, request))
    }
}

fn search(query: &str, at: u64) -> PastSearch {
    PastSearch {
        query: query.into(),
        at,
    }
}

fn opened(domain: &str, times: u32, at: u64) -> Opened {
    Opened {
        query: "bank".into(),
        domain: domain.into(),
        at,
        times,
    }
}

fn queries(history: &History) -> Vec<&str> {
    history.searches.iter().map(|s| s.query.as_str()).collect()
}

#[test]
fn counts_add_up_what_each_side_added() {
    let base = History {
        opened: vec![opened("usbank.com", 2, 1)],
        ..History::default()
    };
    let local = History {
        opened: vec![opened("usbank.com", 3, 2)],
        ..History::default()
    };
    let remote = History {
        opened: vec![opened("usbank.com", 4, 3)],
        ..History::default()
    };
    let merged = merge_history(Some(&base), &local, &remote);
    assert_eq!(merged.opened[0].times, 5);
    assert_eq!(merged.opened[0].at, 3);
    // Without a base nothing is counted twice.
    let merged = merge_history(None, &local, &remote);
    assert_eq!(merged.opened[0].times, 4);
}

#[test]
fn deleted_stays_deleted_unless_changed_on_the_other_side() {
    let base = History {
        searches: vec![search("github", 1), search("chase", 1)],
        ..History::default()
    };
    // Local cleared its history; remote searched chase again and added rust.
    let local = History::default();
    let remote = History {
        searches: vec![search("rust", 3), search("chase", 2), search("github", 1)],
        ..History::default()
    };
    let merged = merge_history(Some(&base), &local, &remote);
    assert_eq!(queries(&merged), ["rust", "chase"]);
    // Without a base nothing is lost.
    let merged = merge_history(None, &local, &remote);
    assert_eq!(queries(&merged), ["rust", "chase", "github"]);
}

#[test]
fn what_was_learned_merges_too() {
    let count = |shown, used| BlockCount {
        block: Block::Places,
        key: "q us bank".into(),
        shown,
        used,
        at: 1,
    };
    let verdict = |verdict, at| SiteVerdict {
        query: "bank".into(),
        domain: "chase.com".into(),
        verdict,
        at,
    };
    let taste = |liked| Taste {
        kind: Trait::Code,
        liked,
        disliked: 0.0,
        seen: liked,
    };
    let base = Learned {
        blocks: vec![count(3, 0)],
        tastes: vec![taste(1.0)],
        ..Learned::default()
    };
    let local = Learned {
        blocks: vec![count(5, 0)],
        verdicts: vec![verdict(Verdict::Up, 1)],
        tastes: vec![taste(2.0)],
        ..Learned::default()
    };
    let remote = Learned {
        blocks: vec![count(4, 1)],
        verdicts: vec![verdict(Verdict::Hide, 2)],
        tastes: vec![taste(3.0)],
        ..Learned::default()
    };
    let merged = merge_learned(Some(&base), &local, &remote);
    assert_eq!((merged.blocks[0].shown, merged.blocks[0].used), (6, 1));
    assert_eq!(merged.verdicts[0].verdict, Verdict::Hide);
    assert_eq!(merged.tastes[0].liked, 4.0);
}

#[test]
fn about_you_merges_both_ways() {
    let about = |interests: &[&str], town: &str| About {
        interests: interests.iter().map(|s| (*s).to_owned()).collect(),
        town: town.into(),
        ..About::default()
    };
    let base = about(&["rust", "cooking"], "Denver");
    let local = about(&["rust"], "Denver");
    let remote = about(&["rust", "cooking", "hiking"], "Boulder");
    let merged = merge_about(Some(&base), &local, &remote);
    assert_eq!(merged.interests, ["rust", "hiking"]);
    assert_eq!(merged.town, "Boulder");
    let merged = merge_about(None, &about(&["Rust"], ""), &remote);
    assert_eq!(merged.interests, ["Rust", "cooking", "hiking"]);
    assert_eq!(merged.town, "Boulder");
}

#[test]
fn link_codes_work_once() {
    let profile = new_profile().unwrap();
    let token = make_code(&profile).unwrap();
    let node = PeerId::random();
    let code = format_code(&token, Some(&node));
    let (parsed, at) = parse_code(&format!(
        "  {}  ",
        code.to_lowercase()
            .replace(&node.to_string().to_lowercase(), &node.to_string(),)
    ))
    .unwrap();
    assert_eq!((parsed.as_str(), at), (token.as_str(), Some(node)));
    assert!(parse_code("hello").is_err());
    assert!(parse_code("ABCD-EFGH@nope").is_err());
    assert_eq!(take_code(&token).as_deref(), Some(profile.as_str()));
    assert_eq!(take_code(&token), None);
}

#[tokio::test]
async fn two_nodes_share_one_profile() {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (a_dir.path(), b_dir.path());
    let (a_id, b_id) = (PeerId::random(), PeerId::random());
    let (a_store, b_store) = (HistoryStore::new(a), HistoryStore::new(b));
    // Node A's browser searched github; node B's browser, chase.
    let p = new_profile().unwrap();
    a_store.update(&p, |h| h.add_search("github", 1)).unwrap();
    let q = new_profile().unwrap();
    b_store.update(&q, |h| h.add_search("chase", 2)).unwrap();
    AboutStore::new(b)
        .save(&q, &About::from_form("hiking", "", ""))
        .unwrap();

    let code = format_code(&make_code(&p).unwrap(), Some(&a_id));
    let to_a = Folder { dir: a, from: b_id };
    let joined = join(b, Some(&to_a), Some(b_id), &code, Some(&q))
        .await
        .unwrap();
    assert_eq!(
        joined,
        Joined::There {
            profile: p.clone(),
            node: a_id
        }
    );
    // Both nodes have both searches, and B's old profile is gone.
    assert_eq!(queries(&b_store.load(&p)), ["chase", "github"]);
    assert_eq!(queries(&a_store.load(&p)), ["chase", "github"]);
    assert_eq!(AboutStore::new(a).load(&p).interests, ["hiking"]);
    assert!(b_store.load(&q).searches.is_empty());
    assert_eq!(links(a, &p).nodes[0].peer, b_id.to_string());
    // The code worked once.
    assert!(join(b, Some(&to_a), Some(b_id), &code, None).await.is_err());

    // A clears its history while B searches rust: the clear reaches B, and
    // rust reaches A.
    a_store.clear(&p).unwrap();
    b_store.update(&p, |h| h.add_search("rust", 3)).unwrap();
    sync_with(b, &to_a, &p, a_id).await.unwrap();
    assert_eq!(queries(&a_store.load(&p)), ["rust"]);
    assert_eq!(queries(&b_store.load(&p)), ["rust"]);

    // Counts add up across rounds without counting twice.
    a_store
        .update(&p, |h| h.add_opened("rust", "rust-lang.org", 4))
        .unwrap();
    sync_with(b, &to_a, &p, a_id).await.unwrap();
    b_store
        .update(&p, |h| h.add_opened("rust", "rust-lang.org", 5))
        .unwrap();
    a_store
        .update(&p, |h| h.add_opened("rust", "rust-lang.org", 6))
        .unwrap();
    sync_with(b, &to_a, &p, a_id).await.unwrap();
    sync_with(b, &to_a, &p, a_id).await.unwrap();
    assert_eq!(a_store.load(&p).opened[0].times, 3);
    assert_eq!(b_store.load(&p).opened[0].times, 3);

    // A lost answer: B's round is behind A's, so they merge without a
    // base, and nothing is counted twice.
    update_link(b, &p, &a_id, |link| link.round = Some(1)).unwrap();
    sync_with(b, &to_a, &p, a_id).await.unwrap();
    assert_eq!(a_store.load(&p).opened[0].times, 3);

    // A node the profile is not shared with is refused.
    let stranger = Folder {
        dir: a,
        from: PeerId::random(),
    };
    let c_dir = tempfile::tempdir().unwrap();
    update_links(c_dir.path(), &p, |l| l.nodes.push(Link::new(&a_id))).unwrap();
    assert!(sync_with(c_dir.path(), &stranger, &p, a_id).await.is_err());
    assert_eq!(
        queries(&HistoryStore::new(c_dir.path()).load(&p)),
        Vec::<&str>::new()
    );

    // Stopping on B tells A.
    stop(b, Some(&to_a), &p, a_id).await.unwrap();
    assert!(links(a, &p).nodes.is_empty());
    assert!(links(b, &p).nodes.is_empty());
    assert!(linked_profiles(b).is_empty());
}

#[tokio::test]
async fn a_code_from_the_same_node_just_switches_browsers() {
    let dir = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(dir.path());
    let me = PeerId::random();
    let (p, q) = (new_profile().unwrap(), new_profile().unwrap());
    store.update(&p, |h| h.add_search("github", 1)).unwrap();
    store.update(&q, |h| h.add_search("chase", 2)).unwrap();
    let code = format_code(&make_code(&p).unwrap(), Some(&me));
    let joined = join(dir.path(), None::<&Folder>, Some(me), &code, Some(&q))
        .await
        .unwrap();
    assert_eq!(joined, Joined::Here(p.clone()));
    assert_eq!(queries(&store.load(&p)), ["chase", "github"]);
    assert!(store.load(&q).searches.is_empty());
}

#[test]
fn the_smaller_id_asks_unless_it_went_quiet() {
    let link = |peer: &str, heard, tried, synced| Link {
        peer: peer.into(),
        heard,
        tried,
        synced,
        ..Link::default()
    };
    let now = 100_000;
    // Asks when changed, or every few minutes anyway.
    assert!(due("a", &link("b", 0, now - 10, now - 10), now - 5, now));
    assert!(!due("a", &link("b", 0, now - 10, now - 10), now - 20, now));
    assert!(due(
        "a",
        &link("b", 0, now - SYNC_EVERY, now - SYNC_EVERY),
        0,
        now
    ));
    // The larger id waits to be asked.
    assert!(!due("b", &link("a", now - 60, now - 60, 0), now, now));
    assert!(due(
        "b",
        &link("a", now - WAIT_FOR_ASKER, now - SYNC_EVERY, 0),
        now,
        now
    ));
    // A node that did not answer is asked again only after a while.
    let mut failing = link("b", 0, now - 60, 0);
    failing.problem = Some("down".into());
    assert!(!due("a", &failing, now, now));
}

#[test]
fn kinds_of_results_merge_like_the_town() {
    use crate::about::Amount;
    let base = About::default().with_kinds([("podcasts", Amount::Off), ("books", Amount::Less)]);
    // Here podcasts came back; there books went to more and papers off.
    let local = About::default().with_kinds([("books", Amount::Less)]);
    let remote = About::default().with_kinds([
        ("podcasts", Amount::Off),
        ("books", Amount::More),
        ("papers", Amount::Off),
    ]);
    let merged = merge_about(Some(&base), &local, &remote);
    let got: Vec<(&str, Amount)> = merged.kinds.iter().map(|(k, a)| (k.as_str(), *a)).collect();
    assert_eq!(got, [("books", Amount::More), ("papers", Amount::Off)]);
}
