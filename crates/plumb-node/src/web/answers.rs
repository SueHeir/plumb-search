//! What a results page shows besides the results: an instant answer above
//! them ([`plumb_answer`]: sums, conversions, the time somewhere) and an
//! info box beside them, about the one thing the query names.
//!
//! Both are built from what the node holds. The only thing fetched for
//! them is the European Central Bank's daily currency rates, once a
//! currency conversion is asked for and then at most every few hours; the
//! request carries nothing of the query.
//!
//! The info box is about the Wikipedia article the results already list
//! for the query (see [`plumb_index::pages::place_pages`]), when the query
//! names it in full and it is listed first, right after the best site, or
//! under it as the article about that site: "albert einstein", "eiffel
//! tower", "github". It shows the article's short description, the
//! official site with the country this node knows it for, and links to
//! Wikipedia and Wikidata. Nothing in it is loaded from elsewhere.
//!
//! When the article's lead is held (`plumb fetch-leads`), the box shows
//! its first sentences too, and a query that asks what something is
//! ("what is a manatee", "define photosynthesis", "who was ada lovelace")
//! is answered above the results with the first sentence of the article
//! the query names ([`definition_answer`]).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use plumb_answer::{Answer, Rates, ECB_RATES_URL};
use plumb_core::profiles::{services_asked, shown_profiles};
use plumb_core::truncate_chars;
use plumb_index::pages::{Page, PlacedPage, FILMS_SET, MUSIC_SET, WIKIDATA_SET};
use plumb_index::Hit;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::{escape_html, homepage_url, http_url};
use crate::country::country_name;

/// Rates older than this are fetched again when next needed.
const RATES_FRESH: Duration = Duration::from_secs(6 * 3600);
/// After a failed fetch, how long to answer without rates.
const RATES_RETRY: Duration = Duration::from_secs(10 * 60);
/// Longest wait for the rates while a results page waits on them.
const RATES_TIMEOUT: Duration = Duration::from_secs(4);

/// The day's currency rates, fetched when first needed.
#[derive(Default)]
pub(crate) struct RatesCache {
    state: Mutex<RatesState>,
}

#[derive(Default)]
struct RatesState {
    rates: Option<(Rates, Instant)>,
    failed: Option<Instant>,
}

impl RatesCache {
    /// The rates, if `query` may need them and they can be had.
    pub(crate) async fn for_query(&self, query: &str) -> Option<Rates> {
        if !plumb_answer::may_need_rates(query) {
            return None;
        }
        let mut state = self.state.lock().await;
        let fresh = |at: &Instant| at.elapsed() < RATES_FRESH;
        match &state.rates {
            Some((rates, at)) if fresh(at) => return Some(rates.clone()),
            _ => {}
        }
        if state.failed.is_some_and(|at| at.elapsed() < RATES_RETRY) {
            return state.rates.as_ref().map(|(rates, _)| rates.clone());
        }
        match fetch_rates().await {
            Ok(rates) => {
                debug!("currency rates of {} fetched", rates.date);
                state.rates = Some((rates.clone(), Instant::now()));
                state.failed = None;
                Some(rates)
            }
            Err(err) => {
                warn!("could not fetch currency rates: {err}");
                state.failed = Some(Instant::now());
                // Older rates beat none; the answer says what day they are of.
                state.rates.as_ref().map(|(rates, _)| rates.clone())
            }
        }
    }
}

async fn fetch_rates() -> anyhow::Result<Rates> {
    let client = reqwest::Client::builder()
        .timeout(RATES_TIMEOUT)
        .user_agent(concat!("plumb/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let text = client
        .get(ECB_RATES_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Rates::parse_ecb(&text).ok_or_else(|| anyhow::anyhow!("the rates file did not read"))
}

/// An instant answer, as a block above the results.
pub(crate) fn render_answer(out: &mut String, answer: &Answer) {
    let _ = write!(
        out,
        "<section class=\"ia\" aria-label=\"Answer\"><p class=\"iaq\">{}</p>\
         <p class=\"iaa\">{}</p>",
        escape_html(&answer.question),
        escape_html(&answer.answer)
    );
    if let Some(note) = &answer.note {
        let _ = write!(out, "<p class=\"m\">{}</p>", escape_html(note));
    }
    out.push_str("</section>\n");
}

/// What an info box shows, also given by `/api/search?full=1`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct InfoBox {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The first sentences of the Wikipedia article.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    /// The Wikipedia article, if it is about one (an item of the
    /// `wikidata` set has none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wikidata: Option<String>,
    /// The official site's domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// The official site's country, when this node knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Official profiles, one per service: the service's name and the
    /// address.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ShownProfile>,
}

/// An official profile as shown: "YouTube" and its address.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ShownProfile {
    pub service: &'static str,
    pub url: String,
    /// Its own account, rather than a listing (a film on IMDb).
    pub official: bool,
}

/// An official profile the query asks for ("mrbeast youtube"), shown first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ProfileAnswer {
    /// Whose it is: the article's title.
    pub of: String,
    pub service: &'static str,
    pub url: String,
    /// Its own account, rather than a listing (a film on IMDb).
    pub official: bool,
    /// Where the link comes from: "Wikidata", "MusicBrainz".
    pub source: &'static str,
    /// A search of the service for it, as no page of it is known: a
    /// song's lyrics searched on Genius.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub search: bool,
    /// The article, for the info box beside it.
    #[serde(skip)]
    pub page: Page,
}

/// What a results page shows besides the results.
#[derive(Debug, Clone, Default)]
pub(crate) struct Extras {
    pub answer: Option<Answer>,
    pub profile: Option<ProfileAnswer>,
    /// What the node's plugins found.
    pub plugins: Vec<crate::plugins::PluginResults>,
    /// What the node's plugins say about its own results, by address.
    pub plugin_notes: crate::plugins::ResultNotes,
    /// The token for the forms of the plugins' buttons, when the page is
    /// for the node's owner and some plugin has buttons.
    pub plugin_token: Option<String>,
    /// Links to the plugins this search fits but did not run.
    pub plugin_offers: Vec<crate::plugins::Offer>,
}

