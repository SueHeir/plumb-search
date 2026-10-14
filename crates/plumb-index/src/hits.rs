//! The hits a search returns: each one's fields, and the lists of them
//! tidied (copies and look-alikes of a site left out, a suggested or docs
//! site moved up, untitled sites moved down).

use std::collections::HashSet;

use anyhow::Result;
use plumb_core::{is_function_word, normalize_text, registrable_domain};
use tantivy::schema::Value;
use tantivy::TantivyDocument;

use crate::names::without_intent_words;
use crate::rank::Ranked;
use crate::{health, Hit, Searcher, WELL_KNOWN_LINK_SCORE};

/// How many results past the ones asked for may move up over a site with
/// no title ([`Searcher::untitled_last`]).
const UNTITLED_LOOKAHEAD: usize = 20;

/// Share of the first result's score a site with no title needs to keep
/// its place anyway ([`Searcher::untitled_last`]).
const UNTITLED_CLOSE_SHARE: f32 = 0.9;

/// Country endings mostly used as generic ones: proton.me, grpc.io.
const GENERIC_COUNTRY_ENDINGS: [&str; 14] = [
    "io", "co", "ai", "me", "tv", "ly", "gg", "fm", "so", "sh", "to", "cc", "ws", "la",
];

/// Link score under which a site that copies a listed site's title is a
/// look-alike ([`without_copies`]).
const COPYCAT_LINK_SCORE: f32 = 0.05;

impl Searcher {
    /// `ranked` with the sites that have no title and are not well known
    /// moved after the titled ones, among the first `limit` plus
    /// [`UNTITLED_LOOKAHEAD`] past the first result: a bare `netflix.net`
    /// tells a person nothing they can judge it by, even when it has the
    /// query's name: netflix.net for "netflix". The first result keeps its
    /// place, title or not, and so does a site whose label is the whole
    /// query of two or more words: awesome-python.com for "awesome python"
    /// is what was asked for, titled or not. A site that Wikidata or its
    /// description says what it is, never crawled or not, is not bare
    /// either, nor is one with another name to show for a title
    /// (cars.com for "used cars"): vanguard.com for "vanguard". Nor is a site that scores
    /// within [`UNTITLED_CLOSE_SHARE`] of the first.
    ///
    /// A bare site the query does not name is left out, first or not, when
    /// only meaning found it: no word of `query` is in its text or its
    /// address. Nothing a person sees then ties it to the search, and the
    /// vectors of sites with little text are near many queries the model
    /// barely knows ("pyenv" found five expired domains). emmys.com for
    /// "emmy awards" shows its word in its address, so it stays. So is a
    /// site only meaning found whose title and description are in another
    /// script than the query ([`plumb_core::written_apart`]), unless well
    /// known or described by Wikidata: an English model reads little of
    /// such text, and its vector sits near niche searches ("OEIS A000108
    /// Catalan numbers" found a Chinese gambling site).
    pub(crate) fn untitled_last(
        &self,
        searcher: &tantivy::Searcher,
        mut ranked: Vec<Ranked>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Ranked>> {
        let window = limit.saturating_add(UNTITLED_LOOKAHEAD).min(ranked.len());
        let rest = ranked.split_off(window);
        let words = normalize_text(query);
        let words: Vec<&str> = words
            .split(' ')
            .filter(|word| word.chars().count() >= 3 && !is_function_word(word))
            .collect();
        let mut kept = Vec::with_capacity(window);
        for site in ranked {
            let bare = !site.label_names_query
                && site.link_score < WELL_KNOWN_LINK_SCORE
                && !self.says_what_it_is(searcher, site.addr)?
                && !self.has_aliases(searcher, site.addr)?;
            let alone = !site.named && found_by_meaning_alone(&site, &words);
            let apart = alone
                && !bare
                && site.link_score < WELL_KNOWN_LINK_SCORE
                && !self.is_official(searcher, site.addr)?
                && self.written_apart(searcher, site.addr, query)?;
            if !(alone && bare || apart) {
                kept.push((site, bare));
            }
        }
        let close = kept
            .first()
            .map_or(0.0, |(first, _)| first.score * UNTITLED_CLOSE_SHARE);
        let mut titled = Vec::with_capacity(rest.len() + kept.len());
        let mut untitled = Vec::new();
        for (i, (site, bare)) in kept.into_iter().enumerate() {
            if i > 0 && bare && site.score < close {
                untitled.push(site);
            } else {
                titled.push(site);
            }
        }
        titled.extend(untitled);
        titled.extend(rest);
        Ok(titled)
    }

    /// Whether the site's title and description are in another script
    /// than `query` ([`plumb_core::written_apart`]).
    fn written_apart(
        &self,
        searcher: &tantivy::Searcher,
        addr: tantivy::DocAddress,
        query: &str,
    ) -> Result<bool> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let text: Vec<&str> = [self.fields.title, self.fields.description]
            .into_iter()
            .filter_map(|field| doc.get_first(field).and_then(|value| value.as_str()))
            .collect();
        Ok(plumb_core::written_apart(&text.join(" "), query))
    }

