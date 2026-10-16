//! Turning crawl results into sites, each its stored item
//! ([`plumb_core::site`]).

use std::collections::{BTreeMap, BTreeSet};

use plumb_core::{
    collapse_whitespace, is_homepage_path, is_useful_anchor, linker_bit, normalize_text, now_unix,
    registrable_domain, site, truncate_chars, valid_links_to, Redirect, Signals, MAX_ALIASES,
    MAX_TEXT_CHARS,
};
use plumb_format::Item;
use url::Url;

use crate::{CrawlOutcome, CrawlResult, CrawledPage};

/// What this crawler reads from a homepage, stamped on every site it
/// reads ([`plumb_core::site::crawl_version`]). Raise it when a change to
/// what crawls read makes the sites read before worth reading again: a
/// better way to pick a homepage's words (the terms that go with its body
/// text), say. Nodes then crawl the sites last read by a version older
/// than [`REREAD_BELOW`] again, best link score first, in the share of
/// each round that goes to sites due again (see `plumb_node::crawl`), so
/// the whole web of a node is read anew most-known sites first, without
/// waiting the month its sites are otherwise due after.
///
/// 0 is how crawlers read homepages up to 2026-10-06 (title, description,
/// headings, the first 100 words of visible text, key pages, links); 1
/// adds the search terms picked from the whole page
/// ([`crate::pick_terms`]); 2 leaves boilerplate (cookie notices, menus
/// made of links, "Learn more" buttons) out of the visible text
/// (`boilerplate.rs`); 3 reads every page in its charset, declared late
/// in its head or not at all (`charset.rs`).
pub const CRAWL_VERSION: u32 = 3;

/// Sites an older crawler read with a `�` or an escape byte in their
/// title or description, as a page read in the wrong charset comes out,
/// are read again ahead of their turn by crawlers from this version on.
pub const CHARSETS_READ_SINCE: u32 = 3;

/// Sites last read by a crawler older than this are read again ahead of
/// their turn (see [`CRAWL_VERSION`]). It stays at 1, as in 0.2.0, until
/// 0.3.0: at 2, a node upgraded from 0.2.0 would read every site it has
/// again at once, and a trusted 0.2.0 crawler's fresher crawl of a site
/// (version 1) would make it due again on every newer node. Sites read
/// by version 1 get version 2's text when they come due as usual.
pub const REREAD_BELOW: u32 = 1;