/// Whether a Wikipedia article lists the pages a name could mean rather
/// than being about one thing.
pub(super) fn is_disambiguation(title: &str, description: Option<&str>) -> bool {
    title.ends_with("(disambiguation)")
        || description.is_some_and(|d| {
            let d = d.to_lowercase();
            d.contains("disambiguation") || d.contains("topics referred to by the same term")
        })
}

/// Whether an info box can be about `page`: a Wikipedia article, a
/// Wikidata item with profiles and no article, or a film or show.
fn is_about_one_thing(page: &Page) -> bool {
    page.set.starts_with("wikipedia-") || page.set == WIKIDATA_SET || page.set == FILMS_SET
}

/// The info box for results `sites` and `pages` (as placed among them),
/// if one article is clearly what the query is about.
///
/// When the first result is a well-known or official site the query names
/// in full, and no article named by the query is listed above it, the box is about that site's article or none: "zoom" is
/// zoom.us, not the 2006 film called Zoom, and "tesla" is tesla.com, not
/// the band. The article about the site is listed under it, named by the
/// query or not ("Zoom Video Communications").
pub(crate) fn info_box(sites: &[Hit], pages: &[PlacedPage]) -> Option<InfoBox> {
    info_from_page(page_about(sites, pages)?, sites)
}

/// The Wikipedia article or Wikidata item that results `sites` and
/// `pages` (as placed among them) are clearly about, as [`info_box`]
/// picks it.
pub(crate) fn page_about<'a>(sites: &[Hit], pages: &'a [PlacedPage]) -> Option<&'a Page> {
    let top = sites.first();
    let top_site = top.map(|hit| hit.domain.as_str());
    // Unless an article named by the query is listed first, above every
    // site: then it is what the query is about ("marie curie").
    let page_leads = pages
        .iter()
        .any(|placed| placed.hit.named && placed.under.is_none() && placed.at == 0);
    let site_wins = !page_leads
        && top.is_some_and(|hit| {
            hit.named && (hit.official || hit.link_score >= plumb_index::WELL_KNOWN_LINK_SCORE)
        });
    let about_one_thing = |placed: &&PlacedPage| {
        let page = &placed.hit.page;
        is_about_one_thing(page)
            // A film or show only when asked for as one: "dune 2021".
            && (page.set != FILMS_SET || placed.hit.whole)
            && !is_disambiguation(&page.title, page.description.as_deref())
    };
    let under_top =
        |placed: &PlacedPage| placed.under.is_some() && placed.under.as_deref() == top_site;
    let placed = if site_wins {
        pages
            .iter()
            .filter(about_one_thing)
            .find(|placed| under_top(placed))?
    } else {
        // The best named article, wherever it is listed: a namesake never
        // stands in for it ("tim cook" is not the historian, "better call
        // saul" not the episode). It gets the box only when listed near
        // the top, alone or under the first site.
        let best = pages
            .iter()
            .filter(|placed| placed.hit.named)
            .filter(about_one_thing)
            .reduce(|best, placed| {
                if placed.hit.score > best.hit.score {
                    placed
                } else {
                    best
                }
            })?;
        let listed_high = match &best.under {
            Some(_) => under_top(best),
            None => best.at <= 1,
        };
        if !listed_high {
            return None;
        }
        best
    };
    Some(&placed.hit.page)
}

/// The info box about the Wikipedia article or Wikidata item `page`.
pub(crate) fn info_from_page(page: &Page, sites: &[Hit]) -> Option<InfoBox> {
    if !is_about_one_thing(page) {
        return None;
    }
    let article =
        if page.set == WIKIDATA_SET || (page.set == FILMS_SET && !page.is_film_with_article()) {
            None
        } else {
            Some(http_url(&page.url)?)
        };
    let site = page
        .site
        .clone()
        .filter(|site| homepage_url(site).is_some());
    let country = site.as_deref().and_then(|site| {
        sites
            .iter()
            .find(|hit| hit.domain == site)?
            .country
            .as_deref()
            .map(|code| country_name(code).to_string())
    });
    Some(InfoBox {
        title: page.title.clone(),
        description: page.description.clone().filter(|d| !d.trim().is_empty()),
        lead: article
            .as_ref()
            .and(page.lead.clone())
            .filter(|lead| !lead.trim().is_empty()),
        article,
        wikidata: page
            .item
            .as_deref()
            .filter(|item| {
                item.starts_with('Q')
                    && item.len() > 1
                    && item[1..].bytes().all(|b| b.is_ascii_digit())
            })
            .map(|item| format!("https://www.wikidata.org/wiki/{item}")),
        site,
        country,
        profiles: shown_profiles(&page.profiles)
            .into_iter()
            .map(|(service, url)| ShownProfile {
                service: service.name,
                url,
                official: service.official,
            })
            .collect(),
    })
}

/// What to search for to find whose profile `query` asks for: the words
/// before the service ("mrbeast" of "mrbeast youtube"), and for lyrics
/// those words as a song too, since a song is only found when asked for
/// as one ("bohemian rhapsody song" for "bohemian rhapsody lyrics").
pub(crate) fn profile_lookups(query: &str) -> Vec<String> {
    let Some((services, name)) = services_asked(query) else {
        return Vec::new();
    };
    let mut lookups = vec![name.clone()];
    if services.iter().any(|s| s.key == "genius-song") {
        lookups.push(format!("{name} song"));
    }
    lookups
}

