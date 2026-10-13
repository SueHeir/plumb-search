//! Listing pages with the sites a search found: under their site, first
//! when the query names them, or not at all.

use std::collections::HashSet;

use plumb_core::{
    adult_level, host_of, normalize_text, registrable_domain, AdultLevel, Operators, SafeSearch,
};
use tantivy::tokenizer::TextAnalyzer;

use super::docs::{docs_found_by_words, docs_kept_below};
use super::sites::{base_title, describes_an_organization, is_government, site_is_titled, squash};
use super::{
    LearnedPlace, Page, PageHit, PlacedPage, ASKING_WORDS, DOCS_SET, FILMS_SET, GITHUB_SET,
    PODCASTS_SET, REFERENCE2_SET, SUBPAGES_SET, WIKIDATA_SET,
};
use crate::analysis;

/// Most pages listed on their own among the sites.
pub const MAX_PAGES_LISTED: usize = 2;

/// Least score of a page the query does not name in full that is listed.
pub const MIN_PARTIAL_SCORE: f32 = 0.3;

/// Where a page the query only names in part is listed: after this many
/// sites.
pub const PARTIAL_AFTER: usize = 3;

/// How much more read ([`PageHit::popularity`]) than the article about
/// the best site's subject an article the query names must be to come
/// before that site ([`crate::Hit::demand`]): about twice the views on
/// English Wikipedia.
pub const DEMAND_MARGIN: f32 = 0.05;

/// The text match ([`crate::Hit::text_score`]) below which a site the
/// query does not name in full comes after an article the query names.
pub const WEAK_SITE_MATCH: f32 = 0.5;

/// Most site results looked through by [`lift_named_sites`].
const LIFTED_FROM: usize = 5;

/// How much better known than a site the whole query names the page's
/// official site must be to be put above it: global.toyota (0.62) stays
/// below toyota.com (0.47), aliexpress.com (0.75) goes above aliexpress.us
/// (0.41).
const LIFT_LINK_MARGIN: f32 = 0.2;

/// Leaves out the sites that lack some of the query's main words
/// ([`crate::Hit::missing_words`]) when the whole query is the name of an
/// article or Wikidata item: "toy story" names the film, so rawstory.com
/// and codastory.com, having one word of it, are no answer. The page's
/// own site stays. Returns how many were left out.
pub fn drop_namesakes_of_words(sites: &mut Vec<crate::Hit>, pages: &[PageHit]) -> usize {
    let Some(named) = pages
        .iter()
        .find(|hit| hit.named && hit.page.item.is_some())
    else {
        return 0;
    };
    let own = named.page.site.as_deref();
    let before = sites.len();
    sites.retain(|hit| !hit.missing_words || Some(hit.domain.as_str()) == own);
    before - sites.len()
}

/// Adds the official site of the best article the query names when the
/// sites found leave it out, as the [`LIFTED_FROM`]th site or last, so
/// that [`lift_named_sites`] can weigh it: "better call saul" names the
/// article whose site is amc.com, which no word of the query matches.
/// `site` gives the site of a domain, crawled or not: the page's site is
/// the item's official website in Wikidata, which is enough to list it
/// (adultswim.com for "rick and morty", never fetched on some nodes).
/// Returns whether it added one.
pub fn add_named_site(
    sites: &mut Vec<crate::Hit>,
    pages: &[PageHit],
    site: impl Fn(&str) -> Option<crate::Hit>,
) -> bool {
    let Some(domain) = pages
        .iter()
        .find(|hit| hit.named && hit.page.item.is_some())
        .and_then(|hit| hit.page.site.as_deref())
    else {
        return false;
    };
    if sites.is_empty() || sites.iter().any(|hit| hit.domain == domain) {
        return false;
    }
    let Some(mut added) = site(domain) else {
        return false;
    };
    let at = sites.len().min(LIFTED_FROM - 1);
    // Scored between its neighbours, so a later sort by score keeps it
    // there.
    let above = sites[at - 1].score;
    added.score = match sites.get(at) {
        Some(below) => (above + below.score) / 2.0,
        None => above - above.abs().max(1.0) * 1e-3,
    };
    sites.insert(at, added);
    true
}