    /// Reads the stored fields of a ranked document, with the
    /// [`plumb_core::simhash`] fingerprint of its homepage's text when it
    /// has one.
    pub(crate) fn hit(
        &self,
        searcher: &tantivy::Searcher,
        ranked: Ranked,
    ) -> Result<(Hit, Option<u64>)> {
        let doc: TantivyDocument = searcher.doc(ranked.addr)?;
        let fingerprint = self
            .fields
            .fingerprint
            .and_then(|field| doc.get_first(field))
            .and_then(|value| value.as_u64());
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let domain = text(self.fields.domain).unwrap_or_default();
        // A homepage read on another site's host, indexed before crawls
        // stopped there (prevention.nih.gov for nih.gov), says what that
        // site is: its address, title and description are left out until
        // the index is rebuilt without them ([`schema::document`]).
        let stored_url = text(self.fields.url);
        let borrowed = stored_url
            .as_deref()
            .is_some_and(|url| reads_another_site(url, &domain));
        let url = stored_url
            .filter(|url| !borrowed && !is_check_address(url))
            .unwrap_or_else(|| format!("https://{domain}/"));
        let own = |field| text(field).filter(|_| !borrowed);
        let about = text(self.fields.about);
        let mut hit = Hit {
            demand: None,
            missing_words: false,
            query_evidence: ranked.query_evidence,
            url,
            // A title in the wrong encoding ("����") says nothing.
            title: own(self.fields.title).filter(|title| !title.contains('\u{FFFD}')),
            official: about.is_some(),
            // The site's own description, unless its template left it
            // unfilled or it is in the wrong encoding, else Wikipedia's,
            // else what Wikidata says it is.
            description: own(self.fields.description)
                .filter(|description| !is_unfilled(description) && !is_garbled(description))
                .or(about),
            domain,
            score: ranked.score,
            text_score: ranked.text_score,
            placing_text_score: ranked.placing_text_score,
            link_score: ranked.link_score,
            country: ranked.country,
            named: ranked.named,
            key_pages: text(self.fields.key_pages)
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
        };
        if let Some(evidence) = &mut hit.query_evidence {
            evidence.source_quality = health::source_quality(
                &hit.domain,
                hit.title.as_deref(),
                hit.description.as_deref(),
                Some(hit.link_score),
            );
        }
        Ok((hit, fingerprint))
    }
}

/// Whether no word of a site's text matched the query and its address,
/// without its ending, holds none of `words` ([`Searcher::untitled_last`]).
fn found_by_meaning_alone(site: &Ranked, words: &[&str]) -> bool {
    site.query_evidence.as_ref().is_some_and(|evidence| {
        let address = evidence
            .domain
            .rsplit_once('.')
            .map_or(evidence.domain.as_str(), |(name, _)| name)
            .to_lowercase();
        evidence.lexical_score <= 0.0 && !words.iter().any(|word| address.contains(word))
    })
}

/// Whether `url`, a site's homepage address, is on another site than
/// `domain`: prevention.nih.gov for nih.gov, play.google.com for
/// google.com ([`plumb_core::subsites`]).
pub(crate) fn reads_another_site(url: &str, domain: &str) -> bool {
    registrable_domain(url).is_some_and(|site| site != domain)
}

/// Whether `url`, where a crawl of a homepage ended, is a bot check's
/// address (https://www.skyscanner.nl/sttc/px/captcha-v2/index.html?uuid=…,
/// https://www.dreamwidth.org/captcha?returnto=…): a link to it leads only
/// to a stale check, so the site's root is linked instead.
fn is_check_address(url: &str) -> bool {
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map_or("", |(_, path)| path);
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    path.contains("captcha") || path.starts_with("cdn-cgi/")
}