/// The official profile `query` asks for ("mrbeast youtube", "valve
/// steam", "bohemian rhapsody lyrics"), when the words before the service
/// name a Wikipedia article or a song or album in `pages` (found for those
/// words) that has a profile there. A song with no page on Genius has its
/// lyrics searched for there, by its title and artist.
pub(crate) fn profile_answer(query: &str, pages: &[PlacedPage]) -> Option<ProfileAnswer> {
    let (services, _) = services_asked(query)?;
    pages
        .iter()
        .filter(|placed| {
            placed.hit.named
                && (is_about_one_thing(&placed.hit.page) || placed.hit.page.set == MUSIC_SET)
        })
        .find_map(|placed| {
            let page = &placed.hit.page;
            let source = if page.set == MUSIC_SET {
                "MusicBrainz"
            } else {
                "Wikidata"
            };
            // A handle before a channel id: services come in that order.
            let found = services.iter().find_map(|service| {
                page.profiles
                    .iter()
                    .filter(|p| p.service == service.key)
                    .find_map(|p| Some((*service, service.url(&p.id)?)))
            });
            if let Some((service, url)) = found {
                return Some(ProfileAnswer {
                    of: page.title.clone(),
                    service: service.name,
                    url,
                    official: service.official,
                    source,
                    search: false,
                    page: page.clone(),
                });
            }
            let genius = services.iter().find(|s| s.key == "genius-song")?;
            if !page.is_song() {
                return None;
            }
            // "Hey Jude The Beatles", the song's title and artist.
            let words = page.aliases.first().unwrap_or(&page.title);
            let words: String = url::form_urlencoded::byte_serialize(words.as_bytes()).collect();
            Some(ProfileAnswer {
                of: page.title.clone(),
                service: genius.name,
                url: format!("https://genius.com/search?q={words}"),
                official: false,
                source,
                search: true,
                page: page.clone(),
            })
        })
}

/// The profile asked for, as the first thing on the page.
pub(crate) fn render_profile(out: &mut String, profile: &ProfileAnswer, icon: Option<&str>) {
    let shown = plumb_core::display_url(&profile.url);
    let domain = plumb_core::registrable_domain(&profile.url).unwrap_or_default();
    let _ = writeln!(
        out,
        "<section class=\"pf\" aria-label=\"{kind}\"><a class=\"r\" href=\"{}\" \
         rel=\"noreferrer\"><span class=\"site\">{}<span class=\"sn\"><span class=\"dn\">{}</span>\
         <span class=\"u\">{}</span></span></span><span class=\"t\">{} on {}</span></a>\
         <p class=\"m\">{note}</p></section>",
        escape_html(&profile.url),
        super::site_badge(&domain, icon),
        escape_html(profile.service),
        escape_html(&shown),
        escape_html(&truncate_chars(&profile.of, 120)),
        escape_html(profile.service),
        kind = if profile.search {
            "Search"
        } else if profile.official {
            "Official profile"
        } else {
            "Listing"
        },
        note = if profile.search {
            format!("Searched for on {}", escape_html(profile.service))
        } else {
            format!(
                "{}, from {}",
                if profile.official {
                    "Official profile"
                } else {
                    "Listing"
                },
                profile.source
            )
        },
    );
}

/// The licence of Wikipedia's text, linked under a description taken from it.
const WIKIPEDIA_LICENCE: &str = "https://creativecommons.org/licenses/by-sa/4.0/";

/// The info box, as an `<aside>` beside the results.
pub(crate) fn render_info_box(out: &mut String, info: &InfoBox) {
    let _ = write!(
        out,
        "<aside class=\"ib\" aria-label=\"About {title}\"><h2>{title}</h2>",
        title = escape_html(&truncate_chars(&info.title, 120))
    );
    if let Some(description) = &info.description {
        let _ = write!(out, "<p class=\"ibd\">{}</p>", escape_html(description));
    }
    if let Some(lead) = &info.lead {
        let _ = write!(out, "<p class=\"ibx\">{}</p>", escape_html(lead));
    }
    let mut facts = String::new();
    if let Some(site) = &info.site {
        if let Some(href) = homepage_url(site) {
            let _ = write!(
                facts,
                "<dt>Official site</dt><dd><a href=\"{}\" rel=\"noreferrer\">{}</a></dd>",
                escape_html(&href),
                escape_html(site)
            );
        }
    }
    if let Some(country) = &info.country {
        let _ = write!(facts, "<dt>Country</dt><dd>{}</dd>", escape_html(country));
    }
    if !facts.is_empty() {
        let _ = write!(out, "<dl>{facts}</dl>");
    }
    // Its own accounts, then where it is listed (IMDb, MusicBrainz).
    for (official, label) in [(true, "Official profiles"), (false, "Listed on")] {
        let shown: Vec<&ShownProfile> = info
            .profiles
            .iter()
            .filter(|p| p.official == official)
            .collect();
        if shown.is_empty() {
            continue;
        }
        let _ = write!(out, "<ul class=\"ibp\" aria-label=\"{label}\">");
        for profile in shown {
            let _ = write!(
                out,
                "<li><a href=\"{}\" rel=\"noreferrer\">{}</a></li>",
                escape_html(&profile.url),
                escape_html(profile.service)
            );
        }
        out.push_str("</ul>");
    }
    // A description from a Wikipedia article is under its licence, which
    // asks for credit; Wikidata's are CC0.
    let licence = (info.article.is_some() && (info.description.is_some() || info.lead.is_some()))
        .then_some(WIKIPEDIA_LICENCE);
    let links: Vec<String> = [
        (info.article.as_deref(), "Wikipedia"),
        (licence, "CC BY-SA"),
        (info.wikidata.as_deref(), "Wikidata"),
    ]
    .into_iter()
    .filter_map(|(href, name)| {
        Some(format!(
            "<a href=\"{}\" rel=\"noreferrer\">{name}</a>",
            escape_html(href?)
        ))
    })
    .collect();
    if !links.is_empty() {
        let _ = write!(out, "<p class=\"ibl\">{}</p>", links.join(" &middot; "));
    }
    out.push_str("</aside>\n");
}

