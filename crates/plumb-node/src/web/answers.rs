//! What a results page shows besides the results: an instant answer above
//! them ([`plumb_answer`]: sums, conversions, the time somewhere, and the
//! time in a town found in the node's places) and an info box beside
//! them, about the one thing the query names.
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
//! When the article's lead is held (`plumb-tools fetch-leads`), the box shows
//! its first sentences too, and a query that asks what something is
//! ("what is a manatee", "define photosynthesis", "who was ada lovelace")
//! is answered above the results with the first sentence of the article
//! the query names ([`definition_answer`]).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plumb_answer::{Answer, Rates, ECB_RATES_URL};
use plumb_core::place::Place;
use plumb_core::profiles::{services_asked, shown_profiles};
use plumb_core::truncate_chars;
use plumb_index::pages::{Page, PlacedPage, FILMS_SET, MUSIC_SET, WIKIDATA_SET};
use plumb_index::Hit;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::weather::place_label;
use super::{escape_html, homepage_url, http_url, AppState};
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

/// The time in a town [`plumb_answer`]'s own list lacks ("time in
/// boulder", "what time is it in porto"), found in the node's places.
/// `country` is the searcher's, to tell towns of one name apart.
pub(super) async fn town_time(
    state: &AppState,
    query: &str,
    country: Option<&str>,
) -> Option<Answer> {
    let name = plumb_answer::asked_town(query)?;
    let backend = Arc::clone(&state.backend);
    let country = country.map(str::to_string);
    let place = tokio::task::spawn_blocking(move || backend.locate(&name, country.as_deref()))
        .await
        .ok()
        .flatten()?;
    time_in_place(&place)
}

/// The time in `place`, when it is a city or town (not a nightclub
/// called Heaven) and the clock it keeps can be told.
pub(crate) fn time_in_place(place: &Place) -> Option<Answer> {
    if !matches!(
        place.kind.as_str(),
        "place=city" | "place=town" | "place=borough"
    ) {
        return None;
    }
    let town = plumb_answer::Town {
        shown: &place_label(place),
        country: place.country.as_deref()?,
        region: place.region.as_deref(),
        lat: place.lat,
        lon: place.lon,
    };
    let now = i64::try_from(plumb_core::now_unix()).unwrap_or(i64::MAX);
    plumb_answer::time_in_town(&town, now)
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
pub struct ProfileAnswer {
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
/// as one ("bohemian rhapsody song" for "bohemian rhapsody lyrics"). For
/// a listing of one kind of work, the name without the words saying that
/// kind too: "top gun movie cast" asks for the film called Top Gun,
/// unless a film is called "Top Gun Movie" ("bee movie cast").
pub fn profile_lookups(query: &str) -> Vec<String> {
    let Some((services, name)) = services_asked(query) else {
        return Vec::new();
    };
    let mut lookups = vec![name.clone()];
    if services.iter().any(|s| s.key == "genius-song") {
        lookups.push(format!("{name} song"));
    }
    if let Some(title) = listed_kinds(&services).and_then(|kinds| kind_named(&name, kinds)) {
        lookups.push(title.to_string());
    }
    lookups
}

/// Services that list one kind of work, with the words that call a work
/// that kind: in its article's title ("Barbie (film)", "Hades (video
/// game)"), at the head of its description ("1986 action drama film by
/// Tony Scott") or after its name in a search ("top gun movie"). A page
/// of the films set is a film or show.
const KIND_LISTINGS: &[(&[&str], &[&str])] = &[
    (
        &[
            "imdb",
            "rotten-tomatoes",
            "letterboxd",
            "tmdb-movie",
            "tmdb-tv",
            "netflix",
        ],
        &[
            "film",
            "movie",
            "series",
            "miniseries",
            "sitcom",
            "show",
            "tv",
        ],
    ),
    (&["steam", "igdb"], &["game"]),
];

/// Services holding an account of someone's own: a person's, a group's or
/// an organization's.
const ACCOUNTS: &[&str] = &[
    "instagram",
    "x",
    "tiktok",
    "facebook",
    "threads",
    "bluesky",
    "mastodon",
    "twitch",
    "linkedin",
    "linkedin-company",
    "soundcloud",
    "patreon",
];

/// Facts only a person (born) or a group or organization (founded, its
/// founders, its CEO or headquarters) has.
const HOLDER_FACTS: &[plumb_core::facts::FactKind] = &[
    plumb_core::facts::FactKind::Born,
    plumb_core::facts::FactKind::Founded,
    plumb_core::facts::FactKind::Founder,
    plumb_core::facts::FactKind::Ceo,
    plumb_core::facts::FactKind::Headquarters,
];

/// Where a description stops saying what its page is and goes on to who
/// made it or what it is part of.
const DESCRIPTION_HEAD_ENDS: &[&str] = &[
    " by ",
    " directed ",
    " created ",
    " starring ",
    " based ",
    " of ",
    " from ",
    " (",
    ",",
];

/// The kinds of work [`KIND_LISTINGS`] has for the listing among
/// `services`, if one is.
fn listed_kinds(services: &[&plumb_core::profiles::Service]) -> Option<&'static [&'static str]> {
    KIND_LISTINGS
        .iter()
        .find(|(keys, _)| services.iter().any(|s| keys.contains(&s.key)))
        .map(|(_, kinds)| *kinds)
}

/// `name` without the words after it saying it is one of `kinds` ("top
/// gun" of "top gun movie", "the office" of "the office tv show", "halo"
/// of "halo video game"), when it has some.
fn kind_named<'a>(name: &'a str, kinds: &[&str]) -> Option<&'a str> {
    let (rest, last) = name.rsplit_once(' ')?;
    kinds.contains(&last).then(|| {
        rest.strip_suffix(" tv")
            .or_else(|| rest.strip_suffix(" video"))
            .unwrap_or(rest)
    })
}

