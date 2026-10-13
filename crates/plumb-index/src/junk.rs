//! Sites left out of the results unless the query names them: they are no
//! use to anyone searching for something else, and some are unsafe to
//! list.
//!
//! - Sites on names reserved for tests (uniteddolls.test).
//! - Online pill shops named after a drug ("ibuprofen dosage" found
//!   celebrex365o24.com, [`health::source_quality`]).
//! - Little-known sites whose homepage is only a sign-in form
//!   ([`is_sign_in_page`]). Meaning puts every sign-in page close to every
//!   "<something> login" search, so "Fidelity NetBenefits login" listed
//!   "DataHubNet - Login" on datahubnet.online and "hulu login" "Login -
//!   iShara.TV" on yablo.tv: a stranger's password box for a searcher
//!   about to type a password. One for someone else's product ("Outlook
//!   Web App" on bpl.net for "microsoft outlook",
//!   [`plumb_core::is_sign_in_portal`]) goes too. live.com, titled
//!   "Outlook", is Microsoft's.
//! - In a search to sign in, little-known sites with no description that
//!   only meaning found. Once the sign-in pages went, "My CardGate" on
//!   curopayments.net took their place for "chase login", "coinbase login"
//!   and "amex login", and three untitled strangers for "schwab login".
//!   Nothing ties them to the site the searcher wants to sign in to.

//! - Little-known online gambling sites, for a search that is not about
//!   gambling. Most are spam on expired domains ("Situs Toto MATARAMTOTO"
//!   on alleghenyartgalleries.com) whose links call them by a big site's
//!   name, so they go even when that name is the query ("ebay").

use plumb_core::{
    asks_to_sign_in, is_sign_in_page, names_label, normalize_text, spells_label,
};

use crate::names::is_reserved_name;
use crate::{health, Hit, WELL_KNOWN_LINK_SCORE};

/// Words, after [`normalize_text`], that only online gambling sites use.
const GAMBLING_WORDS: &[&str] = &[
    "online casino",
    "casino online",
    "casino game",
    "casino games",
    "online slots",
    "slot online",
    "slots online",
    "free spins",
    "sportsbook",
    "sports betting",
    "betting",
    "bookmaker",
    "pokies",
    // Indonesian.
    "togel",
    "gacor",
    "maxwin",
    "slot gacor",
    "rtp slot",
    "situs slot",
    "situs toto",
    "situs judi",
    "judi online",
    "judi slot",
    "judi bola",
    "agen judi",
    "bandarqq",
    "dominoqq",
    "pkv games",
    "sbobet",
    "slot88",
    // Portuguese, Spanish and Turkish.
    "apostas",
    "cassino online",
    "apuestas",
    "bahis",
    "deneme bonusu",
];

/// The same in scripts whose vowel marks [`normalize_text`] splits words
/// at, found in the lowercase text as it is.
const GAMBLING_TEXT: &[&str] = &[
    // Thai: slots, baccarat, football betting, casino.
    "สล็อต",
    "บาคาร่า",
    "แทงบอล",
    "คาสิโน",
    // Persian: betting, casino.
    "شرط بندی",
    "کازینو",
    // Russian: casino, sports betting, bookmaker.
    "казино",
    "ставки на спорт",
    "букмекер",
    // Chinese: gambling, baccarat, casino, "entertainment city".
    "博彩",
    "百家乐",
    "赌场",
    "娱乐城",
    // The same in traditional characters, and as Japanese writes 娯.
    "百家樂",
    "賭場",
    "娛樂城",
    "娯樂城",
    // Vietnamese: bookmaker, betting.
    "nhà cái",
    "cá cược",
];

/// Query words that make a search one about gambling, where gambling
/// sites are what was asked for.
const GAMBLING_QUERY_WORDS: &[&str] = &[
    "bet",
    "bets",
    "betting",
    "casino",
    "casinos",
    "gambling",
    "odds",
    "picks",
    "poker",
    "slot",
    "slots",
    "lottery",
    "lotto",
    "toto",
    "jackpot",
    "bingo",
    "roulette",
    "blackjack",
    "baccarat",
    "wager",
];

/// Whether `hit` stays out of the results of `query`, which `named` says
/// names it in full.
pub(crate) fn left_out(query: &str, hit: &Hit, named: bool) -> bool {
    let little_known = hit.link_score < WELL_KNOWN_LINK_SCORE && !hit.official;
    if little_known
        && is_gambling([hit.title.as_deref(), hit.description.as_deref()])
        && !asks_about_gambling(query)
        && !spells_label(query, &hit.domain)
    {
        return true;
    }
    if named {
        return false;
    }
    if is_reserved_name(&hit.domain) {
        return true;
    }
    if health::source_quality(
        &hit.domain,
        hit.title.as_deref(),
        hit.description.as_deref(),
        Some(hit.link_score),
    )
    .is_some_and(|quality| quality.informational_penalty())
    {
        return true;
    }
    if hit.link_score >= WELL_KNOWN_LINK_SCORE {
        return false;
    }
    let title = hit.title.as_deref();
    let says_nothing_meaning_found = || {
        hit.description.is_none()
            && hit
                .query_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.lexical_score <= 0.0)
    };
    title.is_some_and(|title| plumb_core::is_sign_in_portal(&hit.domain, title))
        || (!hit.official
            && (is_sign_in_page(&hit.url, title)
                || (asks_to_sign_in(query) && says_nothing_meaning_found()))
            && !names_label(query, &hit.domain))
}

/// Whether `texts` say they are an online gambling site's: "Situs Toto
/// MATARAMTOTO : Link Toto Slot Depo 5k", "RTP Slot: Info RTP Live Slot
/// Pragmatic Play", "Lazybar Casino 🎰 Play The Best Pokies". A gambling
/// brand made of a word and "toto" or "qq" counts too ("RUPIAHTOTO",
/// "JurusQQ"); a casino hotel does not ("Golden Nugget Las Vegas Hotel &
/// Casino").
pub(crate) fn is_gambling<'a>(texts: impl IntoIterator<Item = Option<&'a str>>) -> bool {
    texts.into_iter().flatten().any(|text| {
        let words = format!(" {} ", normalize_text(text));
        let raw = text.to_lowercase().replace('\u{200c}', " ");
        GAMBLING_WORDS
            .iter()
            .any(|phrase| words.contains(&format!(" {phrase} ")))
            || GAMBLING_TEXT.iter().any(|phrase| raw.contains(phrase))
            || words.split(' ').any(|word| {
                word.is_ascii()
                    && ((word.len() >= 6 && word.ends_with("toto"))
                        || (word.len() >= 5 && word.ends_with("qq")))
            })
    })
}

/// Whether `query` asks about gambling: "nfl free vegas picks", "las
/// vegas casinos", "slot gacor".
fn asks_about_gambling(query: &str) -> bool {
    is_gambling([Some(query)])
        || normalize_text(query)
            .split(' ')
            .any(|word| GAMBLING_QUERY_WORDS.contains(&word))
}
