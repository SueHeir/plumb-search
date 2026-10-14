//! Opening the page index and the searches callers make of it: pages,
//! entities, definitions, songs, spelling, and a site's popularity.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::paper_query::PaperQuery;
use plumb_core::{registrable_domain, Operators};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, ReloadPolicy, TantivyDocument, Term};

use super::building::schema;
use super::page::is_item_id;
use super::placing::{is_year, options_allow};
use super::records::records_named;
use super::scoring::truncate_keeping_inner_pages;
use super::sites::{base_title, describes_an_event};
use super::{
    Page, PageHit, PageIndexStats, PageSearcher, PaperCoverage, PlacedPage, ALIAS_MATCH,
    ASKING_WORDS, CANDIDATES, FILMS_SET, GITHUB_SET, KNOWN_WORD_PAGES, MIN_PAGE_FIX_PAGES,
    MUSIC_SET, PAGE_FIX_RATIO, PAGE_SLIP_WEIGHT, PAPERS_SET, POPULARITY_SCALE, POPULARITY_SHARE,
    TITLE_CANDIDATES, WIKIDATA_SET, WIKTIONARY_SET,
};
use crate::analysis;

/// What kind of page words around a name ask for ([`hinted_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hint {
    /// Its encyclopedia article: "lululemon wikipedia".
    Article,
    /// Its repository on GitHub: "tokio github".
    Repo,
    /// A paper of that title: "batch normalization paper".
    Paper,
    /// A work of one of these kinds of that name, of which something is
    /// asked: "titanic trailer", "let it be chords".
    Work(&'static [&'static str]),
    /// A film, show or game of that name when there is one, else
    /// whatever is named so: "titanic movie" for the 1997 film, not the
    /// ship, but "tom hanks movie" still for Tom Hanks.
    Screen,
    /// A person of that name, of whom something is asked: "obama
    /// daughters", "picasso paintings".
    Person,
    /// A monarch or pope of that name and number: "henry viii" of "henry
    /// the 8th" ([`regnal_name`]).
    Regnal,
    /// An event, competition, team or venue of that name, of which
    /// something is asked: "lakers schedule", "coachella lineup".
    Event,
    /// Whatever is named so: "who is galileo", "ariana grande age".
    Any,
    /// The thing, of which one part is asked about: "ibuprofen side
    /// effects", "volcano facts". A search for papers keeps those words
    /// ("loss function").
    Aspect,
    /// Only pages the name names: what a fact question asks about, or the
    /// list a search for one asks for ("list of popes" of "popes in
    /// order"). "madrid spanish" (of "madrid spanish capital city") names
    /// nothing, and the clubs it describes would push Madrid out.
    Named,
    /// Of the pages the name names, those on a field's sense of it, when
    /// there are any ([`plumb_core::article::field_asked`]): "what is a
    /// cell in biology" asks about the cell of biology.
    Field(&'static [&'static str]),
}

/// What a song is called in its title or description ([`Page::is_work_of`]).
const SONG: &[&str] = &["song", "single"];

/// What a film, show or game is called so.
const SCREEN: &[&str] = &[
    "film",
    "movie",
    "series",
    "miniseries",
    "sitcom",
    "show",
    "game",
];

/// Words before a name that ask who sang a song, where to see a film or
/// show, or how long one is. Longer leads come first.
const WORK_LEADS: &[(&str, &[&str])] = &[
    ("who sings the song ", SONG),
    ("who sang the song ", SONG),
    ("who sings ", SONG),
    ("who sang ", SONG),
    ("where to watch ", SCREEN),
    ("where can i watch ", SCREEN),
    ("where to stream ", SCREEN),
    ("where can i stream ", SCREEN),
    ("how many seasons of ", SCREEN),
    ("how many episodes of ", SCREEN),
];

/// Words before a name that ask about it. "What's" is read as "what is";
/// "whats" is not, since "whats app" is a misspelled name.
const HINT_LEADS: &[&str] = &[
    "who is ",
    "who was ",
    "who are ",
    "who were ",
    "tell me about ",
    "what is the meaning of ",
    "what is the definition of ",
    "what is a ",
    "what is an ",
    "what is the ",
    "what is ",
    "what are ",
    "what was ",
    "what were ",
    "who won the ",
    "who won ",
    "who wins the ",
    "who wins ",
    "who will win the ",
    "who will win ",
    "winner of the ",
    "winner of ",
    "winners of the ",
    "winners of ",
    "what does ",
    "define ",
    "definition of ",
    "meaning of ",
    "symptoms of ",
    "side effects of ",
    "benefits of ",
    "health benefits of ",
    "causes of ",
    "treatment for ",
    "treatment of ",
    "uses of ",
    "examples of ",
    "info about ",
    "information about ",
    "facts about ",
    "what causes a ",
    "what causes an ",
    "what causes the ",
    "what causes ",
    "where is the ",
    "where is ",
    "where are the ",
    "where are ",
    "how many people live in ",
];

/// What "how ... is" asks the measure of: "how tall is the eiffel tower".
const MEASURES: &[&str] = &[
    "tall", "old", "big", "long", "deep", "high", "far", "wide", "large", "heavy",
];

/// Questions that name what they ask about between a lead and a tail:
/// "refrigerator" of "how does a refrigerator work", "metabolism" of "how
/// metabolism works", "meiosis" of "where does meiosis occur", "coffee" of
/// "is coffee bad for you", "penicillin" of "who discovered penicillin",
/// "liver" of "what does the liver do", "taylor swift" of "where is taylor
/// swift from". The first pair a question has decides.
const ASKED_BETWEEN: &[(&[&str], &[&str])] = &[
    (&["how does ", "how do "], &[" work"]),
    (&["how "], &[" works", " work"]),
    (
        &["where does ", "where do "],
        &[" take place", " occur", " happen", " come from"],
    ),
    (&["where is ", "where are "], &[" from"]),
    (
        &["is ", "are "],
        &[
            " good for you",
            " bad for you",
            " healthy",
            " safe",
            " beneficial",
            " harmful",
            " dangerous",
            " effective",
        ],
    ),
    (&["does ", "do "], &[" really work", " work"]),
    (&["what does ", "what do "], &[" do"]),
    (&["who invented ", "who discovered "], &[""]),
    (&["when was ", "when were "], &[" invented", " discovered"]),
];

/// Words that make what a question names a clause or a pronoun, not a
/// name: "how do i make it work", "how many hours do nurses work".
const NOT_NAMES: &[&str] = &[
    "it", "this", "that", "they", "these", "those", "you", "i", "we", "he", "she", "to", "do",
    "does", "did", "is", "are", "can", "should", "many", "much", "long", "often",
];

/// The most words a question's name has: more are a clause.
const MAX_ASKED_WORDS: usize = 4;

/// What a question of how something works, how big it is, where it
/// happens or whether it is good for you asks about: "refrigerator" of
/// "how does a refrigerator work", "eiffel tower" of "how tall is the
/// eiffel tower". `None` for other questions ("how do i make pancakes",
/// where "how do" starts any question), and when what it names is not a
/// name ([`asked_name`]): "it", or a clause.
fn asked_how(q: &str) -> Option<&str> {
    let between = ASKED_BETWEEN.iter().find_map(|(leads, tails)| {
        let rest = leads.iter().find_map(|lead| q.strip_prefix(lead))?;
        tails.iter().find_map(|tail| rest.strip_suffix(tail))
    });
    let measured = || {
        let (measure, rest) = q.strip_prefix("how ")?.split_once(' ')?;
        let rest = ["is ", "are ", "was "]
            .iter()
            .find_map(|verb| rest.strip_prefix(verb))?;
        // "how tall is tom cruise in feet" and "how old is obama now" ask
        // about Tom Cruise and Obama.
        let rest = plumb_core::facts::without_size_unit(rest)
            .or_else(|| plumb_core::facts::without_now(rest))
            .unwrap_or(rest);
        MEASURES.contains(&measure).then_some(rest)
    };
    asked_name(between.or_else(measured)?)
}

/// `text` as the name a question asks about, without a leading "a", "an"
/// or "the". `None` when it is empty, has more than [`MAX_ASKED_WORDS`]
/// words or has one of [`NOT_NAMES`].
fn asked_name(text: &str) -> Option<&str> {
    let name = ["a ", "an ", "the "]
        .iter()
        .find_map(|article| text.strip_prefix(article))
        .unwrap_or(text);
    let words = || name.split(' ');
    (!name.is_empty()
        && words().count() <= MAX_ASKED_WORDS
        && !words().any(|word| NOT_NAMES.contains(&word)))
    .then_some(name)
}