/// Puts first the official site of the best page the query names, when
/// it is among the first [`LIFTED_FROM`] sites: what Wikipedia and
/// Wikidata call exactly what was searched for says which site it is.
/// "youtube music" names the article YouTube Music, whose site is
/// youtube.com, so youtube.com goes above youtube.de; "google maps" puts
/// google.com above googlemaps.com. An official site the whole query
/// names keeps first place.
///
/// The homepage of a repository the query names goes first too when the
/// query names that site as well: "awesome python" puts awesome-python.com,
/// with vinta/awesome-python under it, above python.org. Never crawled, it
/// would otherwise be taken for a mere spelling of the query.
pub fn lift_named_sites(sites: &mut [crate::Hit], pages: &[PageHit]) {
    let Some(at) = named_page_site(sites, pages.iter()) else {
        return;
    };
    // An official or well-known site named by all of the query stays
    // first: google.com for "google", not about.google, Google's own site
    // in Wikidata. So does any site named by all of it when the page's
    // site is too and is not far better known: both are names of what was
    // searched for, and the ranking already weighed them, so toyota.com
    // stays above global.toyota for "toyota". aliexpress.us, though, does
    // not stay above aliexpress.com, which far more sites link to.
    let about_as_known = sites[0].link_score + LIFT_LINK_MARGIN >= sites[at].link_score;
    if at > 0
        && sites[0].named
        && ((sites[at].named && about_as_known)
            || sites[0].official
            || sites[0].link_score >= crate::WELL_KNOWN_LINK_SCORE)
    {
        return;
    }
    sites[..=at].rotate_right(1);
}

/// The existing site corroborated by the best named entity page's
/// official-site identity, or by a named repository of a named site.
/// Used by both lifting and the learned relevance guard. Site titles,
/// which may be borrowed, never supply this identity; films do not either.
pub(crate) fn named_page_site<'a>(
    sites: &[crate::Hit],
    mut pages: impl Iterator<Item = &'a PageHit> + Clone,
) -> Option<usize> {
    let shown = sites.len().min(LIFTED_FROM);
    let lifted = |hit: &PageHit| {
        let site = hit.page.site.as_deref()?;
        let at = sites[..shown].iter().position(|s| s.domain == site)?;
        let ok = if hit.page.item.is_some() {
            sites[at].official
        } else {
            hit.page.set == GITHUB_SET && sites[at].named
        };
        ok.then_some(at)
    };
    let article = pages
        .clone()
        .find(|hit| hit.named && hit.page.item.is_some() && hit.page.set != FILMS_SET);
    let repo = pages.find(|hit| hit.named && hit.page.set == GITHUB_SET);
    article.and_then(lifted).or_else(|| repo.and_then(lifted))
}