/// The pages of `pages` (found for a subject's words) whose facts are the
/// subject's, in the order to try them: the pages it names, then those
/// listed under the site it names, each about one thing and none a
/// disambiguation page.
pub(crate) fn fact_pages(pages: &[PlacedPage]) -> impl Iterator<Item = &PlacedPage> {
    let named = pages.iter().filter(|placed| placed.hit.named);
    let of_sites = pages
        .iter()
        .filter(|placed| !placed.hit.named && placed.under.is_some());
    named
        .chain(of_sites)
        .filter(|placed| is_about_one_thing(&placed.hit.page))
        .filter(|placed| {
            !is_disambiguation(
                &placed.hit.page.title,
                placed.hit.page.description.as_deref(),
            )
        })
}

/// The fact `asked` asks for, when the first page named by its subject
/// (in `pages`, found for the subject's words) that has one of its kinds
/// has it: "Canberra" for "capital of australia". Failing that, the
/// article listed under the site the subject names: "Apple Inc." under
/// apple.com for "ceo of apple", where the article named "Apple" is the
/// fruit. `now` (Unix seconds) works out an age.
pub(crate) fn fact_answer(
    asked: &plumb_core::facts::FactQuestion,
    pages: &[PlacedPage],
    now: u64,
) -> Option<plumb_answer::Answer> {
    use plumb_core::facts::FactKind;
    fact_pages(pages).find_map(|placed| {
        let page = &placed.hit.page;
        let kind = asked
            .kinds
            .iter()
            .copied()
            .find(|kind| page.facts.iter().any(|fact| fact.kind == *kind))?;
        let values: Vec<&str> = page
            .facts
            .iter()
            .filter(|fact| fact.kind == kind)
            .map(|fact| fact.value.as_str())
            .collect();
        let date_of = |kind: FactKind| {
            page.facts
                .iter()
                .find(|fact| fact.kind == kind)
                .and_then(|fact| plumb_core::facts::Date::parse(&fact.value))
        };
        let (question, answer, note) = if asked.age && kind == FactKind::Born {
            let born = date_of(FactKind::Born)?;
            match date_of(FactKind::Died) {
                Some(died) => (
                    format!("Age of {}", page.title),
                    format!("Died at {}", born.years_until(&died)?),
                    Some(format!("{} to {}", born.display(), died.display())),
                ),
                None => (
                    format!("Age of {}", page.title),
                    format!("{} years old", born.years_until(&date_from_unix(now))?),
                    Some(format!("Born {}", born.display())),
                ),
            }
        } else {
            let (answer, note) = fact_text(kind, &values)?;
            (kind.question(&page.title), answer, note)
        };
        let from = "from Wikidata";
        Some(plumb_answer::Answer {
            kind: plumb_answer::Kind::Fact,
            question,
            answer,
            note: Some(match note {
                Some(note) => format!("{note}, {from}"),
                None => "From Wikidata".to_string(),
            }),
        })
    })
}

/// Words before a name that ask what it is: "what is a" of "what is a
/// manatee".
const DEFINITION_LEADS: &[&str] = &[
    "what is a ",
    "what is an ",
    "what is the ",
    "what is ",
    "what are ",
    "what was ",
    "what were ",
    "who is ",
    "who was ",
    "who were ",
    "define ",
    "definition of ",
    "meaning of ",
    "tell me about ",
];

/// Words after a name that ask what it is: " definition" of
/// "photosynthesis definition".
const DEFINITION_TAILS: &[&str] = &[" definition", " meaning", " defined", " explained"];

/// The name `query` asks what it is: "manatee" of "what is a manatee?",
/// "photosynthesis" of "define photosynthesis". `None` for a query that
/// asks nothing so, or more than that ("what is the capital of france"
/// asks a fact, "what is my ip" about the searcher).
pub(crate) fn definition_asked(query: &str) -> Option<String> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches(['?', '.', '!']))
        .to_lowercase();
    let name = DEFINITION_LEADS
        .iter()
        .find_map(|lead| q.strip_prefix(lead))
        .or_else(|| {
            DEFINITION_TAILS
                .iter()
                .find_map(|tail| q.strip_suffix(tail))
        })?
        .trim();
    let words: Vec<&str> = name.split_whitespace().collect();
    // A name, not a question of its own.
    if words.is_empty()
        || words.len() > 6
        || words.iter().any(|word| {
            matches!(
                *word,
                "my" | "your"
                    | "i"
                    | "you"
                    | "we"
                    | "of"
                    | "in"
                    | "for"
                    | "to"
                    | "best"
                    | "difference"
            )
        })
    {
        return None;
    }
    Some(name.to_string())
}

/// The first sentence of the article that `pages` (found for the name a
/// query asks about, [`definition_asked`]) name, when its lead is held:
/// "The West Indian manatee is the largest surviving member of the order
/// Sirenia." Only a page the name names in full, about one thing and not
/// a disambiguation page, answers.
pub(crate) fn definition_answer(pages: &[PlacedPage]) -> Option<plumb_answer::Answer> {
    let placed = fact_pages(pages).find(|placed| placed.hit.named)?;
    let page = &placed.hit.page;
    if !page.is_article() {
        return None;
    }
    let lead = page.lead.as_deref()?;
    let sentence = plumb_core::article::first_sentence(lead).trim();
    if sentence.is_empty() {
        return None;
    }
    Some(plumb_answer::Answer {
        kind: plumb_answer::Kind::Definition,
        question: page.title.clone(),
        answer: sentence.to_string(),
        note: Some("From Wikipedia, CC BY-SA".to_string()),
    })
}

/// Whether `query` asks what a word means rather than what a thing is:
/// "define anadromous", "prioritize meaning". Such a query is answered
/// from Wiktionary first, others from Wikipedia first.
pub(crate) fn asks_word(query: &str) -> bool {
    let q = query.to_lowercase();
    q.split_whitespace()
        .any(|word| matches!(word, "define" | "definition" | "meaning" | "means" | "mean"))
}

/// What the Wiktionary word `page` means, as an answer: "(adjective) Of
/// fish, migrating up rivers from the sea to breed in fresh water."
pub(crate) fn word_answer(page: &Page) -> Option<plumb_answer::Answer> {
    let meaning = page.description.as_deref()?.trim();
    (!meaning.is_empty()).then(|| plumb_answer::Answer {
        kind: plumb_answer::Kind::Definition,
        question: page.title.clone(),
        answer: meaning.to_string(),
        note: Some("From Wiktionary, CC BY-SA".to_string()),
    })
}