/// Descriptions, after [`normalize_text`], that a site's template was
/// meant to fill in.
const UNFILLED: &[&str] = &[
    "description",
    "desc",
    "site description",
    "meta description",
    "description here",
    "your description here",
    "null",
    "undefined",
];

/// Whether `description` is a template's placeholder no one filled in,
/// which says nothing of the site: "{{description}}" (frengly.com), "{{
/// page.excerpt }}", "null", "YOUR DESCRIPTION HERE", or a name and
/// "desc" ("surbl desc" on surbl.org).
pub(crate) fn is_unfilled(description: &str) -> bool {
    let text = description.trim();
    let words = normalize_text(text);
    [("{{", "}}"), ("{%", "%}"), ("${", "}")]
        .iter()
        .any(|(open, close)| text.starts_with(open) && text.ends_with(close))
        || UNFILLED.contains(&words.as_str())
        || words
            .split_once(' ')
            .is_some_and(|(_, last)| last == "desc")
}

/// Whether `text` was read in the wrong encoding, so it says nothing: a
/// quarter or more of its characters are `�` ("���¾�γ�����…" for the
/// Chinese of jwview.com), or it is one long run of base64
/// ("JTNDZGl2JTIwY2xhc3Mi…" for motorsportweek.com's markup). One
/// accent lost ("D�couvrez et explorez") leaves the rest readable.
pub(crate) fn is_garbled(text: &str) -> bool {
    let text = text.trim();
    let chars = text.chars().filter(|c| !c.is_whitespace()).count();
    let lost = text.chars().filter(|&c| c == '\u{FFFD}').count();
    let base64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    lost * 4 >= chars.max(1)
        || (chars >= 24
            && text.chars().all(base64)
            && text.chars().any(|c| c.is_ascii_digit())
            && text.chars().any(|c| c.is_ascii_uppercase())
            && text.chars().any(|c| c.is_ascii_lowercase()))
}

/// The text of `hit` that search operators look at, besides its domain.
pub(crate) fn hit_texts(hit: &Hit) -> impl Iterator<Item = &str> {
    [
        hit.title.as_deref(),
        hit.description.as_deref(),
        Some(hit.url.as_str()),
    ]
    .into_iter()
    .flatten()
}

/// Puts first the docs site of what `name` names when `query` asks for its
/// docs ("postgres docs", "mdn web docs"): far more sites link to
/// github.com with "docs" than to postgresql.org.
pub(crate) fn docs_site_first(query: &str, name: &str, hits: &mut [Hit]) {
    let words = normalize_text(query);
    if !(words.ends_with(" docs") || words.ends_with(" documentation")) {
        return;
    }
    let Some(site) = plumb_core::docs::named_site(name) else {
        return;
    };
    // The docs' own site, or the site it is part of: mozilla.org for
    // developer.mozilla.org.
    let of_docs = |domain: &str| {
        site.domain == domain
            || site
                .domain
                .strip_suffix(domain)
                .is_some_and(|sub| sub.ends_with('.'))
    };
    if let Some(at) = hits.iter().position(|hit| of_docs(&hit.domain)) {
        hits[..=at].rotate_right(1);
    }
}

/// The label and suffix of a registrable domain: ("google", "co.uk")
/// for google.co.uk. `None` for a host below one (news.ycombinator.com).
pub(crate) fn brand_label(domain: &str) -> Option<(&str, &str)> {
    if registrable_domain(domain).as_deref() != Some(domain) {
        return None;
    }
    let (label, suffix) = domain.split_once('.')?;
    // news.ycombinator.com is a site of its own, but not a brand's domain.
    registrable_domain(suffix)
        .is_none()
        .then_some((label, suffix))
}

/// Moves `site`, the site a spelling suggestion names, up to second when
/// it is listed lower: "tco bell" lists tacobell.com right under bell.ca,
/// not fourth. Its score is put between its new neighbours', so scores
/// still go down the list. Not first: the results stay those of the query
/// as typed.
pub fn suggested_site_second(hits: &mut Vec<Hit>, site: &str) {
    let Some(at) = hits.iter().position(|hit| hit.domain == site) else {
        return;
    };
    if at <= 1 {
        return;
    }
    let mut hit = hits.remove(at);
    hit.score = (hits[0].score + hits[1].score) / 2.0;
    hits.insert(1, hit);
}