/// Where `pages` (best first) go among the site results `sites`:
///
/// - A page about one of the sites (its [`Page::site`]) goes under that
///   site's result rather than in a place of its own: "python" lists
///   python.org with the article on the Python language under it.
/// - Of the others, at most [`MAX_PAGES_LISTED`] are listed: pages the
///   query names in full, and others scoring at least
///   [`MIN_PARTIAL_SCORE`]; only one when the best site is named by the
///   query too.
/// - A named page comes right after the best site when that site is
///   probably the website of what the query names: a named page found
///   for the query is about an organization or product
///   ([`describes_an_organization`]) and the site is called after it
///   ("tauri" finds the article "Tauri (software framework)", so tauri.app
///   stays first; "robinhood" finds "Robinhood Markets", so robinhood.com
///   stays above the outlaw). It also comes after an official website
///   ([`crate::Hit::official`]) or one better known than the page is read
///   ([`PageHit::popularity`] against [`crate::Hit::link_score`]).
///   Otherwise the page comes first: "marie curie" lists the article
///   before mariecurie.org. Pages named only in part come after
///   [`PARTIAL_AFTER`] sites, questions and papers with most of the
///   query's words after the best site.
/// - A page the query asks for as a whole ([`PageHit::whole`]: a question,
///   or a book with its author) comes first, unless the best site is
///   named by the query.
/// - A site called exactly what was searched for stays first when the
///   best page named so is an organization or a repository: "us bank"
///   lists usbank.com before the article "U.S. Bancorp".
///
/// A page the learned ranking listed on its own ([`PageHit::learned`])
/// stays where it put it: before the same site, or last, except that a
/// docs page found by its words never comes before the best site.
pub fn place_pages(query: &str, sites: &[crate::Hit], mut pages: Vec<PageHit>) -> Vec<PlacedPage> {
    pages.retain(|hit| {
        crate::health::page_allowed(query, &hit.page) && subpage_asked(query, sites, hit)
    });
    if !asks_for_podcasts(query) {
        // Podcasts it does not ask for take no other page's place
        // ([`keep_page_rules`]).
        pages.sort_by_key(|hit| hit.page.set == PODCASTS_SET);
    }
    let mut placed = place_pages_by_rules(query, sites, pages);
    for page in placed.iter_mut().filter(|p| p.under.is_none()) {
        match &page.hit.learned {
            Some(LearnedPlace::Before(domain)) => {
                if let Some(at) = sites.iter().position(|s| &s.domain == domain) {
                    page.at = at;
                }
            }
            Some(LearnedPlace::Last) => page.at = sites.len(),
            None => {}
        }
    }
    keep_page_rules(query, sites, &mut placed);
    placed
}

/// Best sites of a query looked at for the site of a subpage found by its
/// words ([`subpage_asked`]).
const SUBPAGE_SITES: usize = 3;

/// Least share of a subpage's title words a query found it by must have,
/// when it does not name the page's site.
const SUBPAGE_TITLE_SHARE: f32 = 0.75;

/// The same share when the title holds every word of the query.
const SUBPAGE_HELD_SHARE: f32 = 0.5;

/// Words a title shares with anything, left out of [`SUBPAGE_TITLE_SHARE`].
const TITLE_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "from", "in", "my", "of", "on", "or", "the", "to", "with",
    "you", "your",
];