/// A fact's values as shown, and a note: "27,204,809" and "counted in
/// 2024".
pub(crate) fn fact_text(
    kind: plumb_core::facts::FactKind,
    values: &[&str],
) -> Option<(String, Option<String>)> {
    use plumb_core::facts::{Date, ValueType};
    let first = *values.first()?;
    Some(match kind.value_type() {
        ValueType::Item => (join_names(values), None),
        ValueType::Time => (Date::parse(first)?.display(), None),
        ValueType::Quantity => {
            let (number, year) = first.split_once(';').unwrap_or((first, ""));
            let amount: f64 = number.parse().ok()?;
            match kind {
                plumb_core::facts::FactKind::Population => (
                    group_digits(amount.round(), 0),
                    (!year.is_empty()).then(|| format!("Counted in {year}")),
                ),
                plumb_core::facts::FactKind::Area => {
                    let km2 = amount / 1e6;
                    let digits = if km2 >= 100.0 { 0 } else { 2 };
                    (
                        format!(
                            "{} km² ({} sq mi)",
                            group_digits(km2, digits),
                            group_digits(km2 * 0.386_102, digits)
                        ),
                        None,
                    )
                }
                _ => {
                    let digits = if amount >= 100.0 {
                        usize::from(amount.fract() != 0.0) * 2
                    } else {
                        2
                    };
                    (
                        format!(
                            "{} m ({} ft)",
                            group_digits(amount, digits),
                            group_digits(amount * 3.280_84, 0)
                        ),
                        None,
                    )
                }
            }
        }
    })
}

/// "A", "A and B", "A, B and C".
fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `x` with `digits` decimals (trailing zeros dropped) and its whole part
/// in groups of three: "8,848.86", "27,204,809".
fn group_digits(x: f64, digits: usize) -> String {
    let text = format!("{:.*}", digits, x.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let fraction = fraction.trim_end_matches('0');
    let sign = if x < 0.0 { "-" } else { "" };
    if fraction.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{fraction}")
    }
}

/// The UTC date of Unix time `secs`.
fn date_from_unix(secs: u64) -> plumb_core::facts::Date {
    // Howard Hinnant's days-to-civil.
    let z = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    plumb_core::facts::Date {
        year: i32::try_from(year).unwrap_or(i32::MAX),
        month: u8::try_from(month).ok(),
        day: u8::try_from(day).ok(),
    }
}

#[cfg(test)]
mod tests {
    use plumb_index::pages::{Page, PageHit};

    use super::*;

    fn article(title: &str, description: &str, site: Option<&str>) -> PageHit {
        PageHit {
            page: Page {
                set: "wikipedia-en".to_string(),
                url: format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_")),
                title: title.to_string(),
                description: Some(description.to_string()),
                site: site.map(str::to_string),
                views: 1000,
                aliases: Vec::new(),
                item: Some("Q937".to_string()),
                profiles: Vec::new(),
                website: None,
                package: None,
                facts: Vec::new(),
                lead: None,
                names: Vec::new(),
            },
            score: 1.0,
            named: true,
            popularity: 0.9,
            whole: false,
            learned: None,
        }
    }

    fn site(domain: &str, country: Option<&str>) -> Hit {
        Hit {
            demand: None,
            missing_words: false,
            placing_text_score: None,
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score: 0.5,
            country: country.map(str::to_string),
            named: false,
            official: false,
            key_pages: Vec::new(),
        }
    }

    fn placed(hit: PageHit, under: Option<&str>, at: usize) -> PlacedPage {
        PlacedPage {
            hit,
            under: under.map(str::to_string),
            at,
        }
    }

    fn with_facts(mut hit: PageHit, facts: &[(plumb_core::facts::FactKind, &str)]) -> PageHit {
        hit.page.facts = facts
            .iter()
            .map(|(kind, value)| plumb_core::facts::Fact {
                kind: *kind,
                value: value.to_string(),
            })
            .collect();
        hit
    }

    #[test]
    fn definitions_answer_what_something_is() {
        assert_eq!(
            definition_asked("What is a manatee?").as_deref(),
            Some("manatee")
        );
        assert_eq!(
            definition_asked("define photosynthesis").as_deref(),
            Some("photosynthesis")
        );
        assert_eq!(
            definition_asked("entropy definition").as_deref(),
            Some("entropy")
        );
        assert_eq!(
            definition_asked("who was ada lovelace").as_deref(),
            Some("ada lovelace")
        );
        assert_eq!(definition_asked("what is my ip"), None);
        assert_eq!(definition_asked("what is the capital of france"), None);
        assert_eq!(definition_asked("manatee"), None);
        let mut manatee = article("West Indian manatee", "species of mammal", None);
        manatee.page.lead = Some(
            "The West Indian manatee is the largest surviving member of the order Sirenia. It lives in shallow waters."
                .into(),
        );
        let answer = definition_answer(&[placed(manatee.clone(), None, 1)]).unwrap();
        assert_eq!(answer.question, "West Indian manatee");
        assert_eq!(
            answer.answer,
            "The West Indian manatee is the largest surviving member of the order Sirenia."
        );
        // Only an article the name names in full answers.
        let mut partly = manatee.clone();
        partly.named = false;
        assert_eq!(definition_answer(&[placed(partly, None, 3)]), None);
        // Nor one without a lead.
        assert_eq!(
            definition_answer(&[placed(article("Manatee", "genus", None), None, 1)]),
            None
        );
        // The info box shows the lead, under Wikipedia's licence.
        let info = info_from_page(&manatee.page, &[]).unwrap();
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(html.contains("<p class=\"ibx\">The West Indian manatee is"));
        assert!(html.contains("CC BY-SA"));
    }