/// Turns crawl results into sites to merge into a node's: one per fetched
/// homepage ([`homepage`]: url, title, description, `site_name` and the
/// JSON-LD names as its names, when it was read, the first
/// [`plumb_core::MAX_LINKS_TO`] other sites it links to), plus one per
/// linked domain carrying the texts of links to its front page and, as
/// its [`Signals::linking_domains`], the number of distinct crawled
/// domains linking to it. Domains seen only as link targets are new
/// discoveries. Each site is its stored item as a node keeps it
/// ([`plumb_core::site`]).
///
/// Details:
/// - Only links to a front page ([`is_homepage_path`] on the URL's path,
///   so any query string is fine) contribute their text. Text on a deep
///   link names the page, not the site: an article headline, or "U.S. Bank"
///   on `facebook.com/usbank`, which would otherwise make facebook.com a
///   match for "us bank". Every link still counts toward `linking_domains`
///   and still creates the linked domain's site.
/// - A link text counts once per linking domain, however often that site
///   repeats it, and merging keeps it that way across crawls (see
///   [`plumb_core::LinkText::linkers`]). Links from a site to itself are
///   ignored, and so are texts that say nothing about the site ("click
///   here", "official website", the URL itself; see
///   [`plumb_core::is_useful_anchor`]).
/// - [`CrawlOutcome::OffsiteRedirect`] marks the domain that redirected
///   with a [`Redirect`] to the registrable domain it points to, timed
///   now, and [`Redirect::inner`] unless it points at that site's front
///   page ([`is_front_page`]), and adds an empty site for that domain, so
///   it gets discovered. Other outcomes add nothing.
/// - Sites of the same domain are merged with [`site::merge`], so there
///   is one per domain, sorted by domain. The homepages read go in first
///   and the sites they lead to after, so that merging, which tidies the
///   names it takes in ([`site::add_alias`]), never takes in a
///   homepage's: they stay as the page wrote them ([`add_name`]).
pub fn to_items(results: &[CrawlResult]) -> Vec<Item> {
    let mut sites: BTreeMap<String, Item> = BTreeMap::new();
    // Domains homepages sent the crawler to.
    let mut redirected_to: BTreeSet<String> = BTreeSet::new();
    // Linked domain -> linking (crawled) domain -> distinct link texts.
    let mut inbound: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();

    let now = now_unix();
    for result in results {
        match &result.outcome {
            CrawlOutcome::Fetched(page) => {
                upsert(&mut sites, homepage(page));
                for link in &page.meta.links {
                    if link.target_domain.is_empty() || link.target_domain == page.domain {
                        continue;
                    }
                    // Created even without a text: the link still counts.
                    let texts = inbound
                        .entry(link.target_domain.as_str())
                        .or_default()
                        .entry(page.domain.as_str())
                        .or_default();
                    if has_useful_front_page_text(&link.url, &link.text) {
                        texts.insert(link.text.as_str());
                    }
                }
            }
            CrawlOutcome::OffsiteRedirect { final_url } => {
                if let Some(domain) = registrable_domain(final_url) {
                    if domain != result.domain {
                        let mut from = site::new(result.domain.as_str());
                        let redirect = Redirect {
                            inner: !is_front_page(final_url, &domain),
                            to: domain.clone(),
                            at: now,
                        };
                        site::set_redirect(&mut from, Some(redirect));
                        upsert(&mut sites, from);
                        redirected_to.insert(domain);
                    }
                }
            }
            CrawlOutcome::RobotsDisallowed
            | CrawlOutcome::HttpStatus { .. }
            | CrawlOutcome::NotHtml { .. }
            | CrawlOutcome::BotCheck { .. }
            | CrawlOutcome::Failed { .. } => {}
        }
    }

    for domain in redirected_to {
        upsert(&mut sites, site::new(domain));
    }
    for (target, linking) in inbound {
        let mut linked = site::new(target);
        let signals = Signals {
            linking_domains: u32::try_from(linking.len()).unwrap_or(u32::MAX),
            ..Signals::default()
        };
        site::set_signals(&mut linked, &signals);
        let mut text_linkers: BTreeMap<&str, u64> = BTreeMap::new();
        for (&linker, texts) in &linking {
            for &text in texts {
                *text_linkers.entry(text).or_default() |= linker_bit(linker);
            }
        }
        for (text, linkers) in text_linkers {
            // Ignores empty texts; keeps the most used ones.
            site::add_link_text_linkers(&mut linked, text, linkers);
        }
        upsert(&mut sites, linked);
    }

    sites.into_values().collect()
}

/// The site a fetched homepage says it is, read when it was fetched by a
/// crawler of [`CRAWL_VERSION`].
fn homepage(page: &CrawledPage) -> Item {
    let meta = &page.meta;
    let mut site = site::new(page.domain.as_str());
    site.url = Some(page.final_url.clone());
    site.title = meta.title.clone();
    site.about = meta.description.clone();
    for name in meta.site_name.iter().chain(&meta.structured_names) {
        add_name(&mut site, name);
    }
    site::set_search_url(&mut site, meta.search_url.clone());
    site.lang = meta.language.clone();
    site::set_headings(&mut site, meta.headings.clone());
    site.text = meta.body_text.clone();
    site::set_terms(&mut site, meta.terms.clone());
    site::set_key_pages(&mut site, meta.key_pages.clone());
    let links = meta.links.iter().map(|link| link.target_domain.clone());
    site::set_links_to(&mut site, valid_links_to(links.collect(), &page.domain));
    site::set_crawled(&mut site, Some(page.fetched_at));
    site::set_crawl_version(&mut site, CRAWL_VERSION);
    site
}