/// Of `candidates`, the articles a lookup of `name` for `query`'s profile
/// may name ([`resolve_entity`]): for a listing of one kind of work
/// ([`KIND_LISTINGS`]), only works of that kind when some are called so.
/// "barbie cast" is the 2023 film's, not the doll's, "the last of us imdb"
/// the show's, not the game's, and "hades steam" the game's, not the
/// god's. Looked up with its kind ("top gun movie"), only a work called
/// so, since a search names every film of the title then, and the film
/// called Top Gun is looked up by its name alone ([`profile_lookups`]).
/// For an account ([`ACCOUNTS`]), only a person, group or organization
/// when some are: "drake instagram" is the rapper's, not the name list's,
/// and "pink instagram" the singer's, not the colour's.
/// The kind asked for decides, never which namesake has the profile.
pub fn profile_candidates(
    query: &str,
    name: &str,
    mut candidates: Vec<plumb_index::pages::PageHit>,
) -> Vec<plumb_index::pages::PageHit> {
    let Some((services, _)) = services_asked(query) else {
        return candidates;
    };
    if services
        .iter()
        .all(|service| ACCOUNTS.contains(&service.key))
    {
        let holder = |hit: &plumb_index::pages::PageHit| {
            hit.page
                .facts
                .iter()
                .any(|fact| HOLDER_FACTS.contains(&fact.kind))
        };
        if candidates.iter().any(holder) {
            candidates.retain(holder);
        }
        return candidates;
    }
    let Some(kinds) = listed_kinds(&services) else {
        return candidates;
    };
    if kind_named(name, kinds).is_some() {
        let name = plumb_core::normalize_text(name);
        candidates.retain(|hit| {
            plumb_core::normalize_text(plumb_index::pages::base_title(&hit.page.title)) == name
        });
    }
    let films = kinds.contains(&"film");
    let of_kind = |hit: &plumb_index::pages::PageHit| {
        let qualifier = hit
            .page
            .title
            .strip_suffix(')')
            .and_then(|title| title.rsplit_once(" ("));
        let description = hit.page.description.as_deref().unwrap_or("").to_lowercase();
        let head_end = DESCRIPTION_HEAD_ENDS
            .iter()
            .filter_map(|end| description.find(end))
            .min()
            .unwrap_or(description.len());
        films && hit.page.set == FILMS_SET
            || qualifier.is_some_and(|(_, what)| {
                what.split_whitespace()
                    .any(|word| kinds.contains(&word.to_lowercase().as_str()))
            })
            || description[..head_end]
                .split_whitespace()
                .last()
                .is_some_and(|word| kinds.contains(&word))
    };
    if candidates.iter().any(of_kind) {
        candidates.retain(of_kind);
    }
    candidates
}

/// Whether a lookup of `query`'s profile ([`profile_lookups`]) goes on
/// to the songs and albums of the music set ([`songs_profile`]) when the
/// article it names, resolved as `resolution`, gives none. Lyrics always
/// do: MusicBrainz knows a song's page on Genius more often than Wikidata,
/// and lists the best-known recording of a title first ("creep" is
/// Radiohead's, though TLC's has an article too). Other profiles only
/// when no article is called so, and never a film's, show's or game's
/// listing ([`KIND_LISTINGS`]), which no song has.
pub fn searches_songs(query: &str, resolution: &EntityResolution) -> bool {
    services_asked(query).is_some_and(|(services, _)| {
        services.iter().any(|s| s.key == "genius-song")
            || matches!(resolution, EntityResolution::Unresolved)
                && listed_kinds(&services).is_none()
    })
}

/// The profile `query` asks for of a song or album among `found`, what a
/// search for a lookup found ([`searches_songs`]). When the lookup's
/// article resolved as `resolution`, the song must have its title: a
/// search for "pyramid song", Radiohead's "Pyramid Song", finds
/// Wolfmother's "Pyramid" as a song too.
pub fn songs_profile(
    query: &str,
    resolution: &EntityResolution,
    found: Vec<PlacedPage>,
) -> Option<ProfileAnswer> {
    let title = |title: &str| plumb_core::normalize_text(plumb_index::pages::base_title(title));
    let songs: Vec<PlacedPage> = found
        .into_iter()
        .filter(|placed| {
            placed.hit.page.set == MUSIC_SET
                && match resolution {
                    EntityResolution::Resolved(page) => {
                        title(&placed.hit.page.title) == title(&page.title)
                    }
                    _ => true,
                }
        })
        .collect();
    profile_answer(query, &songs)
}

/// The official profile `query` asks for ("mrbeast youtube", "valve
/// steam", "bohemian rhapsody lyrics"), when the words before the service
/// name a Wikipedia article or a song or album in `pages` (found for those
/// words) that has a profile there. A song with no page on Genius has its
/// lyrics searched for there, by its title and artist.
pub fn profile_answer(query: &str, pages: &[PlacedPage]) -> Option<ProfileAnswer> {
    pages
        .iter()
        .filter(|placed| {
            placed.hit.named
                && (is_about_one_thing(&placed.hit.page) || placed.hit.page.set == MUSIC_SET)
        })
        .find_map(|placed| profile_answer_from_page(query, &placed.hit.page))
}