/// `hits` without the copies of a site listed with them:
///
/// - One site per brand: airbnb.fr, airbnb.co.uk and airbnb.tech next to
///   airbnb.com, or google.de next to google.com, are the same site again
///   when they say nothing of their own (no title, or a title that names
///   the brand). Where one of them is the official site of
///   something Wikidata describes, it stays and the others go, even when
///   they rank above it (23andme.org's copy of 23andMe's title above
///   23andme.com); otherwise the best known (most linked) stays. Every official site
///   stays (cbc.ca next to cbc.com), and so does a country site with a
///   title of its own (amazon.co.jp's), short of the next case.
/// - The brand's site for a country the searcher is not in and did not
///   name, when the query is the brand's name, under its better-known
///   site on a generic ending, official or not and in whatever language:
///   mcdonalds.co.jp, mcdonalds.co.kr and mcdonalds.ca under
///   mcdonalds.com, skyscanner.nl under skyscanner.net. An official one
///   goes only under an official one (paris.fr stays next to paris.com),
///   or under one of the brand's official sites titled and described as
///   it is (google.fr's "Google Search" for "google maps").
/// - A site nobody links to whose title is that of a site listed above it
///   (a copy of Coinbase's title on elephant-blue.org).
/// - A little-known site whose title is that of a site listed with it,
///   is no name of its own ([`is_own_name`]) and has no description
///   other than that site's: a network of spam sites under one title
///   ("INFORMATION40" on bountystory.site and three more of its endings,
///   "Big-B" on five gambling sites, "MailUp Tracking Service" for "ups
///   tracking"), or a copy of CNN's whole homepage on mos-scripts.com.
///   The first of them stays. Two dentists both titled "Family
///   Dentistry" say different things of themselves, and both stay.
///
/// `typed` is the domain the query is, which always stays, and so do the
/// sites of `home`, the searcher's country (bbc.co.uk in Britain). The
/// brand's site for a country the query names (`asked`) is no copy
/// either: amazon.co.uk for "amazon uk", mcdonalds.co.jp for "mcdonald's
/// japan".
pub(crate) fn without_copies(
    hits: Vec<Hit>,
    typed: Option<&str>,
    home: Option<&str>,
    asked: &[&str],
) -> Vec<Hit> {
    // The title names the brand anywhere in it: "StubHub UK", "Buy and
    // sell tickets | StubHub".
    let names_brand = |hit: &Hit, label: &str| {
        hit.title
            .as_deref()
            .is_none_or(|title| normalize_text(title).replace(' ', "").contains(label))
    };
    // Endings used as generic ones (.io, .me, .co) are not countries', but
    // the same ending under a country's own (.com.co, Colombia's; .com.ai)
    // still is.
    let country = |suffix: &str| {
        suffix.rsplit('.').next().is_some_and(|tld| {
            tld.len() == 2 && (suffix.contains('.') || !GENERIC_COUNTRY_ENDINGS.contains(&tld))
        })
    };
    // The best known, not the best ranked: ranking comes again after this
    // (the learned order), and cbc.bb ranked above cbc.ca is no reason to
    // leave out cbc.ca.
    let better_known = |i: usize, j: usize| {
        j != i
            && (hits[j].link_score > hits[i].link_score
                || (hits[j].link_score == hits[i].link_score && j < i))
    };
    // By its ending, not by the country its record gives: skyscanner.de
    // is Germany's site of a British company. An ending used worldwide
    // (.eu, .md) is no country's.
    let elsewhere = |hit: &Hit| {
        plumb_core::tld_country(&hit.domain)
            .is_some_and(|at| home != Some(at) && !asked.contains(&at))
    };
    let named_country =
        |hit: &Hit| plumb_core::tld_country(&hit.domain).is_some_and(|at| asked.contains(&at));
    let mut copies = vec![false; hits.len()];
    for (i, hit) in hits.iter().enumerate() {
        if typed == Some(hit.domain.as_str()) {
            continue;
        }
        let title = hit
            .title
            .as_deref()
            .map(normalize_text)
            .filter(|title| !title.is_empty());
        let same_title =
            |other: &Hit| title.is_some() && other.title.as_deref().map(normalize_text) == title;
        // The brand's site for another country, when the query is the
        // brand's name and its site on a generic ending is better known
        // (time.is, which "time" does not name, stays), or when official
        // and titled and described word for word as a better-known official
        // site of the brand: Wikidata gives Google Search a dozen addresses
        // (google.fr, google.ca, all "Google Search", "search engine by
        // Google LLC").
        copies[i] = brand_label(&hit.domain).is_some_and(|(label, suffix)| {
            country(suffix)
                && elsewhere(hit)
                && hits.iter().enumerate().any(|(j, other)| {
                    better_known(i, j)
                        && brand_label(&other.domain).is_some_and(|(l, s)| {
                            l == label
                                && ((hit.named && !country(s) && (other.official || !hit.official))
                                    || (hit.official
                                        && other.official
                                        && same_title(other)
                                        && hit.description.is_some()
                                        && other.description == hit.description))
                        })
                })
        });
        if copies[i] || hit.official || (home.is_some() && hit.country.as_deref() == home) {
            continue;
        }
        // Another site's title word for word, naming that site and not
        // this one: elephant-blue.org titled "Coinbase - Buy and Sell
        // Bitcoin…", however linked, short of a well-known site (live.com
        // titled "Outlook" is Microsoft's own).
        let names_other = |other: &Hit| {
            brand_label(&other.domain).is_some_and(|(label, _)| {
                label.len() >= 4 && names_brand(other, label) && !hit.domain.contains(label)
            })
        };
        let someone_elses = hit.link_score < WELL_KNOWN_LINK_SCORE && !is_own_name(hit);
        let description = hit.description.as_deref().map(normalize_text);
        let says_nothing_else = |other: &Hit| {
            description.is_none() || other.description.as_deref().map(normalize_text) == description
        };
        if hits.iter().enumerate().any(|(j, other)| {
            let better = other.link_score > hit.link_score;
            same_title(other)
                && ((better
                    && (hit.link_score < COPYCAT_LINK_SCORE
                        || (hit.link_score < WELL_KNOWN_LINK_SCORE && names_other(other))))
                    || ((better || (other.link_score == hit.link_score && j < i))
                        && someone_elses
                        && says_nothing_else(other)))
        }) {
            copies[i] = true;
            continue;
        }
        let Some((label, suffix)) = brand_label(&hit.domain).filter(|_| !named_country(hit)) else {
            continue;
        };
        // A copy is the brand again somewhere it says nothing of its own:
        // on a country's ending with a title naming the brand (airbnb.fr
        // next to airbnb.com), on a site nobody links to (quizlet.fun),
        // or with no title next to the brand's own titled site
        // (airbnb.tech). A site on a generic ending is never a copy of a
        // country's (honda.com of honda.com.vn, astro.build of
        // astro.com.my), and a site with no title is not a copy of a
        // namesake whose title does not name the brand (hm.com of hm.edu)
        // nor when it is well known: no title is then a gap in the crawl
        // (abc.net.au next to abc.com, weather.com next to weather.gov).
        let same_brand = |other: &Hit| {
            brand_label(&other.domain).is_some_and(|(l, s)| {
                l == label
                    && s != suffix
                    && (country(suffix) || !country(s))
                    && match &hit.title {
                        None => {
                            hit.link_score < WELL_KNOWN_LINK_SCORE
                                && other.title.is_some()
                                && names_brand(other, label)
                        }
                        Some(_) => {
                            (country(suffix) || hit.link_score < COPYCAT_LINK_SCORE)
                                && (names_brand(hit, label) || same_title(other))
                        }
                    }
            })
        };
        // An official namesake known no better: vanguard.edu (official)
        // is no reason to leave out vanguard.com, better linked, when the
        // crawl has no title for it to tell them apart by.
        let official = hits
            .iter()
            .any(|other| other.official && other.link_score >= hit.link_score && same_brand(other));
        let better = hits
            .iter()
            .enumerate()
            .any(|(j, other)| same_brand(other) && better_known(i, j));
        copies[i] = official || better;
    }
    hits.into_iter()
        .zip(copies)
        .filter_map(|(hit, copy)| (!copy).then_some(hit))
        .collect()
}