/// Adds `name` to the site's names as the page wrote it (whitespace
/// collapsed, cut to [`MAX_TEXT_CHARS`]), unless one the same once
/// normalized is there or it has [`MAX_ALIASES`]. Unlike
/// [`site::add_alias`], it leaves the name's Unicode form and control
/// characters as they are: a node's sites tidy what they take
/// ([`site::clean`]), and so the changes a crawl saves and the sites it
/// shares stay byte for byte what they were when crawls made records.
fn add_name(site: &mut Item, name: &str) {
    let name = truncate_chars(&collapse_whitespace(name), MAX_TEXT_CHARS);
    let key = normalize_text(&name);
    if key.is_empty() || site.names.len() >= MAX_ALIASES {
        return;
    }
    if site.names.iter().any(|known| normalize_text(known) == key) {
        return;
    }
    site.names.push(name);
}

/// Whether a link points at a site's front page (any query string is fine)
/// with text worth keeping.
fn has_useful_front_page_text(url: &str, text: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|parsed| is_homepage_path(parsed.path()) && is_useful_anchor(text, url, &parsed))
}

/// Whether `url` is the front page of `site`: on its own host or `www.`
/// and it, at a homepage path ([`is_homepage_path`]), whatever the query.
/// `https://www.pnc.com/en/` is pnc.com's; Google's sign-in on
/// `accounts.google.com` and Bungie's page on Destiny are not.
fn is_front_page(url: &str, site: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        let host = url.host_str().unwrap_or_default();
        host.strip_prefix("www.").unwrap_or(host) == site && is_homepage_path(url.path())
    })
}