/// Renders a profile only after the caller has resolved its entity.
pub fn profile_answer_from_page(query: &str, page: &Page) -> Option<ProfileAnswer> {
    let (services, _) = services_asked(query)?;
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
/// subject's, in the order to try them: the pages it names, those with a
/// plain title first (Wikipedia's main sense of a name: "Australia", the
/// country, before "Australia (continent)"), then those listed under the
/// site it names, each about one thing and none a disambiguation page.
pub(crate) fn fact_pages(pages: &[PlacedPage]) -> impl Iterator<Item = &PlacedPage> {
    let qualified = |placed: &&PlacedPage| placed.hit.page.title.trim_end().ends_with(')');
    let named = pages
        .iter()
        .filter(|placed| placed.hit.named)
        .filter(move |placed| !qualified(placed))
        .chain(
            pages
                .iter()
                .filter(|placed| placed.hit.named)
                .filter(move |placed| qualified(placed)),
        );
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

/// Resolving an entity does not inspect whether it has the requested
/// fact. A missing fact must never select a namesake instead.
pub enum EntityResolution<'a> {
    Resolved(&'a Page),
    Ambiguous(Vec<&'a Page>),
    Unresolved,
}

pub fn resolve_entity<'a>(
    subject: &str,
    candidates: &'a [plumb_index::pages::PageHit],
    kinds: Option<&[plumb_core::facts::FactKind]>,
) -> EntityResolution<'a> {
    use plumb_core::facts::FactKind;
    let key = |text: &str| plumb_core::collapse_whitespace(text).to_lowercase();
    let subject = key(subject);
    let mut seen = std::collections::HashSet::new();
    let mut candidates: Vec<_> = candidates
        .iter()
        .filter(|hit| {
            is_about_one_thing(&hit.page)
                && !is_disambiguation(&hit.page.title, hit.page.description.as_deref())
                && hit.page.item.as_deref().is_some_and(|id| {
                    id.starts_with('Q')
                        && id.len() > 1
                        && id[1..].bytes().all(|b| b.is_ascii_digit())
                })
                && seen.insert(hit.page.item.as_deref())
        })
        .collect();
    // CEO/headquarters requests supply an organization type, independently
    // of imported property availability. Founding properties do not:
    // settlements and other entities can have a founder or inception too.
    let company = kinds.is_some_and(|kinds| {
        kinds
            .iter()
            .any(|kind| matches!(kind, FactKind::Ceo | FactKind::Headquarters))
    });
    if company {
        let organizations: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|hit| {
                let description = key(hit.page.description.as_deref().unwrap_or(""));
                let words: Vec<_> = description.split(|c: char| !c.is_alphabetic()).collect();
                description.contains("record label")
                    || [
                        "company",
                        "corporation",
                        "organization",
                        "organisation",
                        "business",
                        "bank",
                        "university",
                        "nonprofit",
                    ]
                    .iter()
                    .any(|word| words.contains(word))
            })
            .collect();
        if !organizations.is_empty() {
            candidates = organizations;
        }
    }
    if let Some(works) = works_made(&subject, &candidates, kinds) {
        match works.as_slice() {
            [work] => return EntityResolution::Resolved(&work.page),
            _ => candidates = works,
        }
    }
    if candidates.is_empty() {
        return EntityResolution::Unresolved;
    }
    if kinds.is_some_and(|kinds| {
        kinds
            .iter()
            .any(|kind| matches!(kind, FactKind::Founder | FactKind::Founded))
    }) {
        // Compact records lack structured entity types. A founding
        // question cannot prefer the plain title or a legal company name
        // over another entity known by the same name. Ask for clarification
        // rather than infer a type from a special-case description word.
        let named: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|hit| {
                plumb_index::same_words(&hit.page.title, &subject)
                    || legal_name(&hit.page.title) == subject
                    || hit.named
                    || hit.whole
            })
            .collect();
        if named.len() > 1 {
            return main_sense(&subject, &named);
        }
    }
    let exact: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|hit| plumb_index::same_words(&hit.page.title, &subject))
        .collect();
    if exact.len() == 1 {
        return EntityResolution::Resolved(&exact[0].page);
    }
    let named: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|hit| legal_name(&hit.page.title) == subject)
        .collect();
    if named.len() == 1 {
        return EntityResolution::Resolved(&named[0].page);
    }

    let named: Vec<_> = candidates.iter().copied().filter(|hit| hit.named).collect();
    if !named.is_empty() {
        candidates = named;
    } else {
        // A type hint cannot validate a partial namesake: "Pear" is
        // not "Pear Records" just because it is the only company found.
        candidates.retain(|hit| hit.whole);
    }
    match candidates.as_slice() {
        [] => EntityResolution::Unresolved,
        [hit] => EntityResolution::Resolved(&hit.page),
        _ => main_sense(&subject, &candidates),
    }
}

/// The bare name of a company's legal one ("Netflix, Inc."), lowercased.
fn legal_name(title: &str) -> String {
    let title = plumb_core::collapse_whitespace(title).to_lowercase();
    for suffix in [
        ", inc.",
        " inc.",
        ", inc",
        " inc",
        " corporation",
        " corp.",
        " ltd.",
        " limited",
    ] {
        if let Some(bare) = title.strip_suffix(suffix) {
            return bare.to_string();
        }
    }
    title
}