    #[test]
    fn words_answer_what_they_mean() {
        assert!(asks_word("define anadromous"));
        assert!(asks_word("prioritize meaning"));
        assert!(!asks_word("what is a manatee"));
        let word = Page::from_word(plumb_core::Article {
            title: "anadromous".into(),
            description: Some(
                "(adjective) Of fish, migrating up rivers from the sea to breed in fresh water."
                    .into(),
            ),
            views: 3,
            ..Default::default()
        });
        let answer = word_answer(&word).unwrap();
        assert_eq!(answer.question, "anadromous");
        assert!(answer.answer.starts_with("(adjective) Of fish"));
        assert_eq!(answer.note.as_deref(), Some("From Wiktionary, CC BY-SA"));
    }

    #[test]
    fn facts_answer_the_questions_that_ask_them() {
        use plumb_core::facts::{fact_asked, FactKind::*};
        let pages = [
            placed(
                article("Australia (disambiguation)", "may refer to", None),
                None,
                0,
            ),
            placed(
                with_facts(
                    article("Australia", "country in Oceania", None),
                    &[
                        (Capital, "Canberra"),
                        (Population, "27204809;2024"),
                        (Area, "7688287000000"),
                    ],
                ),
                None,
                1,
            ),
        ];
        let ask = |q: &str, pages: &[PlacedPage]| {
            fact_answer(&fact_asked(q).unwrap(), pages, 1_791_244_800)
        };
        let capital = ask("capital of australia", &pages).unwrap();
        assert_eq!(capital.question, "Capital of Australia");
        assert_eq!(capital.answer, "Canberra");
        assert_eq!(capital.note.as_deref(), Some("From Wikidata"));
        let people = ask("australia population", &pages).unwrap();
        assert_eq!(people.answer, "27,204,809");
        assert_eq!(
            people.note.as_deref(),
            Some("Counted in 2024, from Wikidata")
        );
        assert_eq!(
            ask("how big is australia", &pages).unwrap().answer,
            "7,688,287 km² (2,968,463 sq mi)"
        );
        // A kind it has no fact of: no answer.
        assert_eq!(ask("australia currency", &pages), None);

        // The article named "Apple" is the fruit; Apple Inc. is listed
        // under apple.com.
        let unnamed = |mut hit: PageHit| {
            hit.named = false;
            hit
        };
        let apple = [
            placed(article("Apple", "fruit", None), None, 1),
            placed(
                unnamed(with_facts(
                    article("Apple Inc.", "American technology company", None),
                    &[(Ceo, "Tim Cook")],
                )),
                Some("apple.com"),
                0,
            ),
            placed(
                unnamed(with_facts(
                    article("Apple Records", "record label", None),
                    &[(Ceo, "Someone")],
                )),
                None,
                2,
            ),
        ];
        let ceo = ask("who is the ceo of apple", &apple).unwrap();
        assert_eq!(
            (ceo.question.as_str(), ceo.answer.as_str()),
            ("CEO of Apple Inc.", "Tim Cook")
        );

        let everest = [placed(
            with_facts(
                article("Mount Everest", "mountain", None),
                &[(Elevation, "8848.86")],
            ),
            None,
            0,
        )];
        assert_eq!(
            ask("how tall is mount everest", &everest).unwrap().answer,
            "8,848.86 m (29,032 ft)"
        );

        let einstein = [placed(
            with_facts(
                article("Albert Einstein", "physicist", None),
                &[(Born, "1879-03-14"), (Died, "1955-04-18")],
            ),
            None,
            0,
        )];
        assert_eq!(
            ask("when was albert einstein born", &einstein)
                .unwrap()
                .answer,
            "March 14, 1879"
        );
        let age = ask("how old is albert einstein", &einstein).unwrap();
        assert_eq!(age.answer, "Died at 76");
        // 2026-10-06 is when the test's "now" is.
        let musk = [placed(
            with_facts(
                article("Elon Musk", "businessman", None),
                &[(Born, "1971-06-28")],
            ),
            None,
            0,
        )];
        assert_eq!(
            ask("how old is elon musk", &musk).unwrap().answer,
            "55 years old"
        );

        let tesla = [placed(
            with_facts(
                article("Tesla, Inc.", "carmaker", None),
                &[
                    (Founder, "Martin Eberhard"),
                    (Founder, "Marc Tarpenning"),
                    (Founder, "Elon Musk"),
                ],
            ),
            None,
            0,
        )];
        assert_eq!(
            ask("who founded tesla", &tesla).unwrap().answer,
            "Martin Eberhard, Marc Tarpenning and Elon Musk"
        );
        // A page not named by the subject is no answer.
        let mut unnamed = tesla.clone();
        unnamed[0].hit.named = false;
        assert_eq!(ask("who founded tesla", &unnamed), None);
    }

    #[test]
    fn dates_of_unix_times() {
        assert_eq!(date_from_unix(0).write(), "1970-01-01");
        assert_eq!(date_from_unix(1_791_244_800).write(), "2026-10-06");
        assert_eq!(date_from_unix(951_782_400).write(), "2000-02-29");
    }

    #[test]
    fn boxes_the_article_listed_first() {
        let pages = [placed(
            article("Albert Einstein", "German-born physicist", None),
            None,
            0,
        )];
        let info = info_box(&[site("einstein.org", None)], &pages).unwrap();
        assert_eq!(info.title, "Albert Einstein");
        assert_eq!(
            info.wikidata.as_deref(),
            Some("https://www.wikidata.org/wiki/Q937")
        );
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(html.contains("<h2>Albert Einstein</h2>"));
        assert!(html.contains("German-born physicist"));
        assert!(html.contains("href=\"https://en.wikipedia.org/wiki/Albert_Einstein\""));
        assert!(html.contains(
            "Wikipedia</a> &middot; <a href=\"https://creativecommons.org/licenses/by-sa/4.0/\" \
             rel=\"noreferrer\">CC BY-SA</a>"
        ));
    }