/// Whether `hit`, when it is a subpage or staged reference page, may be listed for
/// `query`: named by its whole title, on one of the query's best sites
/// ("rotten tomatoes oppenheimer", "met museum hours"), or with most of
/// its title's words, or of its title before a colon, in the query
/// ("amazon leadership principles" for "Amazon Leadership Principles:
/// Values and company culture"). A big site's news, press releases and reviews
/// share words with all sorts of searches: "seven summits" is not a
/// leadership summit at jnj.com, nor "mount everest" a film review at
/// ign.com. Pages of other sets always may.
pub(crate) fn subpage_asked(query: &str, sites: &[crate::Hit], hit: &PageHit) -> bool {
    if (hit.page.set != SUBPAGES_SET && hit.page.set != REFERENCE2_SET) || hit.named {
        return true;
    }
    // Retrieval/options already constrain known language. A locale-like
    // path is only a fallback for older pages with no language metadata.
    let language = hit.page.language();
    if language.is_none() && in_other_language(&hit.page.url) {
        return false;
    }
    let domain = registrable_domain(&hit.page.url);
    if domain.is_some()
        && sites
            .iter()
            .take(SUBPAGE_SITES)
            .any(|site| registrable_domain(&site.domain) == domain)
    {
        return true;
    }
    static STEMS: std::sync::LazyLock<TextAnalyzer> =
        std::sync::LazyLock::new(analysis::stemmed_analyzer);
    let content_analyzer = language
        .filter(|language| *language != "en")
        .map(|language| analysis::content_analyzer(Some(language)));
    let words = |text: &str| -> Vec<String> {
        if let Some(analyzer) = &content_analyzer {
            return analysis::tokens(analyzer, text)
                .into_iter()
                .filter(|word| word.chars().count() > 1)
                .collect();
        }
        text.split(|c: char| !c.is_alphanumeric())
            // The "s" of "John's" says nothing.
            .filter(|word| word.chars().count() > 1)
            .map(str::to_lowercase)
            .filter(|word| {
                !ASKING_WORDS.contains(&word.as_str()) && !TITLE_STOP_WORDS.contains(&word.as_str())
            })
            .flat_map(|word| analysis::tokens(&STEMS, &word))
            .collect()
    };
    let asked: HashSet<String> = words(query).into_iter().collect();
    let fewest = asked.len().min(2);
    if fewest == 0 {
        return false;
    }
    let has_most = |title: &str| {
        let in_order = words(title);
        let title: HashSet<String> = in_order.iter().cloned().collect();
        // A year the query asks for and the title lacks is another page:
        // "nobel prize in physics 2025" is not "A Nobel prize for particle
        // physics".
        if asked
            .iter()
            .any(|word| is_year(word) && !title.contains(word))
        {
            return false;
        }
        let shared = title.intersection(&asked).count();
        // A query the title holds whole needs only half of it: "john
        // martinis" for "John Martinis - CHM".
        // The relaxed threshold requires the query's terms together:
        // "seven summits" is not "The Seven Second Summits of the World".
        let held = in_order.windows(asked.len()).any(|run| {
            run.iter().all(|word| asked.contains(word))
                && run.iter().collect::<HashSet<_>>().len() == asked.len()
        });
        let share = if held {
            SUBPAGE_HELD_SHARE
        } else {
            SUBPAGE_TITLE_SHARE
        };
        shared >= fewest && shared as f32 >= share * title.len() as f32
    };
    // Or of its title before a subtitle, "Declaration of Independence" in
    // "Declaration of Independence: A Transcription", or before the site's
    // name, "Rule 30" in "Rule 30 -- from Wolfram MathWorld". Not before a
    // dash: "The Great Gatsby - On Broadway" is the show.
    let title = hit.page.title.as_str();
    has_most(title)
        || [":", " -- "].iter().any(|mark| {
            title
                .split_once(mark)
                .is_some_and(|(head, _)| has_most(head))
        })
}

/// Whether `word` is a year: four digits from 1000 to 2999.
fn is_year(word: &str) -> bool {
    word.len() == 4
        && word.bytes().all(|b| b.is_ascii_digit())
        && matches!(word.as_bytes()[0], b'1' | b'2')
}

/// Languages a site's pages in another language than English are under
/// (`https://www.iso.org/ru/...`, `/de-de/`). "uk" is left out: it is the
/// UK's pages as often as Ukrainian ones.
const OTHER_LANGUAGES: &[&str] = &[
    "ar", "bg", "cs", "da", "de", "el", "es", "fa", "fi", "fr", "he", "hi", "hu", "id", "it", "ja",
    "ko", "nl", "no", "pl", "pt", "ro", "ru", "sk", "sv", "th", "tr", "vi", "zh",
];

/// Whether `url`'s path starts with another language than English:
/// `https://www.iso.org/ru/iso-4217-currency-codes.html`.
fn in_other_language(url: &str) -> bool {
    let path = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split_once('/')
        .map_or("", |(_, path)| path);
    let first = path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let language = first.split(['-', '_']).next().unwrap_or("");
    first.len() <= 5 && OTHER_LANGUAGES.contains(&language)
}