/// Settles a tie between pages named like `subject` (lowercased): the one
/// Wikipedia article known by the name in full, by its title, its legal
/// name or a redirect ("Beethoven" to Ludwig van Beethoven), is Wikipedia's
/// main sense of it, so items with no article and pages named so only
/// before a qualifier ("Beethoven (film)") give way to it. Two such
/// articles ("Apple" and "Apple Inc.") leave the question open.
fn main_sense<'a>(subject: &str, tied: &[&'a plumb_index::pages::PageHit]) -> EntityResolution<'a> {
    let mut own = tied.iter().filter(|hit| {
        let title = &hit.page.title;
        hit.page.is_article()
            && (legal_name(title) == subject || hit.named && !title.trim_end().ends_with(')'))
    });
    match (own.next(), own.next()) {
        (Some(hit), None) => EntityResolution::Resolved(&hit.page),
        _ => EntityResolution::Ambiguous(tied.iter().map(|hit| &hit.page).collect()),
    }
}

/// What a question about a work's maker asks: the subject is the work.
/// Only works have these, so a page without one is no answer to it, an
/// exception to [`EntityResolution`]'s rule ([`works_made`]).
const MAKERS: &[plumb_core::facts::FactKind] = {
    use plumb_core::facts::FactKind::*;
    &[Author, Director, Composer, Creator]
};

/// How many times as read as every other work the subject names the work
/// a maker question means must be ("Titanic (1997 film)", not the 1953 one).
const MOST_READ_WORK: u64 = 10;

/// The works a question about a work's maker can be about, when it is one
/// and `candidates` hold any: those `subject` (lowercased) names, by their
/// title or another name, with or without a qualifier or "the", that have
/// the maker it asks for first. Only a work has an author or a director,
/// and compact records say what a page is only by such facts: "who wrote
/// 1984" asks about the novel known as 1984, not the year, and "who
/// directed titanic" about a film, not the ship. Of several, one read far
/// more than each of the others is the one meant, so it is given alone.
/// Only the first kind asked counts: "director of the fbi" asks for no TV
/// series' creator.
fn works_made<'a>(
    subject: &str,
    candidates: &[&'a plumb_index::pages::PageHit],
    kinds: Option<&[plumb_core::facts::FactKind]>,
) -> Option<Vec<&'a plumb_index::pages::PageHit>> {
    let maker = kinds?.first().filter(|kind| MAKERS.contains(kind))?;
    // fact_asked keeps a work's "the" for "The Hobbit", but "the mona
    // lisa" is "Mona Lisa".
    let named = |name: &str| {
        let name =
            plumb_core::collapse_whitespace(plumb_index::pages::base_title(name)).to_lowercase();
        name == subject || subject.strip_prefix("the ") == Some(name.as_str())
    };
    let mut works: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|hit| {
            let page = &hit.page;
            (hit.named || named(&page.title) || page.aliases.iter().any(|alias| named(alias)))
                && page.facts.iter().any(|fact| fact.kind == *maker)
        })
        .collect();
    works.sort_by_key(|hit| std::cmp::Reverse(hit.page.views));
    match works.as_slice() {
        [] => None,
        [most, next, ..] if next.page.views.saturating_mul(MOST_READ_WORK) >= most.page.views => {
            Some(works)
        }
        [most, ..] => Some(vec![*most]),
    }
}

/// Legacy displayed-page selection retained for evaluator compatibility.
/// User-facing adapters resolve entities before rendering their facts.
pub fn fact_answer(
    asked: &plumb_core::facts::FactQuestion,
    pages: &[PlacedPage],
    now: u64,
) -> Option<plumb_answer::Answer> {
    fact_pages(pages).find_map(|placed| fact_answer_from_page(asked, &placed.hit.page, now))
}

/// Renders facts only after the caller has resolved an entity.
pub(crate) fn fact_answer_from_page(
    asked: &plumb_core::facts::FactQuestion,
    page: &Page,
    now: u64,
) -> Option<plumb_answer::Answer> {
    use plumb_core::facts::FactKind;
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
}