/// Adds `site` to `sites`, merged ([`site::merge`]) into the one of its
/// domain already there.
fn upsert(sites: &mut BTreeMap<String, Item>, site: Item) {
    match sites.get_mut(&site.id) {
        Some(held) => site::merge(held, site),
        None => {
            sites.insert(site.id.clone(), site);
        }
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::sites::Sites;
    use plumb_core::{KeyPage, LinkText, SiteRecord};

    use super::*;
    use crate::{OutLink, PageMeta};

    /// A fetched homepage of `domain` with links given as (URL, normalized text).
    fn fetched(domain: &str, links: &[(&str, &str)]) -> CrawlResult {
        CrawlResult {
            domain: domain.into(),
            outcome: CrawlOutcome::Fetched(CrawledPage {
                domain: domain.into(),
                final_url: format!("https://www.{domain}/"),
                status: 200,
                fetched_at: 1_700_000_000,
                meta: PageMeta {
                    title: Some(format!("{domain} home")),
                    description: Some("About us".into()),
                    site_name: Some(format!("{domain} site")),
                    structured_names: vec![format!("{domain} Corporation")],
                    search_url: None,
                    language: Some("en".into()),
                    icons: Vec::new(),
                    key_pages: Vec::new(),
                    headings: Vec::new(),
                    sections: Vec::new(),
                    search: None,
                    body_text: None,
                    page_text: String::new(),
                    terms: Vec::new(),
                    feed: None,
                    links: links
                        .iter()
                        .map(|&(url, text)| OutLink {
                            url: url.into(),
                            target_domain: registrable_domain(url).unwrap(),
                            text: text.into(),
                        })
                        .collect(),
                    own_links: Vec::new(),
                },
                icon: None,
            }),
        }
    }

    fn result(domain: &str, outcome: CrawlOutcome) -> CrawlResult {
        CrawlResult {
            domain: domain.into(),
            outcome,
        }
    }

    /// The site of `domain` among `sites`.
    fn get<'a>(sites: &'a [Item], domain: &str) -> &'a Item {
        sites.iter().find(|site| site.id == domain).unwrap()
    }

    /// A site's link texts as (text, number of linking sites).
    fn texts(site: &Item) -> Vec<(String, u32)> {
        let link_texts = site::link_texts(site).into_iter();
        link_texts.map(|lt| (lt.text, lt.count)).collect()
    }

    fn linking_domains(site: &Item) -> u32 {
        site::signals(site).linking_domains
    }

    #[test]
    fn fetched_pages_become_sites() {
        let sites = to_items(&[fetched("usbank.com", &[])]);
        assert_eq!(sites.len(), 1);
        let site = &sites[0];
        assert_eq!(site.id, "usbank.com");
        assert_eq!(site.url.as_deref(), Some("https://www.usbank.com/"));
        assert_eq!(site.title.as_deref(), Some("usbank.com home"));
        assert_eq!(site.about.as_deref(), Some("About us"));
        assert_eq!(site.names, ["usbank.com site", "usbank.com Corporation"]);
        assert_eq!(site::crawled(site), Some(1_700_000_000));
        assert_eq!(site::crawl_version(site), CRAWL_VERSION);
        assert!(site::link_texts(site).is_empty());
        assert_eq!(linking_domains(site), 0);
    }

    #[test]
    fn links_become_link_texts_and_linking_domains() {
        let results = [
            fetched(
                "a.com",
                &[
                    ("https://target.org/", "target"),
                    ("https://www.target.org/", "target"),
                    ("https://target.org/", "our friends"),
                    ("https://b.com/", "bee"),
                    ("https://www.a.com/", "self link"),
                ],
            ),
            fetched(
                "b.com",
                &[
                    ("https://target.org/", "target"),
                    ("https://target.org/", ""),
                ],
            ),
            fetched("c.com", &[("https://target.org/", "")]),
        ];
        let sites = to_items(&results);
        let domains: Vec<&str> = sites.iter().map(|site| site.id.as_str()).collect();
        assert_eq!(domains, ["a.com", "b.com", "c.com", "target.org"]);

        let target = &sites[3];
        assert_eq!(
            texts(target),
            [("target".into(), 2), ("our friends".into(), 1)]
        );
        assert_eq!(linking_domains(target), 3);
        assert_eq!(
            (target.title.as_deref(), site::crawled(target)),
            (None, None)
        );

        // A crawled domain that is also linked keeps its page and gains texts.
        let b = &sites[1];
        assert_eq!(b.title.as_deref(), Some("b.com home"));
        assert_eq!(texts(b), [("bee".into(), 1)]);
        assert_eq!(linking_domains(b), 1);

        // Self links do not count.
        let a = &sites[0];
        assert!(site::link_texts(a).is_empty());
        assert_eq!(linking_domains(a), 0);

        // Each homepage keeps the sites it links to, once each, in page order.
        assert_eq!(site::links_to(a), ["target.org", "b.com"]);
        assert_eq!(site::links_to(b), ["target.org"]);
        assert!(site::links_to(target).is_empty());
    }

    #[test]
    fn only_links_to_front_pages_carry_text() {
        let results = [
            fetched(
                "usbank.com",
                &[
                    ("https://www.facebook.com/usbank", "us bank"),
                    (
                        "https://news.example.org/2026/10/03/story.html",
                        "big story",
                    ),
                    ("https://partner.org/?ref=usbank", "partner"),
                    ("https://www.partner.org/en-us/", "partner us"),
                    ("https://shop.example.net/index.html", "shop"),
                ],
            ),
            fetched("other.com", &[("https://www.facebook.com/", "facebook")]),
        ];
        let sites = to_items(&results);

        // Deep links still count and discover, but their text names the page.
        let facebook = get(&sites, "facebook.com");
        assert_eq!(texts(facebook), [("facebook".into(), 1)]);
        assert_eq!(linking_domains(facebook), 2);
        let news = get(&sites, "example.org");
        assert!(site::link_texts(news).is_empty());
        assert_eq!(linking_domains(news), 1);

        // Front pages, with a query string, a locale or an index file.
        assert_eq!(
            texts(get(&sites, "partner.org")),
            [("partner".into(), 1), ("partner us".into(), 1)]
        );
        assert_eq!(texts(get(&sites, "example.net")), [("shop".into(), 1)]);
    }

    #[test]
    fn texts_that_say_nothing_about_the_site_are_dropped() {
        let results = [fetched(
            "a.com",
            &[
                ("https://acme.example/", "click here"),
                ("https://acme.example/", "official website"),
                ("https://acme.example/", "home"),
                ("https://www.third.org/", "https www third org"),
                ("https://acme.example/", "acme rockets"),
            ],
        )];
        let sites = to_items(&results);
        let acme = get(&sites, "acme.example");
        assert_eq!(texts(acme), [("acme rockets".into(), 1)]);
        // The links still count.
        assert_eq!(linking_domains(acme), 1);
        assert!(site::link_texts(get(&sites, "third.org")).is_empty());
        assert_eq!(linking_domains(get(&sites, "third.org")), 1);
    }

    #[test]
    fn offsite_redirects_discover_the_destination_and_mark_the_source() {
        let results = [
            result(
                "fb.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: "https://www.facebook.com/?_rdr".into(),
                },
            ),
            result(
                "ip.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: "http://10.0.0.1/".into(),
                },
            ),
        ];
        let sites = to_items(&results);
        assert_eq!(sites.len(), 2, "{sites:?}");
        assert_eq!(sites[0], site::new("facebook.com"));
        assert_eq!(sites[1].id, "fb.com");
        let redirect = site::redirect(&sites[1]).unwrap();
        assert_eq!(redirect.to, "facebook.com");
        assert!(redirect.at > 0);
        assert!(!redirect.inner);
    }

    #[test]
    fn redirects_off_the_front_page_are_inner() {
        for (url, inner) in [
            ("https://pnc.com", false),
            ("https://www.pnc.com/en/?utm_source=pncbank", false),
            ("https://www.computerworld.com/au/", false),
            ("https://www.bungie.net/7/en/Destiny", true),
            ("https://www.nba.com/celtics/", true),
            ("https://accounts.google.com/ServiceLogin?continue=x", true),
            ("https://validate.perfdrive.com/?ssa=1", true),
            ("https://starcraft2.blizzard.com/", true),
        ] {
            let sites = to_items(&[result(
                "old.com",
                CrawlOutcome::OffsiteRedirect {
                    final_url: url.into(),
                },
            )]);
            let old = get(&sites, "old.com");
            assert_eq!(site::redirect(old).unwrap().inner, inner, "{url}");
        }
    }

    #[test]
    fn other_outcomes_add_nothing() {
        let results = [
            result("a.com", CrawlOutcome::RobotsDisallowed),
            result("b.com", CrawlOutcome::HttpStatus { status: 503 }),
            result(
                "c.com",
                CrawlOutcome::NotHtml {
                    content_type: "application/pdf".into(),
                },
            ),
            result(
                "d.com",
                CrawlOutcome::Failed {
                    error: "timed out".into(),
                    network: true,
                },
            ),
        ];
        assert!(to_items(&results).is_empty());
    }

    #[test]
    fn sites_merge_into_a_node_s_sites() {
        let mut known = site::new("target.org");
        // Seen before from old.net and a.com; a.com links again below.
        site::add_link_text(&mut known, "target", "old.net");
        site::add_link_text(&mut known, "target", "a.com");
        site::set_signals(
            &mut known,
            &Signals {
                linking_domains: 1,
                ..Signals::default()
            },
        );
        let mut sites: Sites = [known].into_iter().collect();
        sites.extend(to_items(&[
            fetched("a.com", &[("https://target.org/", "target")]),
            fetched("b.com", &[("https://target.org/", "target")]),
        ]));
        let target = sites.get("target.org").unwrap();
        assert_eq!(
            site::link_texts(target),
            [LinkText::from_linkers(
                "target",
                linker_bit("old.net") | linker_bit("a.com") | linker_bit("b.com")
            )]
        );
        assert_eq!(texts(target), [("target".into(), 3)]);
        assert_eq!(linking_domains(target), 2);
        assert_eq!(sites.len(), 3);
    }

    /// What crawls made of their results when they made records, as they
    /// did up to step 7b of the data format plan: [`to_items`] is checked
    /// against it.
    fn records_made(results: &[CrawlResult]) -> Vec<SiteRecord> {
        let mut records: BTreeMap<String, SiteRecord> = BTreeMap::new();
        let mut inbound: BTreeMap<&str, BTreeMap<&str, BTreeSet<&str>>> = BTreeMap::new();
        let upsert = |records: &mut BTreeMap<String, SiteRecord>, record: SiteRecord| match records
            .get_mut(&record.domain)
        {
            Some(existing) => existing.merge(record),
            None => {
                records.insert(record.domain.clone(), record);
            }
        };
        let now = now_unix();
        for result in results {
            match &result.outcome {
                CrawlOutcome::Fetched(page) => {
                    let mut record = SiteRecord::new(page.domain.as_str());
                    record.url = Some(page.final_url.clone());
                    record.title = page.meta.title.clone();
                    record.description = page.meta.description.clone();
                    if let Some(site_name) = &page.meta.site_name {
                        record.add_alias(site_name);
                    }
                    for name in &page.meta.structured_names {
                        record.add_alias(name);
                    }
                    record.search_url = page.meta.search_url.clone();
                    record.language = page.meta.language.clone();
                    record.headings = page.meta.headings.clone();
                    record.body_text = page.meta.body_text.clone();
                    record.terms = page.meta.terms.clone();
                    record.key_pages = page.meta.key_pages.clone();
                    let links = page.meta.links.iter();
                    let links = links.map(|link| link.target_domain.clone()).collect();
                    record.links_to = valid_links_to(links, &page.domain);
                    record.crawled_at = Some(page.fetched_at);
                    record.crawl_version = CRAWL_VERSION;
                    upsert(&mut records, record);
                    for link in &page.meta.links {
                        if link.target_domain.is_empty() || link.target_domain == page.domain {
                            continue;
                        }
                        let texts = inbound
                            .entry(link.target_domain.as_str())
                            .or_default()
                            .entry(page.domain.as_str())
                            .or_default();
                        if has_useful_front_page_text(&link.url, &link.text) {
                            texts.insert(link.text.as_str());
                        }
                    }
                }
                CrawlOutcome::OffsiteRedirect { final_url } => {
                    if let Some(domain) = registrable_domain(final_url) {
                        if domain != result.domain {
                            let mut from = SiteRecord::new(result.domain.as_str());
                            from.redirect = Some(Redirect {
                                inner: !is_front_page(final_url, &domain),
                                to: domain.clone(),
                                at: now,
                            });
                            upsert(&mut records, from);
                            upsert(&mut records, SiteRecord::new(domain));
                        }
                    }
                }
                _ => {}
            }
        }
        for (target, linking) in inbound {
            let mut record = SiteRecord::new(target);
            record.signals.linking_domains = u32::try_from(linking.len()).unwrap_or(u32::MAX);
            let mut text_linkers: BTreeMap<&str, u64> = BTreeMap::new();
            for (&linker, texts) in &linking {
                for &text in texts {
                    *text_linkers.entry(text).or_default() |= linker_bit(linker);
                }
            }
            for (text, linkers) in text_linkers {
                record.add_link_text_linkers(text, linkers);
            }
            upsert(&mut records, record);
        }
        records.into_values().collect()
    }

    /// Results that differ in what making sites of them reads: rich and
    /// plain homepages, names and link texts that are not tidy, more
    /// names than a site keeps, a homepage sent to another site read in
    /// the same batch (before and after it), sent inside it, to an
    /// address and to itself, and links of every kind.
    fn varied_results() -> Vec<CrawlResult> {
        let mut rich = fetched(
            "rich.com",
            &[
                ("https://plain.org/", "Plain\u{7} Org"),
                ("https://www.plain.org/", "Pla\u{301}in"),
                ("https://foo_bar.com/", "foo bar"),
                ("https://deep.net/a/b", "deep"),
                ("https://moved.com/", "moved"),
            ],
        );
        let CrawlOutcome::Fetched(page) = &mut rich.outcome else {
            unreachable!()
        };
        page.meta.site_name = Some("Cafe\u{301}\u{1B} Rich".into());
        page.meta.structured_names = (0..20).map(|i| format!("Rich {i}")).collect();
        page.meta
            .structured_names
            .insert(0, "cafe\u{301}\u{1B} rich".into());
        page.meta.search_url = Some("https://rich.com/search?q={searchTerms}".into());
        page.meta.headings = vec!["Welcome".into(), " Spaced ".into()];
        page.meta.body_text = Some("word ".repeat(300));
        page.meta.terms = vec!["rich".into()];
        page.meta.key_pages = vec![
            KeyPage {
                label: "About".into(),
                url: "https://www.rich.com/about".into(),
            },
            KeyPage {
                label: "Help".into(),
                url: "HTTPS://WWW.Rich.com/help%2ehtml".into(),
            },
        ];
        let mut plain = fetched("plain.org", &[("https://rich.com/", "Rich")]);
        let CrawlOutcome::Fetched(page) = &mut plain.outcome else {
            unreachable!()
        };
        page.meta.site_name = Some("Ste\u{301}phane".into());
        let moved = |from: &str, to: &str| {
            result(
                from,
                CrawlOutcome::OffsiteRedirect {
                    final_url: to.into(),
                },
            )
        };
        vec![
            moved("old-rich.com", "https://www.rich.com/"),
            rich,
            moved("inner.com", "https://plain.org/en/about"),
            plain,
            moved("moved.com", "https://www.rich.com/"),
            moved("self.com", "https://www.self.com/"),
            moved("ip.com", "http://10.0.0.1/"),
            result("robots.com", CrawlOutcome::RobotsDisallowed),
        ]
    }

    #[test]
    fn sites_are_the_stored_items_of_the_records_crawls_made() {
        let varied = varied_results();
        let mut reversed = varied.clone();
        reversed.reverse();
        let linking = [
            fetched("a.com", &[("https://target.org/", "target")]),
            fetched(
                "b.com",
                &[("https://target.org/", "Target"), ("https://a.com/", "a")],
            ),
        ];
        for results in [&varied[..], &reversed, &linking, &varied[..1]] {
            let records = records_made(results);
            let mut sites = to_items(results);
            assert!(sites.len() > 1 || results.len() == 1, "{sites:?}");
            for (site, record) in sites.iter_mut().zip(&records) {
                // A redirect's time is the clock's, read by each in turn.
                if let (Some(mut theirs), Some(mine)) = (site::redirect(site), &record.redirect) {
                    theirs.at = mine.at;
                    site::set_redirect(site, Some(theirs));
                }
                assert_eq!(
                    *site,
                    record.clone().stored_item_as_is(),
                    "{}",
                    record.domain
                );
                assert_eq!(&SiteRecord::from_stored_item(site).unwrap(), record);
            }
            assert_eq!(sites.len(), records.len());
        }
        // The names stay as the page wrote them, as records kept them.
        let sites = to_items(&varied);
        assert_eq!(get(&sites, "rich.com").names[0], "Cafe\u{301}\u{1B} Rich");
        assert_eq!(get(&sites, "rich.com").names.len(), MAX_ALIASES);
        let plain = &get(&sites, "plain.org").names;
        assert_eq!(*plain, ["Ste\u{301}phane", "plain.org Corporation"]);
    }
}