/// The places no ranking overrides, applied after [`place_pages`] and
/// after [`crate::learned::reorder`]: a docs page found by its words never
/// comes before the best site unless that site is the docs' own (see
/// [`docs_kept_below`]), and, unless the query asks for them,
/// podcasts come after the best site and the other pages listed on their
/// own ("better call saul" wants amc.com and the article first).
pub fn keep_page_rules(query: &str, sites: &[crate::Hit], placed: &mut Vec<PlacedPage>) {
    placed.retain(|page| crate::health::page_allowed(query, &page.hit.page));
    for page in placed.iter_mut().filter(|p| p.under.is_none()) {
        if docs_kept_below(&page.hit, sites) {
            page.at = page.at.max(1).min(sites.len());
        }
    }
    if asks_for_podcasts(query) {
        return;
    }
    let others = placed
        .iter()
        .filter(|p| p.under.is_none() && p.hit.page.set != PODCASTS_SET)
        .map(|p| p.at)
        .max()
        .unwrap_or(0);
    let (podcasts, mut rest): (Vec<PlacedPage>, Vec<PlacedPage>) = std::mem::take(placed)
        .into_iter()
        .partition(|p| p.under.is_none() && p.hit.page.set == PODCASTS_SET);
    for mut page in podcasts {
        page.at = page.at.max(others).max(1).min(sites.len());
        rest.push(page);
    }
    *placed = rest;
}

/// Whether `query` asks for podcasts or episodes.
fn asks_for_podcasts(query: &str) -> bool {
    query.split_whitespace().any(|word| {
        matches!(
            word.to_lowercase().as_str(),
            "podcast" | "podcasts" | "episode" | "episodes"
        )
    })
}