/// Words of a title that only set two things side by side: "the
/// difference between mitosis and meiosis".
const COMPARING_WORDS: &[&str] = &[
    "vs",
    "versus",
    "v",
    "difference",
    "differences",
    "between",
    "compared",
    "comparison",
    "better",
    "or",
];

/// The two things a comparison sets side by side, each looked up as a
/// name: "mitosis" and "meiosis" of "mitosis vs meiosis" or "difference
/// between mitosis and meiosis". `None` for other queries, and when a
/// side is not a name ([`asked_name`]).
pub(crate) fn compared(query: &str) -> Option<[String; 2]> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches('?'))
        .to_lowercase()
        .replace('’', "'");
    let (a, b) = [
        "what is the difference between ",
        "what's the difference between ",
        "difference between ",
    ]
    .iter()
    .find_map(|lead| q.strip_prefix(lead)?.split_once(" and "))
    .or_else(|| {
        [" vs. ", " vs ", " versus "]
            .iter()
            .find_map(|word| q.split_once(word))
    })?;
    let side = |text: &str| asked_name(text.trim()).map(str::to_string);
    Some([side(a)?, side(b)?])
}

/// Words before a name that ask for one side of it: "benefits of
/// meditation", "uses of graphene". Unlike [`HINT_LEADS`], they also start
/// the names of articles on that side ("Causes of the French Revolution"),
/// so the name is looked up only when nothing is called the whole query.
const ASPECT_LEADS: &[&str] = &[
    "benefits of ",
    "health benefits of ",
    "uses of ",
    "importance of ",
    "characteristics of ",
    "advantages and disadvantages of ",
    "advantages of ",
    "disadvantages of ",
    "pros and cons of ",
    "effects of ",
    "side effects of ",
    "examples of ",
    "types of ",
    "causes of ",
    "symptoms of ",
    "history of ",
];

/// The name an [`ASPECT_LEADS`] query asks about: "meditation" of
/// "benefits of meditation".
pub(crate) fn aspect_asked(query: &str) -> Option<String> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches('?')).to_lowercase();
    asked_name(ASPECT_LEADS.iter().find_map(|lead| q.strip_prefix(lead))?).map(str::to_string)
}

/// Words after a name that say what about it is wanted.
const HINT_TAILS: &[(&str, Hint)] = &[
    (" wikipedia", Hint::Article),
    (" wiki", Hint::Article),
    (" github repo", Hint::Repo),
    (" github", Hint::Repo),
    (" repository", Hint::Repo),
    (" repo", Hint::Repo),
    (" paper", Hint::Paper),
    (" papers", Hint::Paper),
    (" arxiv", Hint::Paper),
    (" winner", Hint::Any),
    (" winners", Hint::Any),
    (" summary", Hint::Any),
    (" plot", Hint::Any),
    (" biography", Hint::Any),
    (" bio", Hint::Any),
    (" age", Hint::Any),
    (" birthday", Hint::Any),
    (" height", Hint::Any),
    (" net worth", Hint::Any),
    (" wife", Hint::Any),
    (" husband", Hint::Any),
    // What people ask about a person, which a person's article answers but
    // whose words it is not called by. "Cause of death" before "death".
    (" cause of death", Hint::Person),
    (" death", Hint::Person),
    (" kids", Hint::Person),
    (" children", Hint::Person),
    (" daughter", Hint::Person),
    (" daughters", Hint::Person),
    (" son", Hint::Person),
    (" sons", Hint::Person),
    (" parents", Hint::Person),
    (" mother", Hint::Person),
    (" father", Hint::Person),
    (" siblings", Hint::Person),
    (" family", Hint::Person),
    (" girlfriend", Hint::Person),
    (" boyfriend", Hint::Person),
    (" education", Hint::Person),
    (" nationality", Hint::Person),
    (" religion", Hint::Person),
    (" awards", Hint::Person),
    (" stats", Hint::Person),
    (" movies", Hint::Person),
    (" films", Hint::Person),
    (" books", Hint::Person),
    (" novels", Hint::Person),
    (" poems", Hint::Person),
    (" songs", Hint::Person),
    (" albums", Hint::Person),
    (" paintings", Hint::Person),
    (" inventions", Hint::Person),
    // What people ask of a song or a film, show or game, whose article is
    // not called by these words. Longer tails first: "hey jude piano
    // chords" is not about a song "hey jude piano".
    (" song lyrics", Hint::Work(SONG)),
    (" lyrics", Hint::Work(SONG)),
    (" guitar chords", Hint::Work(SONG)),
    (" piano chords", Hint::Work(SONG)),
    (" ukulele chords", Hint::Work(SONG)),
    (" chords", Hint::Work(SONG)),
    (" guitar tabs", Hint::Work(SONG)),
    (" guitar tab", Hint::Work(SONG)),
    (" karaoke", Hint::Work(SONG)),
    (" music video", Hint::Work(SONG)),
    (" official trailer", Hint::Work(SCREEN)),
    (" trailer", Hint::Work(SCREEN)),
    (" box office", Hint::Work(SCREEN)),
    (" reviews", Hint::Work(SCREEN)),
    (" review", Hint::Work(SCREEN)),
    (" release date", Hint::Work(SCREEN)),
    (" ending explained", Hint::Work(SCREEN)),
    (" ending", Hint::Work(SCREEN)),
    (" finale", Hint::Work(SCREEN)),
    (" characters", Hint::Work(SCREEN)),
    (" actors", Hint::Work(SCREEN)),
    // What people ask of an event, competition, team or venue, whose
    // article is not called by these words. Longer tails first.
    (" schedule", Hint::Event),
    (" fixtures", Hint::Event),
    (" standings", Hint::Event),
    (" table", Hint::Event),
    (" live scores", Hint::Event),
    (" live score", Hint::Event),
    (" scores", Hint::Event),
    (" score", Hint::Event),
    (" results", Hint::Event),
    (" bracket", Hint::Event),
    (" tickets", Hint::Event),
    (" lineup", Hint::Event),
    (" roster", Hint::Event),
    (" squad", Hint::Event),
    (" highlights", Hint::Event),
    (" live stream", Hint::Event),
    (" odds", Hint::Event),
    (" draw", Hint::Event),
    (" predictions", Hint::Event),
    (" start time", Hint::Event),
    (" cast", Hint::Any),
    (" author", Hint::Any),
    (" movie", Hint::Screen),
    (" film", Hint::Any),
    (" meaning", Hint::Any),
    (" mean", Hint::Any),
    (" means", Hint::Any),
    (" definition", Hint::Any),
    (" stand for", Hint::Any),
    (" stands for", Hint::Any),
    (" explained", Hint::Any),
    (" quotes", Hint::Any),
    // The article on a drug, an illness or a thing of science says what
    // these ask, and none of the words: "melatonin dosage", "shingles
    // symptoms", "photosynthesis equation".
    (" side effects", Hint::Aspect),
    (" symptoms", Hint::Aspect),
    (" dosage", Hint::Aspect),
    (" dose", Hint::Aspect),
    (" health benefits", Hint::Aspect),
    (" benefits", Hint::Aspect),
    (" treatment", Hint::Aspect),
    (" causes", Hint::Aspect),
    (" facts", Hint::Aspect),
    (" uses", Hint::Aspect),
    (" diagram", Hint::Aspect),
];

/// Words before a name that ask what is usual or wise for it, not what it
/// is: "normal" of "normal blood pressure".
const JUDGING_WORDS: &[&str] = &[
    "normal", "good", "bad", "healthy", "average", "ideal", "typical", "safe",
];

/// `name` without a word in front that judges it ([`JUDGING_WORDS`]):
/// "blood pressure" of "normal blood pressure". `None` without one, or
/// when a single word is left, which is as often initials: "healthy bmi"
/// would name BMI, a research firm.
fn judged_name(name: &str) -> Option<&str> {
    let (first, rest) = name.split_once(' ')?;
    (JUDGING_WORDS.contains(&first) && rest.contains(' ')).then_some(rest)
}

/// What a search asks of a thing of science, which the thing's article
/// says, before its name with "of" or after it: "melting point of iron",
/// "iron melting point". Longer first: "glucose chemical formula" is not
/// about a "glucose chemical". Those a fact question asks ("atomic number
/// of gold", "area of france", "radius of mars") are left to
/// [`plumb_core::facts::fact_asked`].
const PROPERTIES: &[&str] = &[
    "melting point",
    "boiling point",
    "freezing point",
    "chemical formula",
    "formula",
    "equation",
    "structure",
    "function",
    "molar mass",
    "atomic mass",
    "atomic weight",
    "density",
    "half life",
    "half-life",
];

