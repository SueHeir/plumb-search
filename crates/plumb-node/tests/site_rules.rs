//! A node that takes a signed site rules set works out sites by it, in
//! place of the built-in lists. Its own test binary: the rules a process
//! uses are set once for good.

use std::path::Path;
use std::process::Command;

use plumb_node::site_rules::{read_site_rules, use_site_rules};

fn plumb(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_plumb"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "plumb {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn a_node_works_out_sites_by_a_signed_site_rules_set() {
    let dir = tempfile::tempdir().unwrap();
    // A suffix list that, unlike the built-in one, lacks github.io.
    let psl = dir.path().join("public_suffix_list.dat");
    std::fs::write(
        &psl,
        "// a short list\ncom\norg\nio\nuk\nco.uk\ngov\n*.ck\n!www.ck\n",
    )
    .unwrap();
    let hosts = dir.path().join("hosts.txt");
    std::fs::write(&hosts, "https://x.github.io/a\nwww.bbc.co.uk\n").unwrap();
    let (set, key) = (dir.path().join("set"), dir.path().join("set.key"));
    let printed = plumb(&[
        "set",
        "site-rules",
        "--psl",
        path(&psl),
        "--out",
        path(&set),
        "--hosts",
        path(&hosts),
    ]);
    assert!(
        printed.contains("x.github.io: x.github.io -> github.io"),
        "{printed}"
    );
    let publisher = plumb(&["set", "new-key", "--out", path(&key)]);
    plumb(&["set", "sign", "--key", path(&key), path(&set)]);

    let other = plumb(&[
        "set",
        "new-key",
        "--out",
        path(&dir.path().join("other.key")),
    ]);
    assert!(read_site_rules(&set, &other.trim().parse().unwrap(), &[]).is_err());
    let (rules, newer) = read_site_rules(&set, &publisher.trim().parse().unwrap(), &[]).unwrap();
    assert_eq!(newer, 0);

    assert_eq!(
        plumb_core::registrable_domain("https://x.github.io/a").as_deref(),
        Some("x.github.io")
    );
    assert!(use_site_rules(rules.clone()));
    assert!(!use_site_rules(rules), "the rules in use stay");
    for (input, site) in [
        ("https://x.github.io/a", Some("github.io")),
        ("https://news.bbc.co.uk/", Some("bbc.co.uk")),
        ("https://a.b.ck/", Some("a.b.ck")),
        ("https://x.www.ck/", Some("www.ck")),
        (
            "https://m.news.ycombinator.com/",
            Some("news.ycombinator.com"),
        ),
        ("https://www.dmv.ca.gov/", Some("dmv.ca.gov")),
        ("https://www.ca.gov/", Some("ca.gov")),
        ("co.uk", None),
    ] {
        assert_eq!(
            plumb_core::registrable_domain(input).as_deref(),
            site,
            "{input}"
        );
    }
    assert_eq!(
        plumb_core::canonical_domain("x.github.io").as_deref(),
        Some("github.io")
    );
    assert_eq!(
        plumb_core::canonical_domain("bbc.co.uk").as_deref(),
        Some("bbc.co.uk")
    );
    assert_eq!(plumb_core::domain_label("bbc.co.uk"), "bbc");
    assert_eq!(
        plumb_core::parent_domain("news.ycombinator.com"),
        Some("ycombinator.com")
    );
    assert_eq!(plumb_core::parent_domain("ycombinator.com"), None);
    assert!(plumb_core::subdomain_sites()
        .any(|pair| pair == ("news.ycombinator.com", "ycombinator.com")));
}