fn place_pages_by_rules(query: &str, sites: &[crate::Hit], pages: Vec<PageHit>) -> Vec<PlacedPage> {
    let site_named = sites.first().is_some_and(|hit| hit.named);
    let query_word = squash(query);
    let organizations_site = sites.first().is_some_and(|site| {
        let label = squash(site.domain.split('.').next().unwrap_or(""));
        // A site of government called after the page and named by the
        // query: fafsa.gov for "fafsa", not ada.gov (the Americans with
        // Disabilities Act) for "ada lovelace".
        let government = site.named && is_government(&site.domain);
        // The site is called what was searched for, and the best page
        // named so is an organization or a project: usbank.com for "us
        // bank" (the article "U.S. Bancorp"), regex101.com for the
        // repository firasdib/Regex101.
        let called = label == query_word
            && pages.iter().find(|page| page.named).is_some_and(|page| {
                page.page.set == GITHUB_SET
                    || describes_an_organization(page.page.description.as_deref())
            });
        called
            || pages.iter().any(|page| {
                page.named
                    && site_is_titled(&site.domain, &page.page.title)
                    && (government
                    || describes_an_organization(page.page.description.as_deref())
                    // "robinhood" spells robinhood.com, not "Robin Hood".
                    || (!query.trim().contains(' ')
                        && label == query_word
                        && base_title(&page.page.title).contains(' ')))
            })
    });
    // A named page goes first only when the best site may be a namesake:
    // one less known than the page is read, or, when Wikipedia has an
    // article about what the site is, one whose article is read far less
    // than the page. What people look up decides between namesakes, not
    // how big the site is: "mars" means the planet, read many times more
    // than Mars Inc. of mars.com; "napoleon" the emperor, not Napoleon,
    // North Dakota.
    // When the site and an article are both called just what was searched
    // for, the article is the one Wikipedia gives that name to, so being
    // read more at all is enough ("password manager" still means a site): on
    // plumbsearch.org the planet "Mars" was read 1.7 times as much as Mars
    // Inc. that day.
    // A site that matches the query only a little is no namesake at all:
    // the article "Nikola Tesla" before tesla.com, "Genghis Khan" before
    // khanacademy.org. One the query names by an official name or as a
    // kind of thing ("veterans affairs", "search engine") matches it in
    // full and is not passed over.
    let page_first = |page: &PageHit| match sites.first() {
        None => true,
        Some(_) if organizations_site => false,
        Some(site)
            if !site.named
                && site.placing_text_score.unwrap_or(site.text_score) < WEAK_SITE_MATCH
                && page.page.is_article() =>
        {
            true
        }
        Some(site) => match site.demand {
            Some(demand) if page.page.is_article() => {
                let margin = if site.named && squash(&page.page.title) == query_word {
                    0.0
                } else {
                    DEMAND_MARGIN
                };
                page.popularity > demand + margin
            }
            _ => !site.official && page.popularity > site.link_score,
        },
    };
    let most = if site_named { 1 } else { MAX_PAGES_LISTED };
    let mut placed: Vec<PlacedPage> = Vec::new();
    let mut listed = 0;
    for hit in pages {
        // A page named like a better one of its set already listed is a
        // namesake of it: once "Eiffel Tower" is under toureiffel.paris, "Eiffel Tower
        // (Six Flags)" only comes after three sites.
        let namesake = hit.named
            && placed.iter().any(|p| {
                p.hit.named
                    && p.hit.page.set == hit.page.set
                    && page_key(base_title(&p.hit.page.title))
                        == page_key(base_title(&hit.page.title))
            });
        if let Some(site) = hit.page.site.as_deref() {
            if sites.iter().any(|s| s.domain == site) {
                // The best page about the site goes under it, unless a
                // later one is named by the query: "google maps" carries
                // Google Maps under google.com, not Google.
                match placed.iter_mut().find(|p| p.under.as_deref() == Some(site)) {
                    None => placed.push(PlacedPage {
                        under: Some(site.to_string()),
                        at: 0,
                        hit,
                    }),
                    Some(carried) if hit.named && !carried.hit.named => carried.hit = hit,
                    Some(_) => {}
                }
                continue;
            }
        }
        if obscure_namesake(&hit, &placed) {
            continue;
        }
        // An item without an article is only told apart from its
        // namesakes by its website.
        if hit.page.set == WIKIDATA_SET {
            continue;
        }
        // One docs page found by its words is enough: more crowd out the
        // questions that answer the same search.
        let docs_found = |p: &PlacedPage| p.hit.page.set == DOCS_SET && !p.hit.named;
        if docs_found_by_words(&hit) && placed.iter().any(docs_found) {
            continue;
        }
        // The first docs page found by its words is listed even when
        // questions took the places for pages: "react usestate hook" wants
        // React's page as well as the questions on it.
        if listed >= most && !docs_found_by_words(&hit)
            || !(hit.named || hit.score >= MIN_PARTIAL_SCORE)
        {
            continue;
        }
        let at = if docs_found_by_words(&hit) {
            // A docs page found by its words comes after the best site:
            // "docker desktop download" wants docker.com first.
            1
        } else if hit.whole {
            // Only the best of them leads: another edition of the book
            // comes after the best site.
            let led = placed.iter().any(|p| p.hit.whole);
            usize::from(site_named || led)
        } else if !hit.named && hit.page.topic().is_some() {
            // A question or paper with most of the query's words: the best
            // question leads a search asked as a question that names no
            // site, as "how to unclog a drain" wants the answer before a
            // site that shares a word with it (cityofdrain.org). A search
            // that only names something ("irs refund status") keeps its
            // site first.
            // A reference page leads too when the best site matches the
            // query only a little: "foul smelling stool" wants the symptom
            // page, not a site called "stool".
            let led = placed.iter().any(|p| {
                p.at == 0
                    && p.under.is_none()
                    && (p.hit.page.is_question() || p.hit.page.is_site_page())
            });
            let weak_site = sites.first().is_some_and(|site| {
                !site.named && site.placing_text_score.unwrap_or(site.text_score) < WEAK_SITE_MATCH
            });
            let leads = !site_named
                && !led
                && (hit.page.is_question() && asked_as_question(query)
                    || hit.page.is_site_page() && (asked_as_question(query) || weak_site));
            usize::from(!leads)
        } else if !hit.named || namesake {
            PARTIAL_AFTER
        } else if hit.page.may_lead() && page_first(&hit) {
            0
        } else {
            1
        };
        listed += 1;
        placed.push(PlacedPage {
            at: at.min(sites.len()),
            under: None,
            hit,
        });
    }
    placed
}

/// How many times as read as a namesake an article that has the name to
/// itself is when the namesake is not listed ([`obscure_namesake`]).
const OBSCURE_NAMESAKE: u64 = 50;