/// Properties asked only before the name: "temperature of the sun",
/// "volume of a sphere", but "oven temperature" is about an oven's
/// setting and "sound volume" about a knob.
const PROPERTIES_OF: &[&str] = &[
    "mass",
    "diameter",
    "temperature",
    "ph",
    "volume",
    "surface area",
    "perimeter",
    "circumference",
];

/// The thing `q` asks a property of ([`PROPERTIES`]): "iron" of "melting
/// point of iron" and of "iron melting point", "sun" of "mass of the sun".
fn property_of(q: &str) -> Option<&str> {
    let before = PROPERTIES
        .iter()
        .chain(PROPERTIES_OF)
        .find_map(|property| q.strip_prefix(property)?.strip_prefix(" of "));
    let thing = before.or_else(|| {
        PROPERTIES
            .iter()
            .find_map(|property| q.strip_suffix(property)?.strip_suffix(' '))
    })?;
    Some(
        ["the ", "a ", "an "]
            .iter()
            .find_map(|article| thing.strip_prefix(article))
            .unwrap_or(thing),
    )
}

/// The list article a search for a list asks for without saying "list
/// of": "list of popes" of "popes in order", "list of us presidents" of
/// "us presidents list". `None` for other searches.
fn list_asked(query: &str) -> Option<String> {
    let q = plumb_core::collapse_whitespace(query.trim()).to_lowercase();
    let name = [" list in order", " in order", " list"]
        .iter()
        .find_map(|tail| q.strip_suffix(tail))?
        .trim();
    let name = name.strip_prefix("list of ").unwrap_or(name);
    (!name.is_empty()).then(|| format!("list of {name}"))
}

/// The name in `query` without the words around it that only say what is
/// wanted, and what kind of page they ask for: "lululemon" of "lululemon
/// wikipedia". `None` when there are none, or nothing else.
pub(crate) fn hinted_name(query: &str) -> Option<(String, Hint)> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches('?'))
        .to_lowercase()
        .replace('’', "'");
    let asked = q
        .strip_prefix("what's ")
        .map(|rest| format!("what is {rest}"));
    let mut name = asked.as_deref().unwrap_or(&q);
    let mut hint = None;
    if let Some(rest) =
        asked_how(name).or_else(|| HINT_LEADS.iter().find_map(|lead| name.strip_prefix(lead)))
    {
        name = rest;
        hint = Some(Hint::Any);
    } else if let Some((rest, kinds)) = WORK_LEADS
        .iter()
        .find_map(|(lead, kinds)| Some((name.strip_prefix(lead)?, *kinds)))
    {
        name = rest;
        hint = Some(Hint::Work(kinds));
    }
    if let Some((rest, tail_hint)) = HINT_TAILS
        .iter()
        .find_map(|(tail, h)| Some((name.strip_suffix(tail)?, *h)))
        .or_else(|| Some((property_of(name)?, Hint::Aspect)))
    {
        name = rest;
        hint = Some(tail_hint);
    }
    // "how did steve jobs die", "what is einstein known for": what an
    // article tells, asked as a question.
    let asked_of = [
        ("how did ", " die", Hint::Person),
        ("what did ", " die of", Hint::Person),
        ("what did ", " die from", Hint::Person),
        ("how many kids does ", " have", Hint::Person),
        ("how many kids did ", " have", Hint::Person),
        ("how many children does ", " have", Hint::Person),
        ("how many children did ", " have", Hint::Person),
        ("where was ", " buried", Hint::Person),
        ("", " buried", Hint::Person),
        ("what did ", " invent", Hint::Person),
        ("what did ", " discover", Hint::Person),
        ("why is ", " famous", Hint::Any),
        ("why was ", " famous", Hint::Any),
        ("", " known for", Hint::Any),
        ("", " famous for", Hint::Any),
        ("", " real name", Hint::Person),
        ("", " full name", Hint::Person),
        ("", " birth name", Hint::Person),
        ("", " iq", Hint::Person),
    ];
    if let Some((rest, asked)) = asked_of.iter().find_map(|(lead, tail, asked)| {
        Some((name.strip_prefix(lead)?.strip_suffix(tail)?, *asked))
    }) {
        name = rest;
        hint = Some(asked);
    }
    let name = name.trim();
    // "obama's daughters" and "einstein's biography" ask about Obama and
    // Einstein, whom "obama's" does not name. A name only asked about
    // keeps it: "what is mcdonald's".
    let name = match hint {
        Some(Hint::Person | Hint::Any) if !q.ends_with(name) => {
            name.strip_suffix("'s").unwrap_or(name)
        }
        _ => name,
    };
    let hint = hint?;
    let (name, hint) = match (hint, plumb_core::article::field_asked(name)) {
        (Hint::Any, Some((rest, marks))) => (rest, Hint::Field(marks)),
        _ => (name, hint),
    };
    (!name.is_empty() && name != q).then(|| (name.to_string(), hint))
}

/// `name` with the year at one of its ends moved to the other: "2025 tour
/// de france" of "tour de france 2025". `None` without one, or with a year
/// alone.
pub(crate) fn year_moved(name: &str) -> Option<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.as_slice() {
        [first, rest @ ..] if !rest.is_empty() && is_year(first) => {
            Some(format!("{} {first}", rest.join(" ")))
        }
        [rest @ .., last] if !rest.is_empty() && is_year(last) => {
            Some(format!("{last} {}", rest.join(" ")))
        }
        _ => None,
    }
}

/// Words after a name that ask for the list of the works of an artist,
/// actor or writer, with the titles of Wikipedia's lists of them, best
/// first, `{}` for the name: "drake albums" for "Drake albums
/// discography", "tom hanks movies" for "Tom Hanks filmography".
const WORK_LISTS: &[(&str, &[&str])] = &[
    ("albums", &["{} albums discography", "{} discography"]),
    ("discography", &["{} discography", "{} albums discography"]),
    ("singles", &["{} singles discography", "{} discography"]),
    (
        "songs",
        &[
            "List of songs recorded by {}",
            "{} singles discography",
            "{} discography",
        ],
    ),
    ("movies", &["{} filmography"]),
    ("films", &["{} filmography"]),
    ("filmography", &["{} filmography"]),
    ("books", &["{} bibliography"]),
    ("novels", &["{} bibliography"]),
    ("bibliography", &["{} bibliography"]),
];

/// The titles of the lists of works `query` asks for ([`WORK_LISTS`]),
/// best first: "Pink Floyd discography" for "pink floyd albums". The
/// name may be said without "the": "beatles songs".
pub(crate) fn work_lists(query: &str) -> Vec<String> {
    let query = plumb_core::collapse_whitespace(query.trim()).to_lowercase();
    let Some((artist, asked)) = query.rsplit_once(' ') else {
        return Vec::new();
    };
    let Some((_, titles)) = WORK_LISTS.iter().find(|(word, _)| *word == asked) else {
        return Vec::new();
    };
    let mut artists = vec![artist.to_string()];
    if !artist.starts_with("the ") {
        artists.push(format!("the {artist}"));
    }
    titles
        .iter()
        .flat_map(|title| {
            artists
                .iter()
                .map(move |artist| title.replace("{}", artist))
        })
        .collect()
}

/// Adds `found` to `hits`. Found both ways, a page keeps its better score
/// and whatever either way says of it.
fn keep_found(hits: &mut Vec<PageHit>, found: Vec<PageHit>) {
    for hit in found {
        match hits.iter_mut().find(|h| h.page == hit.page) {
            Some(kept) => {
                kept.named |= hit.named;
                kept.whole |= hit.whole;
                kept.score = kept.score.max(hit.score);
            }
            None => hits.push(hit),
        }
    }
}

/// Least listeners of a song or album that [`PageSearcher::known_song`]
/// takes a search of its title alone to be for: about the 7,000 most
/// listened.
pub const KNOWN_SONG_LISTENERS: u64 = 50_000;

/// How many times the listeners of every other song or album of its title
/// such a one has.
pub const KNOWN_SONG_MARGIN: u64 = 5;

/// How many of the first sites [`PageSearcher::note_demand`] notes.
pub const DEMAND_NOTED: usize = 3;