/// Which of `hits` are look-alikes of a well-known site `query` names
/// that is listed with them, wherever it ranks (gmail.com under
/// google.com for "gmail"). Little-known sites that are no one's
/// official one, and:
///
/// - Its label on another ending with nothing to say what it is (no
///   title or description): gmail.ru, gmail.co for "gmail", paypal.biz
///   for "paypal". A bare facebook.co under facebook.com only looks like
///   a trap. Country sites of the brand stay: they are well known
///   (amazon.de) or crawled (toyota.jp).
/// - Its label split by a hyphen, whatever its title says: po-ki.hu for
///   "poki", yout-ube.com for "youtube", chat-gpt.ru for "chatgpt".
/// - A query that is its name and words like "login", spelled as one
///   label: facebook-login.com and facebooklogin.com for "facebook
///   login", paypal-login.us for "paypal login". Not the name with other
///   words (googlemaps.com for "google maps" is Google's).
///
/// `typed`, the domain the query is, never is.
pub(crate) fn look_alikes(hits: &[Hit], query: &str, typed: Option<&str>) -> Vec<bool> {
    let label = |hit: &Hit| hit.domain.split('.').next().unwrap_or("").to_string();
    let joined =
        |label: &str| -> String { label.chars().filter(|c| !matches!(c, '-' | '_')).collect() };
    let brands: HashSet<String> = hits
        .iter()
        .filter(|hit| hit.named && hit.link_score >= WELL_KNOWN_LINK_SCORE)
        .map(label)
        .collect();
    // A query that ends in words like "login", spelled as one label.
    let spelled = without_intent_words(query).map(|_| normalize_text(query).replace(' ', ""));
    hits.iter()
        .map(|hit| {
            let own = label(hit);
            let bare = hit.title.is_none() && hit.description.is_none();
            !brands.is_empty()
                && hit.link_score < WELL_KNOWN_LINK_SCORE
                && !hit.official
                && typed != Some(hit.domain.as_str())
                && ((bare && brands.contains(&own))
                    || (joined(&own) != own && brands.contains(&joined(&own)))
                    || spelled.as_deref() == Some(joined(&own).as_str()))
        })
        .collect()
}