/// Whether `hit` is an article the query names only by its title before a
/// qualifier, read less than a fiftieth as much as an article of its set
/// already placed that has the name to itself: "Prince William (1788
/// ship)" and "Prince William (TV series)" for "prince william", "Lady Gaga
/// (song)" for "lady gaga". Named so, it scored nearly as high as the
/// article, popularity being only half a page's score, and took a place
/// among the first results. "Cleopatra (1963 film)" and "Julius Caesar
/// (play)" are read enough to stay.
fn obscure_namesake(hit: &PageHit, placed: &[PlacedPage]) -> bool {
    let unqualified = |page: &Page| base_title(&page.title) == page.title;
    hit.named
        && hit.page.is_article()
        && !unqualified(&hit.page)
        && placed.iter().any(|p| {
            p.hit.named
                && p.hit.page.set == hit.page.set
                && unqualified(&p.hit.page)
                && hit.page.views.saturating_mul(OBSCURE_NAMESAKE) < p.hit.page.views
        })
}

/// Words a search asked as a question starts with.
const QUESTION_WORDS: &[&str] = &[
    "how", "why", "what", "whats", "what's", "when", "where", "which", "who", "can", "could",
    "should", "do", "does", "did", "is", "are", "will", "would",
];

/// Whether `query` is asked as a question: "how to unclog a drain", "can
/// you freeze cooked rice".
pub(crate) fn asked_as_question(query: &str) -> bool {
    query
        .split_whitespace()
        .next()
        .is_some_and(|word| QUESTION_WORDS.contains(&word.to_lowercase().as_str()))
}

/// Pages looked at for a query with search operators, before they narrow
/// them.
pub const OPERATOR_PAGES: usize = 100;

/// Most pages listed for a `site:` query.
pub const MAX_SITE_PAGES: usize = 10;

/// Whether the search operators `ops` allow `page`: its address's host
/// and its title, description and other names.
pub fn operators_allow(ops: &Operators, page: &Page) -> bool {
    let host = host_of(&page.url).unwrap_or_default();
    let texts = [page.title.as_str()]
        .into_iter()
        .chain(page.description.as_deref())
        .chain(page.aliases.iter().map(String::as_str))
        .chain(
            page.paper
                .iter()
                .flat_map(|p| p.authors.iter().map(String::as_str)),
        );
    ops.allows(&host, texts)
}

/// Whether the searcher's `options` allow `page`: one in another
/// language than [`crate::SearchOptions::language`] is left out, and
/// strict safe search leaves out pages whose title or description is
/// suggestive.
pub fn options_allow(options: &crate::SearchOptions, page: &Page) -> bool {
    if let Some(wanted) = &options.language {
        let Some(wanted) = plumb_core::language_code(wanted) else {
            return false;
        };
        if Some(wanted.as_str()) != page.language()
            && !(wanted == "en" && page.language().is_none())
        {
            return false;
        }
    }
    if options.safe == SafeSearch::Strict {
        let texts = [page.title.as_str()]
            .into_iter()
            .chain(page.description.as_deref());
        if adult_level("", texts) != AdultLevel::None {
            return false;
        }
    }
    true
}

/// [`place_pages`] for a query with search operators `ops`, of the pages
/// they allow. A `site:` query lists up to [`MAX_SITE_PAGES`] pages on
/// that site after the sites, best first: "site:wikipedia.org einstein"
/// lists the articles.
pub fn place_operator_pages(
    ops: &Operators,
    sites: &[crate::Hit],
    pages: Vec<PageHit>,
) -> Vec<PlacedPage> {
    let pages = pages
        .into_iter()
        .filter(|hit| operators_allow(ops, &hit.page));
    if ops.sites.is_empty() {
        return place_pages(&ops.words, sites, pages.collect());
    }
    pages
        .take(MAX_SITE_PAGES)
        .map(|hit| PlacedPage {
            hit,
            under: None,
            at: sites.len(),
        })
        .collect()
}

/// Normalized form of `text` as page keys compare it, for tests and tools.
pub fn page_key(text: &str) -> String {
    normalize_text(text).replace(' ', "")
}