/// Pages looked at for one a suggested spelling names
/// ([`PageSearcher::list_suggested_page`]).
const SUGGESTED_PAGES: usize = 10;

/// Least popularity of an article whose name a query's first words are
/// taken for a slip of ([`PageSearcher::fix_page_name`]): about 6,000
/// reads in the days Wikipedia's counts cover. Stephen Hawking has seven
/// times that; Stephen Hawkins, a name of its own, has 48.
const WELL_READ_NAME: f32 = 0.55;

/// Most of a query's first words read as one name
/// ([`PageSearcher::fix_page_name`]).
const MAX_NAME_WORDS: usize = 4;

/// Most read pages looked at for an article with a name's key.
const NAME_PAGES: usize = 5;

impl PageSearcher {
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)
            .with_context(|| format!("opening the page index in {}", dir.display()))?;
        analysis::register(index.tokenizers());
        let (schema, fields) = schema();
        if index.schema() != schema {
            bail!(
                "the page index in {} was built by another version; rebuild it",
                dir.display()
            );
        }
        let stats: PageIndexStats = serde_json::from_slice(
            &std::fs::read(dir.join("pages.json"))
                .with_context(|| format!("reading {}/pages.json", dir.display()))?,
        )?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(PageSearcher {
            reader,
            fields,
            words: analysis::words_analyzer(),
            joined: analysis::joined_analyzer(),
            stemmed: analysis::stemmed_analyzer(),
            stats,
            requested_language: None,
        })
    }

    pub fn num_pages(&self) -> u64 {
        self.stats.pages
    }

    pub fn paper_coverage(&self) -> &PaperCoverage {
        &self.stats.papers
    }

    /// A lightweight view restricted to declared/set-supported language
    /// before each candidate collector's cap. The existing English path
    /// retains old records with unknown language, still labeled unknown;
    /// other languages require actual metadata. Invalid tags match nothing.
    pub fn in_language(&self, language: Option<&str>) -> Self {
        Self {
            reader: self.reader.clone(),
            fields: self.fields,
            words: self.words.clone(),
            joined: self.joined.clone(),
            stemmed: analysis::content_analyzer(
                language.and_then(plumb_core::language_code).as_deref(),
            ),
            stats: self.stats.clone(),
            requested_language: language
                .map(|tag| plumb_core::language_code(tag).unwrap_or_else(|| "invalid".to_string())),
        }
    }

    /// Actual indexed language counts, for explaining empty constrained
    /// results without claiming that the requested pages do not exist.
    pub fn language_coverage(&self) -> (&std::collections::BTreeMap<String, u64>, u64) {
        let known = self
            .stats
            .languages
            .values()
            .fold(0u64, |count, pages| count.saturating_add(*pages));
        (
            &self.stats.languages,
            self.stats.pages.saturating_sub(known),
        )
    }

    pub(crate) fn language_query(&self, query: &dyn Query) -> Box<dyn Query> {
        match &self.requested_language {
            None => query.box_clone(),
            Some(language) => {
                let mut allowed = vec![(
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.language, language),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )];
                if language == "en" {
                    allowed.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.language, "unknown"),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, query.box_clone()),
                    (
                        Occur::Must,
                        Box::new(ConstScoreQuery::new(
                            Box::new(BooleanQuery::new(allowed)),
                            0.0,
                        )),
                    ),
                ]))
            }
        }
    }

    /// How much the most read Wikipedia article about what `domain` is the
    /// official website of is read, as [`PageHit::popularity`]: how much
    /// people look up what the site is. `None` when no article is about it.
    pub fn site_popularity(&self, domain: &str) -> Result<Option<f32>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.site, domain),
            IndexRecordOption::Basic,
        );
        let best = searcher.search(
            &query,
            &TopDocs::with_limit(1).order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
        )?;
        Ok(best
            .first()
            .and_then(|&(popularity, _)| popularity)
            .map(|popularity| popularity as f32 / POPULARITY_SCALE))
    }

    /// Notes on the first [`DEMAND_NOTED`] of `sites` how much the article
    /// about each is read ([`PageSearcher::site_popularity`],
    /// [`crate::Hit::demand`]), for [`place_pages`](super::place_pages) to
    /// weigh against the pages a query names. A few, not one: a node may
    /// reorder the first results for its searcher after.
    pub fn note_demand(&self, sites: &mut [crate::Hit]) -> Result<()> {
        for site in sites.iter_mut().take(DEMAND_NOTED) {
            site.demand = self.site_popularity(&site.domain)?;
        }
        Ok(())
    }

    /// Whether `word` (one word of [`analysis::words_analyzer`]) is in the
    /// names of at least [`KNOWN_WORD_PAGES`] pages.
    pub fn knows_word(&self, word: &str) -> Result<bool> {
        let searcher = self.reader.searcher();
        Ok(searcher.doc_freq(&Term::from_field_text(self.fields.words, word))? >= KNOWN_WORD_PAGES)
    }

    /// `spelling`, suggested for `query`, with the words the pages know
    /// ([`PageSearcher::knows_word`]) put back as typed; `None` when no
    /// change is left. The sites index hardly knows "pkce", "dplyr" or
    /// "stain", so it suggests "pace", "plyr" and "spain"; pages name them.
    /// A word the pages know only as a misspelling of the word suggested
    /// ([`PageSearcher::misspells`]) is not put back. A correction to a
    /// site's name ([`crate::Spelling::site`]) is kept: "wels fargo" is a
    /// typo even though Wels is a town.
    pub fn check_spelling(
        &self,
        query: &str,
        spelling: crate::Spelling,
    ) -> Result<Option<crate::Spelling>> {
        if spelling.site.is_some() {
            return Ok(Some(spelling));
        }
        let typed = analysis::tokens(&self.words, query);
        let mut fixed: Vec<String> = spelling
            .query
            .split_whitespace()
            .map(String::from)
            .collect();
        if typed.len() == fixed.len() {
            for (word, fix) in typed.iter().zip(fixed.iter_mut()) {
                if word != fix && self.knows_word(word)? && !self.misspells(word, fix)? {
                    fix.clone_from(word);
                }
            }
        } else {
            // Words were split or joined, so they can't be put back one by
            // one: a known word among those changed drops the suggestion.
            for word in &typed {
                if !fixed.contains(word) && self.knows_word(word)? {
                    return Ok(None);
                }
            }
        }
        if fixed == typed {
            return Ok(None);
        }
        Ok(Some(crate::Spelling {
            query: fixed.join(" "),
            ..spelling
        }))
    }

    /// Whether `word` is a misspelling of `fix` that Wikipedia keeps as a
    /// redirect: the most read article with the word among its names is
    /// titled with `fix` and not with the word. "mississipi" is in the
    /// names of ten pages, but only as a redirect to Mississippi and the
    /// like; "stain" is the title of Stain, not a misspelling of "spain".
    fn misspells(&self, word: &str, fix: &str) -> Result<bool> {
        let searcher = self.reader.searcher();
        let named = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.words, word),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (Occur::Must, self.scope_term("set:wikipedia-en")),
        ]);
        let top =
            TopDocs::with_limit(1).order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let Some((_, address)) = searcher.search(&named, &top)?.pop() else {
            return Ok(false);
        };
        let document: TantivyDocument = searcher.doc(address)?;
        let Some(stored) = document
            .get_first(self.fields.page)
            .and_then(|v| v.as_str())
        else {
            return Ok(false);
        };
        let page: Page = serde_json::from_str(stored)?;
        let title = analysis::tokens(&self.words, &page.title);
        Ok(title.iter().any(|t| t == fix) && !title.iter().any(|t| t == word))
    }

    /// Whether the pages `found` for `query` say it is spelled as meant, so
    /// that no other spelling is suggested: one is a package, holds all of
    /// a question's main words ("git undo last commit" is not "git und") or
    /// is named by it ("serde crate" is not "serde create"). A page does
    /// not count when `fix`, the spelling suggested, names it before the
    /// query does, by its title or by another name read more (the most
    /// read come first): Wikipedia keeps the misspellings people type as
    /// redirects, so "einstien" names Albert Einstein, but after
    /// "Einstein".
    pub fn spelled_right(
        &self,
        query: &str,
        found: &[PageHit],
        fix: Option<&crate::Spelling>,
    ) -> bool {
        let key = |text: &str| {
            analysis::tokens(&self.joined, text)
                .pop()
                .unwrap_or_default()
        };
        let typed = key(query);
        let fixed = fix
            .map(|fix| key(&fix.query))
            .filter(|fixed| *fixed != typed);
        let misnamed = |page: &Page| {
            fixed.as_ref().is_some_and(|fixed| {
                [page.title.as_str(), base_title(&page.title)]
                    .into_iter()
                    .chain(page.aliases.iter().map(String::as_str))
                    .map(key)
                    .find(|name| *name == typed || name == fixed)
                    .is_some_and(|name| name == *fixed)
            })
        };
        found
            .iter()
            .any(|hit| hit.page.package.is_some() || hit.whole || hit.named && !misnamed(&hit.page))
    }

    /// A suggested spelling of `query` from the words of the pages' names,
    /// for queries about things rather than sites ("anubas", "budafest"),
    /// whose words the sites index hardly has. Each word the pages do not
    /// know ([`PageSearcher::knows_word`]) and `site_known` does not either
    /// is replaced by the near word found in the most pages, at least
    /// [`PAGE_FIX_RATIO`] times as many as have the word typed and at least
    /// [`MIN_PAGE_FIX_PAGES`], or with the spelling model, the likeliest by
    /// the noisy channel ([`crate::spell_model`]). With the model, a word
    /// is kept unless the slip is likelier than the word, a word no page
    /// has counting as if one did ("perft" is not "perf"), or the slip is
    /// one the model learned and is likelier than the word with how much
    /// commoner the near word is weighed by [`PAGE_SLIP_WEIGHT`]; that
    /// holds for a word the pages know too ("recieve", "definately").
    /// A word typed in capitals, with letters English does not use, or in
    /// a web address is kept, and so is a word that names something
    /// ([`PageSearcher::names_something`]), however few pages have it:
    /// "magicka" is not "magick", nor "baidu baike" "baidu bake".
    /// `None` when no word changes.
    pub fn suggest_spelling(
        &self,
        query: &str,
        model: Option<&crate::spell_model::Model>,
        site_known: &dyn Fn(&str) -> bool,
    ) -> Result<Option<crate::Spelling>> {
        // A web address is a name, not words: "amazn.com" is not "amazon
        // com". The sites suggest the address it is a slip of.
        if crate::names::typed_domain(query).is_some() {
            return Ok(None);
        }
        let searcher = self.reader.searcher();
        let docs = |word: &str| -> Result<u64> {
            Ok(searcher.doc_freq(&Term::from_field_text(self.fields.words, word))?)
        };
        let typed = analysis::tokens(&self.words, query);
        // Names, abbreviations and addresses: "ETIAS" is not "elias", "OLED"
        // not "old", nor "ratp.info" "rate info". Nor is a word typed with
        // letters English does not use a slip of one without them: "Edlén"
        // is not "ellen", nor the mojibake "mÃ©xico" "ma xiao".
        let mut kept: HashSet<String> = HashSet::new();
        for part in query.split_whitespace() {
            let whole = part.contains('.') && registrable_domain(part).is_some()
                || part.chars().any(|c| c.is_alphabetic() && !c.is_ascii());
            for piece in part.split(|c: char| !c.is_alphanumeric()) {
                let capitals = piece.chars().filter(|c| c.is_alphabetic()).count() > 1
                    && !piece.chars().any(char::is_lowercase);
                if whole || capitals {
                    kept.extend(analysis::tokens(&self.words, piece));
                }
            }
        }
        let mut fixed = typed.clone();
        let named = self.fix_page_name(&mut fixed, model)?;
        for word in fixed.iter_mut().skip(named) {
            let chars = word.chars().count();
            let edits = crate::spell::max_edits(chars);
            if edits == 0
                || !word.chars().all(char::is_alphabetic)
                || site_known(word)
                || kept.contains(word.as_str())
            {
                continue;
            }
            let known = self.knows_word(word)?;
            if known && model.is_none() {
                continue;
            }
            let typed_docs = docs(word)?;
            let needed = MIN_PAGE_FIX_PAGES.max(PAGE_FIX_RATIO.saturating_mul(typed_docs));
            let mut best: Option<(f64, String)> = None;
            for (term, distance) in
                crate::spell::near_terms(&searcher, self.fields.words, word, edits)?
            {
                if !crate::spell::plausible_word_fix(word, &term) {
                    continue;
                }
                let term_docs = docs(&term)?;
                if term_docs < needed {
                    continue;
                }
                // Without a model, an edit costs as much as a thousand
                // times the pages.
                let cost = match model {
                    Some(model) => model.ln_channel(word, &term),
                    None => -7.0 * f64::from(distance),
                };
                let likelihood = cost + (term_docs as f64).ln();
                if let Some(model) = model {
                    let commoner = (term_docs as f64 / typed_docs.max(1) as f64).ln();
                    // As likely as the word typed, a word no page has
                    // counting as if one did: "perft" is not "perf".
                    let rare = !known && cost + commoner >= 0.0;
                    let slip =
                        model.learned(word, &term) && cost + PAGE_SLIP_WEIGHT * commoner >= 0.0;
                    if !rare && !slip {
                        continue;
                    }
                }
                if best.as_ref().is_none_or(|(b, _)| likelihood > *b) {
                    best = Some((likelihood, term));
                }
            }
            if let Some((_, term)) = best {
                if !self.names_something(word, &typed)? {
                    *word = term;
                }
            }
        }
        Ok((fixed != typed).then(|| crate::Spelling {
            query: fixed.join(" "),
            site: None,
            applied: false,
        }))
    }

    /// Lists second among `placed` the page `spelling` names, as the site a
    /// suggestion names is ([`crate::suggested_site_second`]): "isaac
    /// newtonn" lists Isaac Newton under what has the letters typed rather
    /// than a click away. Not when the results are for the spelling
    /// already, the page is listed, or `options` leave it out. It counts
    /// as named by the spelling, not the query, so it gets no info box.
    pub fn list_suggested_page(
        &self,
        spelling: &crate::Spelling,
        options: &crate::SearchOptions,
        placed: &mut Vec<PlacedPage>,
    ) -> Result<()> {
        if spelling.applied {
            return Ok(());
        }
        let found = self.search(&spelling.query, SUGGESTED_PAGES)?;
        let Some(mut hit) = found.into_iter().find(|hit| {
            hit.named
                && options_allow(options, &hit.page)
                && crate::health::page_allowed(&spelling.query, &hit.page)
        }) else {
            return Ok(());
        };
        if placed.iter().any(|p| p.hit.page.url == hit.page.url) {
            return Ok(());
        }
        hit.named = false;
        let at = placed
            .iter()
            .position(|p| p.under.is_none() && p.at >= 1)
            .unwrap_or(placed.len());
        placed.insert(
            at,
            PlacedPage {
                hit,
                under: None,
                at: 1,
            },
        );
        Ok(())
    }

    /// Corrects the first words of `words`, two or more, when they are a
    /// slip of a well-read article's name ([`WELL_READ_NAME`]): "jeff bezoz"
    /// is Jeff Bezos, "leonardo da vinchi net worth" Leonardo da Vinci's. As
    /// the sites' speller corrects a site's name: the joined words are within
    /// [`crate::spell::max_edits`] of the name's, and each word changed is
    /// within its own, keeps its first letter and has no digits ("iphone 15"
    /// is not iPhone 16). Fewest edits first, then the likeliest slip by
    /// `model`. How many words it corrected: none when they name a page as
    /// typed.
    fn fix_page_name(
        &self,
        words: &mut [String],
        model: Option<&crate::spell_model::Model>,
    ) -> Result<usize> {
        let searcher = self.reader.searcher();
        for covers in (2..=words.len().min(MAX_NAME_WORDS)).rev() {
            let key = words[..covers].concat();
            if searcher.doc_freq(&Term::from_field_text(self.fields.keys, &key))? > 0 {
                return Ok(0);
            }
            let edits = crate::spell::max_edits(key.chars().count());
            let mut near = crate::spell::near_terms(&searcher, self.fields.keys, &key, edits)?;
            let cost = |name: &str| model.map_or(0.0, |model| model.ln_channel(&key, name));
            near.sort_by(|a, b| a.1.cmp(&b.1).then(cost(&b.0).total_cmp(&cost(&a.0))));
            for (name, _) in near {
                if let Some(fixed) = self.well_read_name(&name, &words[..covers])? {
                    words[..covers].clone_from_slice(&fixed);
                    return Ok(covers);
                }
            }
        }
        Ok(0)
    }

    /// The words of the name keyed `key` of the most read article that has
    /// it, when that article is read at least [`WELL_READ_NAME`] and each of
    /// its words is `typed`'s or a slip of it ([`PageSearcher::fix_page_name`]).
    fn well_read_name(&self, key: &str, typed: &[String]) -> Result<Option<Vec<String>>> {
        let searcher = self.reader.searcher();
        let named = TermQuery::new(
            Term::from_field_text(self.fields.keys, key),
            IndexRecordOption::Basic,
        );
        let top = TopDocs::with_limit(NAME_PAGES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let slip = |word: &String, fix: &String| {
            word == fix
                || word.chars().chain(fix.chars()).all(char::is_alphabetic)
                    && crate::spell::within_edits(word, fix)
                    && crate::spell::plausible_word_fix(word, fix)
        };
        for (popularity, address) in searcher.search(self.language_query(&named).as_ref(), &top)? {
            if (popularity.unwrap_or(0) as f32 / POPULARITY_SCALE) < WELL_READ_NAME {
                break;
            }
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if !page.is_article() {
                continue;
            }
            let names = [page.title.as_str(), base_title(&page.title)]
                .into_iter()
                .chain(page.names.iter().map(String::as_str));
            for name in names {
                if analysis::tokens(&self.joined, name).pop().as_deref() != Some(key) {
                    continue;
                }
                let fixed = analysis::tokens(&self.words, name);
                if fixed.len() == typed.len() && typed.iter().zip(&fixed).all(|(w, f)| slip(w, f)) {
                    return Ok(Some(fixed));
                }
            }
        }
        Ok(None)
    }

    /// Whether `word`, typed among `words`, is the name of something rather
    /// than a slip, however near a commoner word it is: Wiktionary has it as
    /// a word ("saute", "renter") or has its singular ("cloths": the set
    /// leaves out forms of other words), a page is named by it ("Everlong",
    /// "htop", "Scald" for Burn), or a page's name has it with another word
    /// typed ("Jean Meeus", "Baidu Baike"). A near spelling of an article's
    /// title is not its name but the redirect Wikipedia keeps for a
    /// misspelling ("Arguement" for Argument).
    fn names_something(&self, word: &str, words: &[String]) -> Result<bool> {
        let searcher = self.reader.searcher();
        let term = |field, text: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, text),
                IndexRecordOption::Basic,
            ))
        };
        let found = |clauses: Vec<Box<dyn Query>>| -> Result<bool> {
            let query = BooleanQuery::new(clauses.into_iter().map(|q| (Occur::Must, q)).collect());
            Ok(searcher.search(&query, &Count)? > 0)
        };
        let plural = word.strip_suffix('s').filter(|one| !one.ends_with('s'));
        for word in std::iter::once(word).chain(plural) {
            if searcher.doc_freq(&Term::from_field_text(self.fields.word, word))? > 0 {
                return Ok(true);
            }
        }
        let top = TopDocs::with_limit(TITLE_CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        for (_, address) in searcher.search(term(self.fields.keys, word).as_ref(), &top)? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            let title = analysis::tokens(&self.joined, base_title(&page.title))
                .pop()
                .unwrap_or_default();
            // An article is also keyed by the names that lead to it, some
            // of them misspellings of its title.
            if title == word || page.is_article() && !crate::spell::within_edits(word, &title) {
                return Ok(true);
            }
        }
        for other in words {
            if other != word
                && !plumb_core::is_function_word(other)
                && found(vec![
                    term(self.fields.words, word),
                    term(self.fields.words, other),
                ])?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Named entity candidates before site blending or learned placement.
    /// Wikipedia's `about` field is searched separately: reference pages
    /// with the same title cannot consume its bounded candidate window.
    /// Uses existing index fields, so an installed index needs no rebuild.
    pub fn entities(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let described = self.described_words(query);
        let words = analysis::tokens(&self.words, query);
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(Vec::new());
        };
        let query_words: HashSet<&str> = words.iter().map(String::as_str).collect();
        let mut hits = Vec::new();
        if !described.is_empty() {
            let clauses = described
                .iter()
                .map(|word| {
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.about, &word.stem),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            let searcher = self.reader.searcher();
            let found = searcher.search(
                self.language_query(&BooleanQuery::new(clauses)).as_ref(),
                &TopDocs::with_limit(CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?;
            for (_, address) in found {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let mut page: Page = serde_json::from_str(stored)?;
                if !page.is_article() || !page.item.as_deref().is_some_and(is_item_id) {
                    continue;
                }
                let (name, named) = self.name_match(&page, query, &joined, &query_words);
                let covered = self.described_match(&page, &described);
                if name <= 0.0 && covered <= 0.0 {
                    continue;
                }
                page.drop_borrowed_site();
                let popularity = document
                    .get_first(self.fields.popularity)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as f32
                    / POPULARITY_SCALE;
                hits.push(PageHit {
                    page,
                    named,
                    whole: covered > 0.0,
                    score: name.max(covered)
                        * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                    popularity,
                    learned: None,
                });
            }
        }
        // Keep exact stop-word names ("The Who"), films and Wikidata profiles.
        for hit in self.search_once(query, CANDIDATES, None)? {
            if (matches!(hit.page.set.as_str(), WIKIDATA_SET | FILMS_SET)
                || hit.page.is_article() && hit.named)
                && hit.page.item.as_deref().is_some_and(is_item_id)
                && (hit.named || hit.whole)
                && !hits.iter().any(|kept| kept.page.url == hit.page.url)
            {
                hits.push(hit);
            }
        }
        hits.sort_by(|a, b| {
            b.named
                .cmp(&a.named)
                .then_with(|| b.score.total_cmp(&a.score))
        });
        hits.truncate(limit.min(CANDIDATES));
        Ok(hits)
    }

    /// The best `limit` pages for `query`, best first, after the records
    /// it names by their identifier ([`records_named`]).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        let paper = PaperQuery::parse(query)?;
        let mut hits = if paper.constrained {
            self.search_papers(&paper, limit)?
        } else {
            self.search_naming_docs(query, &Operators::default(), false, limit)?
        };
        let mut records = records_named(query);
        hits.retain(|hit| !records.iter().any(|record| record.page.url == hit.page.url));
        records.append(&mut hits);
        Ok(records)
    }

    pub(crate) fn search_general(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<Vec<PageHit>> {
        let mut hits = self.search_once(query, limit, scope)?;
        // "drake albums", "tom hanks movies": Wikipedia's list of the
        // works of an artist, actor or writer, which the query names only
        // in part.
        if scope.is_none() {
            if let Some(list) = self.first_titled(&work_lists(query))? {
                match hits.iter_mut().find(|hit| hit.page == list.page) {
                    Some(kept) => {
                        kept.named = true;
                        kept.score = kept.score.max(list.score);
                    }
                    None => hits.push(list),
                }
                hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            }
        }
        let added = match compared(query) {
            Some(sides) => self.add_compared(&mut hits, &sides, limit, scope)?,
            None => self.add_hinted(&mut hits, query, limit, scope)?,
        };
        if added {
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            truncate_keeping_inner_pages(&mut hits, limit);
        }
        Ok(hits)
    }

    /// Adds to `hits` the pages named by what `query` asks about: "galileo"
    /// of "who is galileo". `false` when it asks about nothing.
    fn add_hinted(
        &self,
        hits: &mut Vec<PageHit>,
        query: &str,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<bool> {
        // "lululemon wikipedia", "batch normalization paper", "who is
        // galileo": the words that say what is wanted are not part of the
        // name, so the name is searched for too. So is what a fact question
        // asks about, or "when was isaac newton born" lists Newton's mother
        // and namesakes, whose descriptions hold its words, and not him;
        // but not when the query is itself a name ("age of empires"), and
        // so is what "benefits of meditation" asks about. So is the list a
        // search for one asks for: "popes in order" finds Pope John
        // numbering and not List of popes, which "list of popes" names. A
        // monarch or pope typed with a number is searched in Roman numerals
        // when no article is named as typed: "charles 3", "henry the 8th".
        // A hinted name is not searched when a page is called the whole
        // query: "plaster cast", "ice age" and "horror film" are not about
        // plaster, ice or horror.
        let called = hits.iter().any(|hit| hit.named && !hit.whole);
        let hinted = hinted_name(query)
            .filter(|_| !called)
            .or_else(|| {
                list_asked(query)
                    .or_else(|| {
                        plumb_core::facts::fact_asked(query)
                            .map(|asked| asked.subject)
                            .or_else(|| aspect_asked(query))
                            .filter(|_| !hits.iter().any(|hit| hit.named))
                    })
                    .map(|name| (name, Hint::Named))
            })
            .or_else(|| {
                regnal_name(query)
                    .filter(|_| !hits.iter().any(|hit| hit.named && hit.page.is_article()))
                    .map(|name| (name, Hint::Regnal))
            });
        // An event's article has its year first or last ("2025 Tour de
        // France", "Eurovision Song Contest 2025"), so a name with a year
        // at one end is searched with it at the other too.
        let (name, hint) = hinted
            .clone()
            .unwrap_or_else(|| (query.to_lowercase(), Hint::Any));
        let moved = year_moved(&name).map(|name| (name, hint));
        let judged = hinted.is_some().then(|| judged_name(&name)).flatten();
        let searches: Vec<(String, Hint)> = hinted.into_iter().chain(moved).collect();
        if searches.is_empty() {
            return Ok(false);
        }
        for (name, hint) in searches {
            self.add_found(hits, &name, hint, limit, scope)?;
        }
        // A work keeps its "the" for "The Hobbit", but "who painted the
        // mona lisa" asks about Mona Lisa, not the song The Mona Lisa: what
        // the name names without it counts too when it holds the fact asked.
        let asked = plumb_core::facts::fact_asked(query).filter(|_| hint == Hint::Named);
        if let (Some(bare), Some(asked)) = (name.strip_prefix("the "), asked) {
            let holds = |hit: &PageHit| {
                let facts = &hit.page.facts;
                facts.iter().any(|fact| asked.kinds.contains(&fact.kind))
            };
            let mut found = self.search_once(bare, limit, scope)?;
            found.retain(|hit| hit.named && holds(hit));
            keep_found(hits, found);
        }
        // "what is a normal blood pressure" asks what is normal for blood
        // pressure: when no page is called that, the pages "blood
        // pressure" names are found too. "Normal distribution" is.
        if let Some(judged) = judged.filter(|_| !hits.iter().any(|hit| hit.named)) {
            self.add_found(hits, judged, Hint::Named, limit, scope)?;
        }
        Ok(true)
    }

    /// Adds to `hits` the pages found for `name`, the kind `hint` asks for.
    fn add_found(
        &self,
        hits: &mut Vec<PageHit>,
        name: &str,
        hint: Hint,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<()> {
        let mut found = self.search_once(name, limit, scope)?;
        match hint {
            // Only a person the name names, and not when a page is called
            // the whole query: "black death" is not about the singer Black,
            // nor "christmas songs" about Christmas.
            Hint::Person => {
                let called = hits.iter().any(|hit| hit.named);
                found.retain(|hit| !called && hit.named && hit.page.is_person());
            }
            // Only a person: "henry v" also names Shakespeare's play.
            Hint::Regnal => found.retain(|hit| hit.named && hit.page.is_person()),
            // Only an event, competition, team or venue, and not when a
            // page is called the whole query: "truth table" is not about
            // a truth, nor "tax bracket" about a tax.
            Hint::Event => {
                let called = hits.iter().any(|hit| hit.named);
                found.retain(|hit| {
                    !called && hit.named && describes_an_event(hit.page.description.as_deref())
                });
            }
            Hint::Named => found.retain(|hit| hit.named),
            Hint::Article => found.retain(|hit| hit.page.set.starts_with("wikipedia-")),
            // Only a repository called so: "reverse repo" is finance.
            Hint::Repo => found.retain(|hit| hit.named && hit.page.set == GITHUB_SET),
            Hint::Paper => {
                if found.iter().any(|hit| hit.page.set == PAPERS_SET) {
                    found.retain(|hit| hit.page.set == PAPERS_SET);
                }
                // A paper whose title starts with the name: "Batch
                // Normalization: Accelerating Deep Network Training…".
                for hit in &mut found {
                    let title = hit.page.title.to_lowercase();
                    let head = title.split(':').next().unwrap_or("").trim();
                    if hit.page.set == PAPERS_SET && head == name {
                        hit.named = true;
                        hit.score = hit.score.max(
                            ALIAS_MATCH
                                * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity),
                        );
                    }
                }
            }
            Hint::Field(marks) => {
                let in_field = |hit: &PageHit| {
                    let page = &hit.page;
                    hit.named
                        && plumb_core::article::in_field(
                            marks,
                            &page.title,
                            page.description.as_deref(),
                            page.lead.as_deref(),
                        )
                };
                if found.iter().any(in_field) {
                    found.retain(|hit| !hit.named || in_field(hit));
                }
            }
            // Only the thing's article: "python examples" asks for code,
            // not the repositories called Python.
            Hint::Aspect => found.retain(|hit| hit.named && hit.page.is_article()),
            // Only a work of the kinds asked for, and not when a page is
            // called the whole query: "power chords" is not about the song
            // "Power".
            Hint::Work(kinds) => {
                let called = hits.iter().any(|hit| hit.named);
                found.retain(|hit| !called && hit.named && hit.page.is_work_of(kinds));
            }
            // Only a film, show or game the name names, when it names
            // one: "titanic movie" is not about the ship.
            Hint::Screen => {
                let screen = |hit: &PageHit| hit.named && hit.page.is_work_of(SCREEN);
                if found.iter().any(screen) {
                    found.retain(|hit| !hit.named || screen(hit));
                }
            }
            // A plural that names no article but namesakes with a
            // qualifier asks about what its singular names: "what are
            // clouds" is about Cloud, not "Clouds (2020 film)", and "what
            // are solar panels" about Solar panel.
            Hint::Any => {
                let qualified =
                    |hit: &PageHit| super::base_title(&hit.page.title) != hit.page.title;
                let last = name.rsplit(' ').next().unwrap_or_default();
                let plural =
                    plumb_core::other_number(last).is_some_and(|one| one.len() < last.len());
                if plural && found.iter().all(|hit| !hit.named || qualified(hit)) {
                    if let Some(singular) = last_word_in_other_number(name) {
                        let named = self.search_once(&singular, limit, scope)?;
                        found.extend(
                            named.into_iter().filter(|hit| {
                                hit.named && hit.page.is_article() && !qualified(hit)
                            }),
                        );
                    }
                }
            }
        }
        keep_found(hits, found);
        Ok(())
    }

    /// Adds to `hits` the pages named by each of the things a comparison
    /// sets side by side ([`compared`]): the articles on mitosis and
    /// meiosis for "mitosis vs meiosis". `false` when one of `hits`
    /// already compares them, as "Crocodiles vs. Alligators" does
    /// "crocodile vs alligator": a page whose title has nothing else of
    /// substance. "Power consumption: SSD vs HDD" asks something narrower.
    fn add_compared(
        &self,
        hits: &mut Vec<PageHit>,
        sides: &[String; 2],
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<bool> {
        let compares = |hit: &PageHit| {
            let mut title = hit.page.title.to_lowercase();
            for side in sides {
                let Some(at) = title.find(side.as_str()) else {
                    return false;
                };
                title.replace_range(at..at + side.len(), " ");
            }
            // What is left of "crocodiles vs. alligators" is "s vs. s".
            analysis::tokens(&self.words, &title).iter().all(|word| {
                word.len() == 1
                    || crate::is_function_word(word)
                    || ASKING_WORDS.contains(&word.as_str())
                    || COMPARING_WORDS.contains(&word.as_str())
            })
        };
        if hits.iter().any(compares) {
            return Ok(false);
        }
        for side in sides {
            self.add_found(hits, side, Hint::Named, limit, scope)?;
        }
        Ok(true)
    }

    pub(crate) fn scope_term(&self, text: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.scope, text),
            IndexRecordOption::Basic,
        ))
    }

    pub(crate) fn page_scope(&self, ops: &Operators, docs: bool) -> Option<Box<dyn Query>> {
        let mut clauses = Vec::new();
        if docs {
            clauses.push((Occur::Must, self.scope_term("set:docs")));
        }
        if !ops.sites.is_empty() {
            let mut hosts = Vec::new();
            for site in &ops.sites {
                hosts.push((Occur::Should, self.scope_term(&format!("under:{site}"))));
                // Operators also allow a site's registrable domain for a
                // subdomain that is not a separately counted site.
                if let Some(domain) = registrable_domain(site) {
                    hosts.push((Occur::Should, self.scope_term(&format!("host:{domain}"))));
                }
            }
            clauses.push((
                Occur::Must,
                Box::new(BooleanQuery::new(hosts)) as Box<dyn Query>,
            ));
        }
        for site in &ops.not_sites {
            clauses.push((Occur::MustNot, self.scope_term(&format!("under:{site}"))));
        }
        // A BooleanQuery with only exclusions matches nothing.
        if !clauses.is_empty() && clauses.iter().all(|(occur, _)| *occur == Occur::MustNot) {
            clauses.push((Occur::Must, Box::new(tantivy::query::AllQuery)));
        }
        (!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
    }

    /// The Wiktionary word `name` is ([`WIKTIONARY_SET`]), written as it
    /// is or with other capitals ("anadromous", "AWOL"): the best-known
    /// such word, or the one written alike when `name` has capitals. In
    /// lower case it says nothing of them: "aka" asks for AKA, not the
    /// little-known vine aka.
    pub fn definition(&self, name: &str) -> Result<Option<Page>> {
        let Some(joined) = analysis::tokens(&self.joined, name).pop() else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let named = TermQuery::new(
            Term::from_field_text(self.fields.word, &joined),
            IndexRecordOption::Basic,
        );
        let top = TopDocs::with_limit(TITLE_CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let wanted = plumb_core::collapse_whitespace(name);
        let mut best: Option<Page> = None;
        for (_, address) in searcher.search(&named, &top)? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if page.set != WIKTIONARY_SET || !page.title.eq_ignore_ascii_case(&wanted) {
                continue;
            }
            if page.title == wanted && wanted.chars().any(char::is_uppercase) {
                return Ok(Some(page));
            }
            best.get_or_insert(page);
        }
        Ok(best)
    }

    /// Adds to `found`, the pages [`PageSearcher::search`] found for
    /// `query`, the article titled in the other number of its last word
    /// when none is named as typed and the best of `sites` only shares a
    /// word with the query ([`only_shares_a_word`]): "tariffs" names the
    /// article Tariff over tariffs.net. A query that sites answer well
    /// keeps its sites: "used cars" is not the article Used car.
    pub fn add_other_number(
        &self,
        query: &str,
        sites: &[crate::Hit],
        found: &mut Vec<PageHit>,
        limit: usize,
    ) -> Result<()> {
        if found.iter().any(|hit| hit.named && hit.page.is_article())
            || !sites.first().is_none_or(only_shares_a_word)
        {
            return Ok(());
        }
        let Some(other) = last_word_in_other_number(query) else {
            return Ok(());
        };
        let mut added = false;
        for hit in self.search(&other, limit)? {
            if !hit.named || !hit.page.is_article() {
                continue;
            }
            // Found as typed only by some of its words: named now.
            match found.iter_mut().find(|h| h.page == hit.page) {
                Some(listed) if listed.named => {}
                Some(listed) => *listed = hit,
                None => found.push(hit),
            }
            added = true;
        }
        if added {
            found.sort_by(|a, b| b.score.total_cmp(&a.score));
            found.truncate(limit);
        }
        Ok(())
    }

    /// The Wikipedia article titled the first of `titles` there is one
    /// of, named by it.
    fn first_titled(&self, titles: &[String]) -> Result<Option<PageHit>> {
        let searcher = self.reader.searcher();
        for title in titles {
            let Some(joined) = analysis::tokens(&self.joined, title).pop() else {
                continue;
            };
            let keyed = TermQuery::new(
                Term::from_field_text(self.fields.keys, &joined),
                IndexRecordOption::Basic,
            );
            let top = TopDocs::with_limit(TITLE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
            for (popularity, address) in
                searcher.search(self.language_query(&keyed).as_ref(), &top)?
            {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let page: Page = serde_json::from_str(stored)?;
                let titled = analysis::tokens(&self.joined, &page.title).pop();
                if !page.is_article() || titled.as_deref() != Some(joined.as_str()) {
                    continue;
                }
                let popularity = popularity.unwrap_or(0) as f32 / POPULARITY_SCALE;
                return Ok(Some(PageHit {
                    page,
                    named: true,
                    whole: false,
                    score: 1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity,
                    popularity,
                    learned: None,
                }));
            }
        }
        Ok(None)
    }

    /// The song or album of the music set whose title is the whole of
    /// `query` when one is far better known than every other of that
    /// title: Radiohead's "Creep" for "creep", with
    /// [`KNOWN_SONG_MARGIN`] times the listeners of TLC's. `None` for a
    /// title many share about equally ("hello", "yesterday"), or one of
    /// fewer than [`KNOWN_SONG_LISTENERS`] listeners. Such a search does
    /// not list the song (its title is too common a word), but it may be
    /// for it.
    pub fn known_song(&self, query: &str) -> Result<Option<Page>> {
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let named = TermQuery::new(
            Term::from_field_text(self.fields.keys, &joined),
            IndexRecordOption::Basic,
        );
        let top = TopDocs::with_limit(CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let mut songs = Vec::new();
        for (_, address) in searcher.search(&named, &top)? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if page.set == MUSIC_SET {
                songs.push(page);
            }
        }
        songs.sort_by_key(|page| std::cmp::Reverse(page.views));
        let mut songs = songs.into_iter();
        let Some(best) = songs.next() else {
            return Ok(None);
        };
        let runner_up = songs.next().map_or(0, |page| page.views);
        Ok((best.views >= KNOWN_SONG_LISTENERS
            && best.views >= runner_up.saturating_mul(KNOWN_SONG_MARGIN))
        .then_some(best))
    }
}

/// `query` with its last word in the other number
/// ([`plumb_core::other_number`]): "tariffs" -> "tariff",
/// "no kings protest" -> "no kings protests". `None` when the word has no
/// other number ("news") or the query is an address.
pub(crate) fn last_word_in_other_number(query: &str) -> Option<String> {
    if query.contains('.') || query.contains(':') {
        return None;
    }
    let text = plumb_core::normalize_text(query);
    let (head, last) = match text.rsplit_once(' ') {
        Some((head, last)) => (Some(head), last),
        None => (None, text.as_str()),
    };
    let other = plumb_core::other_number(last)?;
    Some(match head {
        Some(head) => format!("{head} {other}"),
        None => other,
    })
}

/// Ordinals a monarch's number is written as after "the": "henry the
/// eighth".
const ORDINALS: &str = "first second third fourth fifth sixth seventh eighth ninth tenth \
    eleventh twelfth thirteenth fourteenth fifteenth sixteenth seventeenth eighteenth nineteenth \
    twentieth";

/// `query` with the number it ends with, in digits or an ordinal, in
/// Roman numerals, as Wikipedia titles monarchs and popes: "henry viii"
/// for "henry 8", "henry 8th", "henry the 8th" and "henry the eighth".
/// `None` when it ends with no number from 1 to 39 after a name of
/// letters.
pub(crate) fn regnal_name(query: &str) -> Option<String> {
    let lower = query.to_lowercase();
    let mut words: Vec<&str> = lower.split_whitespace().collect();
    let last = words.pop()?;
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| last.strip_suffix(suffix))
        .unwrap_or(last);
    let mut number = match digits.parse::<usize>() {
        Ok(number) => number,
        Err(_) if words.last() == Some(&"the") => {
            ORDINALS
                .split_whitespace()
                .position(|ordinal| ordinal == last)?
                + 1
        }
        Err(_) => return None,
    };
    if words.last() == Some(&"the") {
        words.pop();
    }
    if !(1..40).contains(&number)
        || words.is_empty()
        || !words
            .iter()
            .all(|word| word.chars().all(char::is_alphabetic))
    {
        return None;
    }
    let mut roman = String::new();
    for (value, numeral) in [(10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")] {
        while number >= value {
            roman.push_str(numeral);
            number -= value;
        }
    }
    Some(format!("{} {roman}", words.join(" ")))
}

/// The text match ([`crate::Hit::text_score`]) below which a site the
/// query does not name only shares a word with it (government.ru for
/// "government shutdown", at 0.15). Sites a description finds by meaning
/// match more ("used cars").
const SHARES_A_WORD_MATCH: f32 = 0.25;

/// The link score below which a site the query names is next to unknown
/// (tariffs.net, at 0.015).
const BARELY_LINKED: f32 = 0.1;

/// Whether `site`, the best site found, only shares a word with the
/// query: a site it does not name that matches it weakly
/// ([`SHARES_A_WORD_MATCH`]), or one it names that is not official and
/// next to nobody links to ([`BARELY_LINKED`]).
fn only_shares_a_word(site: &crate::Hit) -> bool {
    if site.named {
        !site.official && site.link_score < BARELY_LINKED
    } else {
        site.placing_text_score.unwrap_or(site.text_score) < SHARES_A_WORD_MATCH
    }
}