/// Words before a name that ask what it is: "what is a" of "what is a
/// manatee". "what's" is read as "what is"; "whats" is not, since "whats
/// app" is a misspelled name.
const DEFINITION_LEADS: &[&str] = &[
    "what is the meaning of ",
    "what is the definition of ",
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

/// Words after "what does" and a name that ask what it means or is: "what
/// does lol mean", "what does nasa stand for", "what does a cpa do".
const DOES_TAILS: &[&str] = &[" mean", " stand for", " stands for", " do"];

/// The name `query` asks what it is: "manatee" of "what is a manatee?",
/// "photosynthesis" of "define photosynthesis", "nasa" of "what does nasa
/// stand for". `None` for a query that asks nothing so, or more than that
/// ("what is the capital of france" asks a fact, "what is my ip" about the
/// searcher).
pub(crate) fn definition_asked(query: &str) -> Option<String> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches(['?', '.', '!']))
        .to_lowercase()
        .replace('’', "'");
    let q = match q.strip_prefix("what's ") {
        Some(rest) => format!("what is {rest}"),
        None => q,
    };
    let does = q.strip_prefix("what does ").and_then(|rest| {
        let name = DOES_TAILS.iter().find_map(|tail| rest.strip_suffix(tail))?;
        Some(
            ["a ", "an ", "the "]
                .iter()
                .find_map(|article| name.strip_prefix(article))
                .unwrap_or(name),
        )
    });
    let name = does
        .or_else(|| {
            DEFINITION_LEADS
                .iter()
                .find_map(|lead| q.strip_prefix(lead))
        })
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
                    | "it"
                    | "this"
                    | "that"
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
/// "define anadromous", "prioritize meaning", "what does gdp stand for".
/// Such a query is answered from Wiktionary first, others from Wikipedia
/// first.
pub(crate) fn asks_word(query: &str) -> bool {
    let q = query.to_lowercase();
    q.contains(" stand for")
        || q.contains(" stands for")
        || q.split_whitespace()
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
        ValueType::Coordinates => {
            let (lat, lon) = plumb_core::facts::coordinates(first)?;
            let (ns, ew) = (
                if lat < 0.0 { 'S' } else { 'N' },
                if lon < 0.0 { 'W' } else { 'E' },
            );
            (
                format!(
                    "{}° {ns}, {}° {ew}",
                    group_digits(lat.abs(), 4),
                    group_digits(lon.abs(), 4)
                ),
                None,
            )
        }
        ValueType::Quantity => {
            let (number, year) = first.split_once(';').unwrap_or((first, ""));
            let amount: f64 = number.parse().ok()?;
            match kind {
                plumb_core::facts::FactKind::Population => (
                    group_digits(amount.round(), 0),
                    (!year.is_empty()).then(|| format!("Counted in {year}")),
                ),
                plumb_core::facts::FactKind::AtomicNumber => (group_digits(amount, 0), None),
                plumb_core::facts::FactKind::OrbitalPeriod => {
                    let days = amount / 86_400.0;
                    let years = days / 365.25;
                    let text = if days < 1.0 {
                        format!("{} hours", group_digits(amount / 3600.0, 2))
                    } else if years < 2.0 {
                        format!("{} days", group_digits(days, 2))
                    } else {
                        format!(
                            "{} years ({} days)",
                            group_digits(years, 2),
                            group_digits(days, 0)
                        )
                    };
                    (text, None)
                }
                plumb_core::facts::FactKind::Radius if amount >= 1000.0 => {
                    let km = amount / 1000.0;
                    let digits = if km >= 100.0 { 0 } else { 2 };
                    (
                        format!(
                            "{} km ({} mi)",
                            group_digits(km, digits),
                            group_digits(km / 1.609_344, digits)
                        ),
                        None,
                    )
                }
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

    #[test]
    fn tells_the_time_in_a_town_found() {
        let place = |name: &str, kind: &str, region: Option<&str>, country: &str| Place {
            rank: 0,
            name: name.into(),
            kind: kind.into(),
            tags: Vec::new(),
            lat: 40.01,
            lon: -105.27,
            town: None,
            region: region.map(str::to_string),
            country: Some(country.into()),
            address: None,
            website: None,
            osm: "n1".into(),
            aliases: Vec::new(),
        };
        let boulder = time_in_place(&place("Boulder", "place=city", Some("CO"), "US")).unwrap();
        assert_eq!(boulder.question, "Time in Boulder, CO");
        assert!(boulder.note.unwrap().contains("UTC-"));
        // A nightclub called Heaven is no town, and El Paso's state keeps
        // two clocks.
        assert!(time_in_place(&place("Heaven", "amenity=nightclub", None, "GB")).is_none());
        assert!(time_in_place(&place("El Paso", "place=city", Some("TX"), "US")).is_none());
    }

    fn article(title: &str, description: &str, site: Option<&str>) -> PageHit {
        PageHit {
            page: Page {
                paper: None,
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
                sections: Vec::new(),
                content_language: None,
                search: None,
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
            query_evidence: None,
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
        // However it is asked.
        for (query, name) in [
            ("what does lol mean", "lol"),
            ("What does NASA stand for?", "nasa"),
            ("what does a cpa do", "cpa"),
            ("what's a roth ira", "roth ira"),
            ("what’s kombucha", "kombucha"),
            ("what is the meaning of serendipity", "serendipity"),
            ("what is the definition of inflation", "inflation"),
        ] {
            assert_eq!(definition_asked(query).as_deref(), Some(name), "{query}");
        }
        assert_eq!(definition_asked("what is my ip"), None);
        assert_eq!(definition_asked("what is the capital of france"), None);
        assert_eq!(definition_asked("what's the capital of france"), None);
        assert_eq!(definition_asked("what does it mean"), None);
        assert_eq!(definition_asked("what does my name mean"), None);
        assert_eq!(definition_asked("whats app"), None);
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
        assert!(asks_word("what does gdp stand for"));
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
    fn founding_questions_leave_shared_names_ambiguous_without_type_evidence() {
        use plumb_core::facts::FactKind::{Founded, Founder};
        for (subject, description) in [
            ("Teral", "mineral"),
            ("Vesrin", "city"),
            ("Ordel", "organization"),
            ("Navren", ""),
        ] {
            let mut main = article(subject, description, None);
            main.page.item = Some("Q910001".into());
            let mut namesake = with_facts(
                article(&format!("{subject} Inc."), "company", None),
                &[(Founder, "Example founder"), (Founded, "1901")],
            );
            namesake.page.item = Some("Q910002".into());
            namesake.named = false;
            for kind in [Founder, Founded] {
                for candidates in [
                    [main.clone(), namesake.clone()],
                    [namesake.clone(), main.clone()],
                ] {
                    let EntityResolution::Ambiguous(found) =
                        resolve_entity(subject, &candidates, Some(&[kind]))
                    else {
                        panic!("{subject} must remain ambiguous for {kind:?}");
                    };
                    assert_eq!(found.len(), 2);
                }
                // Removing all enrichment cannot change identity resolution.
                let mut empty = namesake.clone();
                empty.page.facts.clear();
                assert!(matches!(
                    resolve_entity(subject, &[main.clone(), empty], Some(&[kind])),
                    EntityResolution::Ambiguous(_)
                ));
            }
        }
    }

    #[test]
    fn a_name_typed_without_its_accents_is_the_title() {
        let entity = |title: &str, item: &str| {
            let mut hit = article(title, "", None);
            hit.page.item = Some(item.into());
            hit
        };
        // Both named by "beyonce": the singer's title is the name asked
        // for, whatever its accent.
        let candidates = [
            entity("Beyoncé (album)", "Q15079464"),
            entity("Beyoncé", "Q36153"),
        ];
        let EntityResolution::Resolved(page) = resolve_entity("beyonce", &candidates, None) else {
            panic!("beyonce is Beyoncé");
        };
        assert_eq!(page.title, "Beyoncé");
        let candidates = [
            entity("Mötley Crüe", "Q151215"),
            entity("Mötley Crüe (album)", "Q1546186"),
        ];
        assert!(matches!(
            resolve_entity("motley crue", &candidates, None),
            EntityResolution::Resolved(page) if page.title == "Mötley Crüe"
        ));
        // Other words are another name.
        assert!(!plumb_index::same_words("Beyoncé", "beyonce knowles"));
        assert!(plumb_index::same_words("AC/DC", "ac dc"));
    }

    #[test]
    fn facts_of_the_main_sense_of_a_name_despite_namesakes() {
        use plumb_core::facts::{fact_asked, FactKind::*};
        let entity = |title: &str, item: &str, facts: &[(plumb_core::facts::FactKind, &str)]| {
            let mut hit = with_facts(article(title, "", None), facts);
            hit.page.item = Some(item.into());
            hit
        };
        let ask = |q: &str, candidates: &[PageHit]| {
            let asked = fact_asked(q).unwrap();
            match resolve_entity(&asked.subject, candidates, Some(&asked.kinds)) {
                EntityResolution::Resolved(page) => fact_answer_from_page(&asked, page, 0),
                _ => None,
            }
        };
        // Named by a redirect ("Beethoven"), the person comes before the
        // pages named so only before a qualifier.
        let beethoven = [
            entity("Beethoven (film)", "Q754892", &[]),
            entity("Ludwig van Beethoven", "Q255", &[(Born, "1770-12-17")]),
            entity("Beethoven (TV series)", "Q2566764", &[]),
        ];
        let born = ask("when was beethoven born", &beethoven).unwrap();
        assert_eq!(born.answer, "December 17, 1770");
        // A founding question too: the organization, not the band.
        let un = [
            entity("United Nations (band)", "Q7888317", &[(Founded, "2005")]),
            entity("United Nations", "Q1065", &[(Founded, "1945-10-24")]),
        ];
        let founded = ask("when was the united nations founded", &un).unwrap();
        assert_eq!(founded.answer, "October 24, 1945");
        // An item with no article gives way to the article of its name.
        let mut no_article = entity("Gold", "Q910023", &[]);
        no_article.page.set = plumb_index::pages::WIKIDATA_SET.into();
        let gold = [no_article, entity("Gold", "Q897", &[(AtomicNumber, "79")])];
        assert_eq!(ask("atomic number of gold", &gold).unwrap().answer, "79");
        // Named only before their qualifiers, namesakes stay a question.
        let qualified = [
            entity("Einstein (American TV series)", "Q910020", &[]),
            entity("Einstein (film)", "Q910022", &[]),
        ];
        assert!(matches!(
            resolve_entity("einstein", &qualified, None),
            EntityResolution::Ambiguous(_)
        ));
    }

    #[test]
    fn founding_questions_resolve_unique_and_qualified_settlements_and_organizations() {
        use plumb_core::facts::{fact_asked, FactKind::Founded};
        let mut city = with_facts(article("Ravelin", "city", None), &[(Founded, "1801")]);
        city.page.item = Some("Q910011".into());
        let mut company = article("Ravelin Inc.", "company", None);
        company.page.item = Some("Q910012".into());
        company.named = false;
        let asked = fact_asked("when was ravelin founded").unwrap();
        let sole = [city.clone()];
        let EntityResolution::Resolved(page) =
            resolve_entity(&asked.subject, &sole, Some(&asked.kinds))
        else {
            panic!("a unique settlement must resolve");
        };
        assert_eq!(
            fact_answer_from_page(&asked, page, 0).unwrap().answer,
            "1801"
        );

        let only_company = [company.clone()];
        let EntityResolution::Resolved(page) =
            resolve_entity(&asked.subject, &only_company, Some(&asked.kinds))
        else {
            panic!("a unique legal organization name must resolve");
        };
        assert_eq!(fact_answer_from_page(&asked, page, 0), None);

        // An explicit organization name must keep its missing fact even
        // when the settlement has the requested founding date.
        city.named = false;
        let both = [city, company];
        let asked = fact_asked("when was ravelin inc. founded").unwrap();
        let EntityResolution::Resolved(page) =
            resolve_entity(&asked.subject, &both, Some(&asked.kinds))
        else {
            panic!("a qualified organization must resolve");
        };
        assert_eq!(page.item.as_deref(), Some("Q910012"));
        assert_eq!(fact_answer_from_page(&asked, page, 0), None);
    }

    #[test]
    fn makers_are_asked_of_the_work_the_name_means() {
        use plumb_core::facts::{fact_asked, FactKind, FactKind::*};
        let page = |title: &str, item: &str, views: u64, facts: &[(FactKind, &str)]| {
            let mut hit = with_facts(article(title, "", None), facts);
            hit.page.item = Some(item.into());
            hit.page.views = views;
            hit
        };
        let ask = |q: &str, candidates: &[PageHit]| {
            let asked = fact_asked(q).unwrap();
            match resolve_entity(&asked.subject, candidates, Some(&asked.kinds)) {
                EntityResolution::Resolved(page) => fact_answer_from_page(&asked, page, 0),
                _ => None,
            }
        };
        // The year is called 1984; the novel is known as "1984 (novel)".
        let mut novel = page(
            "Nineteen Eighty-Four",
            "Q208460",
            30_159,
            &[(Author, "George Orwell")],
        );
        novel.named = false;
        novel.page.aliases = vec!["1984 (novel)".into()];
        let advert = page(
            "1984 (advertisement)",
            "Q4566",
            900,
            &[(Director, "Ridley Scott")],
        );
        let nineteen = [page("1984", "Q2432", 2_014, &[]), advert, novel];
        let answer = ask("who wrote 1984", &nineteen).unwrap();
        assert_eq!(answer.question, "Author of Nineteen Eighty-Four");
        assert_eq!(answer.answer, "George Orwell");
        assert_eq!(
            ask("who directed 1984", &nineteen).unwrap().answer,
            "Ridley Scott"
        );
        // Of films called Titanic, the one read far more.
        let titanic = [
            page("Titanic", "Q25173", 39_360, &[(Owner, "White Star Line")]),
            page(
                "Titanic (1953 film)",
                "Q1140317",
                1_500,
                &[(Director, "Jean Negulesco")],
            ),
            page(
                "Titanic (1997 film)",
                "Q44578",
                30_683,
                &[(Director, "James Cameron")],
            ),
        ];
        let answer = ask("who directed titanic", &titanic).unwrap();
        assert_eq!(
            (answer.question.as_str(), answer.answer.as_str()),
            ("Director of Titanic (1997 film)", "James Cameron")
        );
        assert_eq!(
            ask("titanic owner", &titanic).unwrap().answer,
            "White Star Line"
        );
        // Works read alike stay a question, and a work called the name
        // keeps it ("The Lion King" over its remake).
        let alien = [
            page(
                "Alien (film)",
                "Q103569",
                23_181,
                &[(Director, "Ridley Scott")],
            ),
            page(
                "Alien (franchise)",
                "Q1813470",
                21_144,
                &[(Director, "James Cameron")],
            ),
        ];
        assert_eq!(ask("who directed alien", &alien), None);
        let lion = [
            page(
                "The Lion King (2019 film)",
                "Q27964762",
                20_000,
                &[(Director, "Jon Favreau")],
            ),
            page(
                "The Lion King",
                "Q36479",
                40_000,
                &[(Director, "Roger Allers")],
            ),
        ];
        assert_eq!(
            ask("who directed the lion king", &lion).unwrap().answer,
            "Roger Allers"
        );
        // A work's "the" need not be in its title.
        let mona = [
            page(
                "Mona Lisa",
                "Q12418",
                34_216,
                &[(Creator, "Leonardo da Vinci")],
            ),
            page(
                "Mona Lisa (Prado)",
                "Q2126369",
                1_497,
                &[(Owner, "Museo del Prado")],
            ),
        ]
        .map(|mut hit| {
            hit.named = false;
            hit
        });
        let answer = ask("who painted the mona lisa", &mona).unwrap();
        assert_eq!(answer.answer, "Leonardo da Vinci");
        // "director of" asks a creator only of what the name means.
        let fbi = [
            page("Federal Bureau of Investigation", "Q8333", 60_000, &[]),
            page(
                "FBI (TV series)",
                "Q57571045",
                14_151,
                &[(Creator, "Dick Wolf")],
            ),
        ];
        assert_eq!(ask("director of fbi", &fbi), None);
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
            let asked = fact_asked(q).unwrap();
            let candidates: Vec<_> = pages
                .iter()
                .enumerate()
                .map(|(index, placed)| {
                    let mut hit = placed.hit.clone();
                    hit.page.item = Some(format!("Q{}", index + 1));
                    hit
                })
                .collect();
            match resolve_entity(&asked.subject, &candidates, Some(&asked.kinds)) {
                EntityResolution::Resolved(page) => {
                    fact_answer_from_page(&asked, page, 1_791_244_800)
                }
                _ => None,
            }
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

        // The continent, found first, gives way to the country, the
        // plain title.
        let both = [
            placed(
                with_facts(
                    article("Australia (continent)", "continent", None),
                    &[(Area, "8600000000000")],
                ),
                None,
                0,
            ),
            placed(
                with_facts(
                    article("Australia", "country in Oceania", None),
                    &[(Area, "7688287000000")],
                ),
                None,
                1,
            ),
        ];
        let first: Vec<&str> = fact_pages(&both)
            .map(|placed| placed.hit.page.title.as_str())
            .collect();
        assert_eq!(first, ["Australia", "Australia (continent)"]);
        assert_eq!(
            ask("how big is australia", &both).unwrap().answer,
            "7,688,287 km² (2,968,463 sq mi)"
        );

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
        // A partial company name is no answer.
        let mut unnamed = tesla.clone();
        unnamed[0].hit.named = false;
        assert_eq!(ask("who founded tesla energy", &unnamed), None);
    }

    #[test]
    fn new_kinds_read_as_people_write_them() {
        use plumb_core::facts::FactKind;
        let text = |kind, value| fact_text(kind, &[value]).unwrap().0;
        assert_eq!(
            text(FactKind::Coordinates, "-35.293056,149.126944"),
            "35.2931° S, 149.1269° E"
        );
        assert_eq!(text(FactKind::AtomicNumber, "79"), "79");
        // Earth's 365.256363 days, Jupiter's 4,332.59, the Moon's 27.32.
        assert_eq!(text(FactKind::OrbitalPeriod, "31558149.763"), "365.26 days");
        assert_eq!(
            text(FactKind::OrbitalPeriod, "374335776"),
            "11.86 years (4,333 days)"
        );
        assert_eq!(text(FactKind::Radius, "6371000"), "6,371 km (3,959 mi)");
        assert_eq!(text(FactKind::Radius, "250"), "250 m (820 ft)");
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
        // Lyrics are looked for among songs whatever the articles called
        // so; other profiles only when there are none.
        let ambiguous = EntityResolution::Ambiguous(Vec::new());
        assert!(searches_songs("creep lyrics", &ambiguous));
        assert!(searches_songs(
            "hey jude spotify",
            &EntityResolution::Unresolved
        ));
        assert!(!searches_songs("hey jude spotify", &ambiguous));
    }

    #[test]
    fn a_work_asked_for_with_its_kind_is_the_one_called_so() {
        assert_eq!(
            profile_lookups("top gun movie cast"),
            ["top gun movie", "top gun"]
        );
        assert_eq!(
            profile_lookups("the office tv show imdb"),
            ["the office tv show", "the office"]
        );
        assert_eq!(
            profile_lookups("halo video game steam"),
            ["halo video game", "halo"]
        );
        assert_eq!(profile_lookups("top gun cast"), ["top gun"]);
        assert_eq!(profile_lookups("mrbeast movie youtube"), ["mrbeast movie"]);
        // With its kind, only a work called so: not the 1955 film a search
        // for "top gun movie" names, but "Bee Movie".
        let mut old = article("Top Gun", "Film by Henry Levin, 1955", None);
        old.page.set = FILMS_SET.to_string();
        assert!(profile_candidates("top gun movie cast", "top gun movie", vec![old]).is_empty());
        let bee = article("Bee Movie", "2007 animated film by Simon J. Smith", None);
        assert_eq!(
            profile_candidates("bee movie cast", "bee movie", vec![bee]).len(),
            1
        );
        // The head of a description says what kind of work it is on.
        let kept = profile_candidates(
            "top gun cast",
            "top gun",
            vec![
                article("Top Gun", "1986 action drama film by Tony Scott", None),
                article("Top Gun (video game)", "1987 video game", None),
                article("Top Gun (franchise)", "Action film franchise", None),
            ],
        );
        let titles: Vec<_> = kept.iter().map(|hit| hit.page.title.as_str()).collect();
        assert_eq!(titles, ["Top Gun"]);
        // No song has a film's listing.
        assert!(!searches_songs(
            "top gun movie cast",
            &EntityResolution::Unresolved
        ));
    }

    #[test]
    fn an_account_is_a_person_s_group_s_or_organization_s() {
        use plumb_core::facts::FactKind::{self, Born, Founded};
        let entity = |title: &str, description: &str, item: &str, facts: &[(FactKind, &str)]| {
            let mut hit = with_facts(article(title, description, None), facts);
            hit.page.item = Some(item.into());
            hit.named = true;
            hit
        };
        let resolved = |query: &str, name: &str, candidates: Vec<PageHit>| match resolve_entity(
            name,
            &profile_candidates(query, name, candidates),
            None,
        ) {
            EntityResolution::Resolved(page) => Some(page.title.clone()),
            _ => None,
        };
        let drake = vec![
            entity(
                "Drake (musician)",
                "Canadian rapper and singer (born 1986)",
                "Q33240",
                &[(Born, "1986-10-24")],
            ),
            entity("Drake (given name)", "Name list", "Q1254221", &[]),
        ];
        // Both named by "drake", so neither without the account asked for.
        assert!(matches!(
            resolve_entity("drake", &drake, None),
            EntityResolution::Ambiguous(_)
        ));
        assert_eq!(
            resolved("drake instagram", "drake", drake).as_deref(),
            Some("Drake (musician)")
        );
        // Over a namesake with the very title: the colour has no account.
        let pink = vec![
            entity("Pink", "Pale tint of red", "Q51101", &[]),
            entity(
                "Pink (singer)",
                "American singer and songwriter (born 1979)",
                "Q160009",
                &[(Born, "1979-09-08")],
            ),
        ];
        assert_eq!(
            resolved("pink instagram", "pink", pink.clone()).as_deref(),
            Some("Pink (singer)")
        );
        // A band has its founding.
        let queen = vec![
            entity(
                "Queen (band)",
                "British rock band",
                "Q15862",
                &[(Founded, "1970")],
            ),
            entity("Queen regnant", "Female monarch", "Q16511993", &[]),
        ];
        assert_eq!(
            resolved("queen x", "queen", queen).as_deref(),
            Some("Queen (band)")
        );
        // YouTube also has a song's video, so every namesake stays.
        assert_eq!(profile_candidates("pink youtube", "pink", pink).len(), 2);
        // With no person, group or organization, nothing is left out.
        let office = vec![
            entity(
                "The Office (American TV series)",
                "American mockumentary sitcom",
                "Q23831",
                &[],
            ),
            entity(
                "The Office (British TV series)",
                "British mockumentary sitcom",
                "Q1052839",
                &[],
            ),
        ];
        assert_eq!(
            profile_candidates("the office instagram", "the office", office).len(),
            2
        );
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
