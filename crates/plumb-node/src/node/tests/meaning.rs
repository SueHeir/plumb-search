//! Search by meaning: embedding sites here, or taking the vectors another
//! node made.

use super::*;

/// The text a node embeds of `record`'s site, read from the records file
/// as the line it is: its stored item as it is.
fn text_of(record: &SiteRecord) -> String {
    plumb_embed::site_text(&record.clone().stored_item_as_is())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_by_meaning_embeds_sites_in_the_background() {
    let dir = seeded_dir();
    // The model is in place, so nothing is downloaded.
    plumb_embed::write_test_model(&dir.path().join(MeaningModel::Small.dir_name())).unwrap();
    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while node.inner.meaning.get().is_none_or(|m| m.is_empty()) {
        assert!(std::time::Instant::now() < deadline, "no vectors made");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Names still win.
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    // Every site with text gets a vector, saved for the next start.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let with_text = fixture_records()
        .iter()
        .filter(|r| !text_of(r).is_empty())
        .count();
    while plumb_embed::Vectors::load(&vectors_path).map_or(0, |v| v.len()) < with_text {
        assert!(std::time::Instant::now() < deadline, "vectors not saved");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_by_meaning_downloads_and_runs_embedding_gemma_when_chosen() {
    let made = tempfile::tempdir().unwrap();
    plumb_embed::write_test_gemma(made.path()).unwrap();
    let model = std::fs::read(made.path().join(plumb_embed::GEMMA_FILE)).unwrap();
    let tokenizer = std::fs::read(made.path().join(plumb_embed::GEMMA_TOKENIZER_FILE)).unwrap();
    let host = SeedHost::start(move |request, _| match request {
        "GET /gemma/model.gguf" => http("200 OK", &model),
        "GET /gemma/tokenizer.json" => http("200 OK", &tokenizer),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    config.meaning_model = MeaningModel::Gemma;
    config.sources.gemma_downloads = [plumb_embed::GEMMA_FILE, plumb_embed::GEMMA_TOKENIZER_FILE]
        .map(|name| (name.to_string(), host.url(&format!("/gemma/{name}"))))
        .to_vec();
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;

    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while plumb_embed::Vectors::load(&vectors_path).map_or(0, |v| v.len()) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no vectors saved: {:?}",
            node.inner.meaning_work()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.shutdown().await.unwrap();
    let gemma_dir = dir.path().join(MeaningModel::Gemma.dir_name());
    let (model, dim, _) = plumb_embed::Vectors::read_header(&vectors_path).unwrap();
    assert_eq!(model, plumb_embed::gemma_id(&gemma_dir).unwrap());
    assert_eq!(model, plumb_embed::gemma_id(made.path()).unwrap());
    assert_eq!(dim, 96);
    // The small model was never downloaded.
    assert!(!dir.path().join(MeaningModel::Small.dir_name()).exists());
}

/// A trusted node's buckets plus its vectors file, served as the set of
/// vectors of its model.
struct WithVectors(plumb_net::BucketTable, PathBuf, String);

impl plumb_net::BucketSource for WithVectors {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        (set == self.2).then(|| self.1.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_site_vectors_made_from_its_own_text_from_a_trusted_node() {
    takes_site_vectors(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_site_vectors_from_their_signed_set_from_any_node() {
    takes_site_vectors(true).await;
}

/// A node turning search by meaning on takes the vectors another node made
/// with its model, from a trusted node's vectors file or, `as_set`, from
/// the signed set of them that any node holds, and keeps only those made
/// from the text it has for a site.
async fn takes_site_vectors(as_set: bool) {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let dir = seeded_dir();
    let model_dir = dir.path().join(MeaningModel::Small.dir_name());
    plumb_embed::write_test_model(&model_dir).unwrap();
    let model = plumb_embed::model_id(&model_dir).unwrap();

    // The trusted node's vectors: one for a site's very text, one for
    // other text, one for a site this node does not know.
    let records = fixture_records();
    let mut with_text = records.iter().filter(|r| !text_of(r).is_empty());
    let same = with_text.next().unwrap();
    let other = with_text.next().unwrap();
    let marker = [7i8; 32];
    let mut theirs = plumb_embed::Vectors::new(model, 32);
    let hash = plumb_embed::text_hash(&text_of(same));
    theirs.insert(&same.domain, hash, &marker).unwrap();
    theirs.insert(&other.domain, [9; 32], &marker).unwrap();
    theirs.insert("unknown-to-it.org", hash, &marker).unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let file = peer_dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    theirs.save(&file).unwrap();
    let held = peer_dir.path().join("held");
    let set = crate::meaning::vectors_set(&model);
    let publisher = SigningKey::from_bytes(&[7; 32]);
    if as_set {
        let out = crate::node::signed_sets::set_dir(&held, &set).unwrap();
        let args = plumb_tools::set::VectorsArgs {
            vectors: file.clone(),
            name: None,
            out: out.clone(),
            license: "CC0-1.0".into(),
            attribution: "From a test".into(),
            level: 3,
        };
        assert_eq!(plumb_tools::set::vectors(&args).unwrap().1, set);
        plumb_tools::set::sign_set(&out, &publisher, None, None).unwrap();
    }
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), &shared_vectors::set_name(&model)),
        Some(file.clone())
    );
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), &shared_vectors::set_name(&[0; 32])),
        None
    );
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), "wikipedia-en"),
        None
    );

    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[SiteRecord::new("lighthouses.org")],
    )
    .unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let source: Arc<dyn plumb_net::BucketSource> = if as_set {
        Arc::new(WithSignedSets(table, held))
    } else {
        Arc::new(WithVectors(table, file, shared_vectors::set_name(&model)))
    };
    let (peer, _records) = plumb_net::start(peer_config, source).await.unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    config.set_publishers = vec![PublicKey::of(&publisher)];
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    // The set needs no trusted node, only a trusted publisher.
    net.trusted_peers = if as_set { Vec::new() } else { vec![peer_id] };
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;

    // Every site with text ends with a vector: the one sent for its text
    // kept as sent, the others made here.
    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let wanted = records.iter().filter(|r| !text_of(r).is_empty()).count();
    let deadline = Instant::now() + Duration::from_secs(60);
    let vectors = loop {
        if let Ok(vectors) = plumb_embed::Vectors::load(&vectors_path) {
            if vectors.len() == wanted && vectors.get(&same.domain).is_some() {
                break vectors;
            }
        }
        assert!(Instant::now() < deadline, "vectors not saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(vectors.get(&same.domain), Some((&hash, &marker[..])));
    assert_ne!(vectors.get(&other.domain).unwrap().1, &marker[..]);
    assert!(vectors.get("unknown-to-it.org").is_none());
    // Read into the node's own file, the set is not kept.
    let taken = crate::node::signed_sets::set_dir(dir.path(), &set).unwrap();
    assert!(!taken.exists() && !taken.with_extension("new").exists());

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}