    #[test]
    fn boxes_the_article_about_the_top_site() {
        let pages = [placed(
            article(
                "GitHub",
                "Software development platform",
                Some("github.com"),
            ),
            Some("github.com"),
            0,
        )];
        let info = info_box(&[site("github.com", Some("US"))], &pages).unwrap();
        assert_eq!(info.site.as_deref(), Some("github.com"));
        assert_eq!(info.country.as_deref(), Some("United States"));
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(html.contains("<dt>Official site</dt><dd><a href=\"https://github.com/\""));
    }

    #[test]
    fn a_named_well_known_site_takes_the_box_from_its_namesakes() {
        let mut zoom = site("zoom.us", Some("US"));
        zoom.named = true;
        zoom.official = true;
        let film = placed(article("Zoom (2006 film)", "Film", None), None, 1);
        let mut company = article(
            "Zoom Video Communications",
            "Video conferencing company",
            Some("zoom.us"),
        );
        company.named = false;
        let company = placed(company, Some("zoom.us"), 0);
        let info = info_box(&[zoom.clone()], &[film.clone(), company]).unwrap();
        assert_eq!(info.title, "Zoom Video Communications");
        // Better no box than the film.
        assert_eq!(info_box(&[zoom.clone()], &[film]), None);
        // An article listed above every site is still what was searched.
        let curie = placed(article("Zoom", "Physicist", None), None, 0);
        assert_eq!(info_box(&[zoom], &[curie]).unwrap().title, "Zoom");
    }

    #[test]
    fn namesakes_never_stand_in_for_the_best_named_article() {
        let sites = [site("amc.com", None), site("apple.com", Some("US"))];
        let mut episode = article("Better Call Saul (Breaking Bad)", "Episode", None);
        episode.score = 0.8;
        let series = article("Better Call Saul", "Television series", Some("amc.com"));
        let pages = [placed(episode, None, 0), placed(series, Some("amc.com"), 0)];
        assert_eq!(info_box(&sites, &pages).unwrap().title, "Better Call Saul");
        // The best article is about a site further down: no box, rather
        // than the historian.
        let mut historian = article("Tim Cook (historian)", "Canadian historian", None);
        historian.score = 0.8;
        let ceo = article("Tim Cook", "Chief executive of Apple", Some("apple.com"));
        let pages = [
            placed(historian, None, 0),
            placed(ceo, Some("apple.com"), 0),
        ];
        assert_eq!(info_box(&sites, &pages), None);
    }

    #[test]
    fn leaves_out_unclear_articles() {
        let sites = [site("a.com", None), site("b.com", None)];
        // Listed after three sites: a namesake or a partial match.
        let late = [placed(
            article("Eiffel Tower (Six Flags)", "Ride", None),
            None,
            3,
        )];
        assert_eq!(info_box(&sites, &late), None);
        // About a site further down.
        let lower = [placed(
            article("B", "Company", Some("b.com")),
            Some("b.com"),
            0,
        )];
        assert_eq!(info_box(&sites, &lower), None);
        let mut partial = article("Mercury", "Topics referred to by the same term", None);
        assert_eq!(info_box(&sites, &[placed(partial.clone(), None, 0)]), None);
        partial.page.description = Some("Planet".to_string());
        partial.named = false;
        assert_eq!(info_box(&sites, &[placed(partial, None, 0)]), None);
    }

    fn profile(service: &str, id: &str) -> plumb_core::profiles::Profile {
        plumb_core::profiles::Profile {
            service: service.into(),
            id: id.into(),
        }
    }

    #[test]
    fn lists_official_profiles() {
        let mut beast = article("MrBeast", "American YouTuber", None);
        beast.page.profiles = vec![
            profile("youtube", "UCX6OQ3DkcsbYNE6H8uQQuVA"),
            profile("youtube-handle", "MrBeast"),
            profile("x", "MrBeast"),
        ];
        let info = info_box(&[], &[placed(beast.clone(), None, 0)]).unwrap();
        let shown: Vec<(&str, &str)> = info
            .profiles
            .iter()
            .map(|p| (p.service, p.url.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("YouTube", "https://www.youtube.com/@MrBeast"),
                ("X", "https://x.com/MrBeast")
            ]
        );
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(html.contains(
            "<li><a href=\"https://www.youtube.com/@MrBeast\" rel=\"noreferrer\">YouTube</a></li>"
        ));

        let pages = [placed(beast, None, 0)];
        let found = profile_answer("MrBeast YouTube", &pages).unwrap();
        assert_eq!(found.url, "https://www.youtube.com/@MrBeast");
        assert_eq!(found.of, "MrBeast");
        assert_eq!(
            profile_answer("mrbeast x", &pages).unwrap().url,
            "https://x.com/MrBeast"
        );
        assert_eq!(profile_answer("mrbeast twitch", &pages), None);
        assert_eq!(profile_answer("mrbeast", &pages), None);
        let mut html = String::new();
        render_profile(&mut html, &found, None);
        assert!(
            html.contains("<span class=\"t\">MrBeast on YouTube</span>"),
            "{html}"
        );
        assert!(html.contains("href=\"https://www.youtube.com/@MrBeast\""));
        assert!(html.contains("Official profile, from Wikidata"), "{html}");
    }