/// `hits` without the near-copies of better-known sites listed with them:
/// a site nobody links to ([`COPYCAT_LINK_SCORE`]) whose homepage text is
/// all but word for word that of a better-linked site in the list
/// ([`plumb_core::simhash::near_copies`] of their `fingerprints`, one per
/// hit): a scraped copy, a parked page on a template, a mirror under a
/// title of its own, which [`without_copies`] cannot tell by its title.
/// A little-known site goes too when its title is no name of its own
/// ([`is_own_name`]) and a site listed above it, as well linked, has its
/// text: one of a spam network's sites ("Beginner's Guide to Coding in
/// Python | Crest Center" and "| Harbor Workshop" on made-up names).
/// Official sites, the domain the query is (`typed`) and the searcher's
/// country's sites (`home`) always stay.
pub(crate) fn without_near_copies(
    hits: Vec<Hit>,
    fingerprints: &[Option<u64>],
    typed: Option<&str>,
    home: Option<&str>,
) -> Vec<Hit> {
    let copy = |i: usize| {
        let hit = &hits[i];
        let Some(Some(print)) = fingerprints.get(i) else {
            return false;
        };
        if hit.official
            || hit.link_score >= WELL_KNOWN_LINK_SCORE
            || typed == Some(hit.domain.as_str())
            || (home.is_some() && hit.country.as_deref() == home)
        {
            return false;
        }
        let someone_elses = !is_own_name(hit);
        hits.iter()
            .zip(fingerprints)
            .enumerate()
            .any(|(j, (other, other_print))| {
                let better = other.link_score > hit.link_score;
                (better && hit.link_score < COPYCAT_LINK_SCORE
                    || (better || (other.link_score == hit.link_score && j < i)) && someone_elses)
                    && other_print
                        .is_some_and(|other| plumb_core::simhash::near_copies(*print, other))
            })
    };
    let copies: Vec<bool> = (0..hits.len()).map(copy).collect();
    hits.into_iter()
        .zip(copies)
        .filter_map(|(hit, copy)| (!copy).then_some(hit))
        .collect()
}

/// Whether the title of `hit` is a name of its own: its address's label
/// is in the title, or the title is in the label ("Joe's Pizza" on
/// joespizzanyc.com, "XX88" on xx88s.com, "München" on münchen.de).
/// Spam sites under one title are mostly not ("INFORMATION40" on
/// bountystory.site), and namesakes titled alike are.
fn is_own_name(hit: &Hit) -> bool {
    let Some(title) = &hit.title else {
        return false;
    };
    let joined = |text: &str| normalize_text(text).replace(' ', "");
    let (title, label) = (
        joined(title),
        joined(&crate::schema::label_text(&hit.domain)),
    );
    !title.is_empty() && (title.contains(&label) || label.contains(&title))
}