    #[test]
    fn lists_where_a_film_is_listed_apart() {
        let mut dune = article("Dune: Part Two", "2024 film by Denis Villeneuve", None);
        dune.page.profiles = vec![
            profile("x", "dunemovie"),
            profile("imdb", "tt15239678"),
            profile("letterboxd", "dune-part-two"),
        ];
        let pages = [placed(dune.clone(), None, 0)];
        let info = info_box(&[], &pages).unwrap();
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(
            html.contains(
                "aria-label=\"Official profiles\"><li><a href=\"https://x.com/dunemovie\""
            ),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Listed on\"><li><a href=\"https://www.imdb.com/title/tt15239678/\" rel=\"noreferrer\">IMDb</a></li>"), "{html}");
        let found = profile_answer("dune part two imdb", &pages).unwrap();
        assert_eq!(found.url, "https://www.imdb.com/title/tt15239678/");
        let mut html = String::new();
        render_profile(&mut html, &found, None);
        assert!(html.contains("Listing, from Wikidata"), "{html}");
    }

    #[test]
    fn boxes_and_links_a_film_asked_for() {
        let film = |item: &str, whole: bool| PageHit {
            page: Page::from_film(plumb_core::article::Article {
                title: "Les Dents de la nuit".into(),
                description: Some("Film by Stephen Cafiero, 2008".into()),
                item: Some(item.into()),
                views: 4,
                profiles: vec![profile("imdb", "tt1103275")],
                ..Default::default()
            })
            .unwrap(),
            named: true,
            whole,
            ..article("x", "x", None)
        };
        // With no English article, it links its IMDb page from Wikidata.
        let pages = [placed(film("Q3230000", false), None, 1)];
        let found = profile_answer("les dents de la nuit imdb", &pages).unwrap();
        assert_eq!(found.url, "https://www.imdb.com/title/tt1103275/");
        assert_eq!(found.source, "Wikidata");
        // Named by its title alone, it gets no info box; asked for, it does,
        // with no article.
        assert!(info_box(&[], &pages).is_none());
        let pages = [placed(film("Q3230000", true), None, 0)];
        let info = info_box(&[], &pages).unwrap();
        assert_eq!(info.article, None);
        assert_eq!(
            info.wikidata.as_deref(),
            Some("https://www.wikidata.org/wiki/Q3230000")
        );
        assert_eq!(info.profiles.len(), 1);
        // With one, the box links the article.
        let pages = [placed(film("Q3230000/Les_Dents_de_la_nuit", true), None, 0)];
        let info = info_box(&[], &pages).unwrap();
        assert_eq!(
            info.article.as_deref(),
            Some("https://en.wikipedia.org/wiki/Les_Dents_de_la_nuit")
        );
    }

    #[test]
    fn links_a_songs_lyrics() {
        let song = |title: &str, by: &str, profiles: Vec<plumb_core::profiles::Profile>| PageHit {
            page: Page::from_music(plumb_core::article::Article {
                title: title.into(),
                description: Some(format!("Song by {by}, 1975")),
                item: Some("recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()),
                views: 211_087,
                aliases: vec![format!("{title} {by}")],
                profiles,
                ..Default::default()
            })
            .unwrap(),
            named: true,
            ..article("x", "x", None)
        };
        // The article on the song has no Genius page; the song has.
        let mut rhapsody = article("Bohemian Rhapsody", "1975 single by Queen", None);
        rhapsody.named = true;
        let pages = [
            placed(rhapsody, None, 0),
            placed(
                song(
                    "Bohemian Rhapsody",
                    "Queen",
                    vec![profile("genius-song", "Queen-bohemian-rhapsody-lyrics")],
                ),
                None,
                1,
            ),
        ];
        let found = profile_answer("bohemian rhapsody lyrics", &pages).unwrap();
        assert_eq!(
            found.url,
            "https://genius.com/Queen-bohemian-rhapsody-lyrics"
        );
        assert!(!found.search);
        let mut html = String::new();
        render_profile(&mut html, &found, None);
        assert!(html.contains("Listing, from MusicBrainz"), "{html}");

        // A song with no Genius page has its lyrics searched for there.
        let pages = [placed(song("Hey Jude", "The Beatles", Vec::new()), None, 0)];
        let found = profile_answer("hey jude lyrics", &pages).unwrap();
        assert_eq!(
            found.url,
            "https://genius.com/search?q=Hey+Jude+The+Beatles"
        );
        assert!(found.search);
        let mut html = String::new();
        render_profile(&mut html, &found, None);
        assert!(html.contains("Searched for on Genius"), "{html}");
        // Never another service.
        assert_eq!(profile_answer("hey jude spotify", &pages), None);
        assert_eq!(
            profile_lookups("hey jude lyrics"),
            ["hey jude", "hey jude song"]
        );
        assert_eq!(profile_lookups("mrbeast youtube"), ["mrbeast"]);
        assert!(profile_lookups("hey jude").is_empty());
    }

    #[test]
    fn boxes_an_item_without_an_article_under_its_site() {
        let item = PageHit {
            page: Page::from_item(plumb_core::article::Article {
                title: "Linus Tech Tips".into(),
                description: Some("Canadian YouTube channel".into()),
                item: Some("Q111862397".into()),
                site: Some("linustechtips.com".into()),
                aliases: vec!["LTT".into()],
                profiles: vec![profile("youtube-handle", "linustechtips")],
                ..Default::default()
            }),
            ..article("x", "x", None)
        };
        let sites = [site("linustechtips.com", None)];
        let pages = [placed(item, Some("linustechtips.com"), 0)];
        let info = info_box(&sites, &pages).unwrap();
        assert_eq!(info.article, None);
        assert_eq!(info.site.as_deref(), Some("linustechtips.com"));
        let mut html = String::new();
        render_info_box(&mut html, &info);
        assert!(
            html.contains("https://www.youtube.com/@linustechtips"),
            "{html}"
        );
        assert!(!html.contains(">Wikipedia<"), "{html}");
        assert!(html.contains("href=\"https://www.wikidata.org/wiki/Q111862397\""));
        let found = profile_answer("linus tech tips youtube", &pages).unwrap();
        assert_eq!(found.url, "https://www.youtube.com/@linustechtips");
    }

    #[test]
    fn escapes_what_it_shows() {
        let pages = [placed(article("<b>", "\"x\" & <script>", None), None, 0)];
        let mut html = String::new();
        render_info_box(&mut html, &info_box(&[], &pages).unwrap());
        assert!(!html.contains("<b>") && !html.contains("<script>"));
        let mut html = String::new();
        render_answer(
            &mut html,
            &Answer {
                kind: plumb_answer::Kind::Calculation,
                question: "<i>".to_string(),
                answer: "&".to_string(),
                note: None,
            },
        );
        assert!(html.contains("&lt;i&gt;") && html.contains("&amp;"));
    }
}
