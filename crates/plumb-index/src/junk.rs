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
//!   name, so they go even when that name is the query ("ebay"). Those
//!   with no description to tell them by go by their names (312bet29.com).
//!   One the query names stays: "lazybar" finds lazybaraustralia.com.
//! - Domains only held, whose homepage says so ([`is_holding_page`]):
//!   kept by a brand-protection registrar ("Registered & Protected by
//!   MarkMonitor" on goldenglobes.org, the Golden Globes' old address,
//!   and paypallogin.com), reserved, parked or for sale. However well
//!   linked or official they were, there is nothing there; only typing
//!   the address finds one.
//! - Little-known sites handing out Android apps' installers (APKs),
//!   most of them changed to unlock what the app charges for
//!   ("Kinemaster MOD APK Download", "GB WhatsApp APK Download"), for a
//!   search that does not ask for one ([`offers_apk`]). Such copies are
//!   how malware reaches phones, and an app's own site or store is well
//!   known or official, or is named by the search ("snaptube").
//! - Little-known sites offering a help line's number in their title
//!   ([`offers_help_line`]): "Epson Printer Support Number
//!   +1-844-932-1846" on epsonprintersupportpro.us came first for "epson
//!   printer support" and "canon printer support number". A company's own
//!   line is on its own site; tech-support scams wait for callers on sites
//!   like these. Only a search spelling the whole name finds one.
//! - Little-known homepages with no description that are only a name
//!   server's or host's abuse contact ([`is_abuse_contact`]): "Abuse" on
//!   nspmanaged.com, "Report Abuse" on dnsperfect.net. Their one word
//!   matched every "<something> abuse" search, and "memantine abuse",
//!   "elder abuse" and "steroid abuse" listed nspmanaged.com.

use plumb_core::{
    asks_to_sign_in, is_sign_in_page, names_label, normalize_text, registrable_domain, spells_label,
};

use crate::names::is_reserved_name;
use crate::schema::title_parts;
use crate::{health, Hit, WELL_KNOWN_LINK_SCORE};

/// Title parts, after [`normalize_text`], of a homepage that only says
/// its domain is held: kept by a brand-protection registrar, reserved,
/// parked or for sale.
const HOLDING_TITLES: &[&str] = &[
    "registered protected by markmonitor",
    "domain is registered",
    "this domain has been registered",
    "this domain is registered",
    "the domain name is registered",
    "this domain name has been registered",
    "this domain is reserved",
    "reserved domain",
    "reserved from registration",
    "domain not available",
    "domain parked",
    "parked domain",
    "domain for sale",
    "this domain is for sale",
];

/// Beginnings of such title parts: "Domain Registered at Safenames",
/// "Domain registered with JOKER.COM", "Domain Reserved".
const HOLDING_STARTS: &[&str] = &["domain registered", "domain reserved"];

/// Beginnings, after [`normalize_text`], of a held domain's description:
/// "This domain name has been registered with Gandi.net. It is currently
/// parked by the owner." A WHOIS site saying when a domain "has been
/// registered" says it further on.
const HOLDING_OPENINGS: &[&str] = &[
    "this domain name has been registered",
    "this domain has been registered",
];

/// Title parts, after [`normalize_text`], of a homepage that is only a
/// form or an address to report abuse of a name server's or host's
/// domains.
const ABUSE_CONTACT_TITLES: &[&str] = &[
    "abuse",
    "report abuse",
    "abuse contact",
    "abuse kontakt",
    "abuse report",
];

/// What a help line's number is called, after [`normalize_text`] and
/// before "number" or "phone number": "Epson Printer Support Number",
/// "QuickBooks Customer Service Phone Number".
const HELP_LINES: &[&str] = &[
    "support",
    "customer service",
    "customer care",
    "helpline",
    "help line",
    "help desk",
    "helpdesk",
    "toll free",
];

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

/// Words of a search that asks for an app's installer or a changed copy
/// of it: "minecraft apk", "kinemaster mod".
const APK_QUERY_WORDS: &[&str] = &[
    "apk", "apks", "mod", "mods", "modded", "hack", "hacked", "crack", "cracked",
];

/// Whether `hit` stays out of the results of `query`, which `named` says
/// names it in full.
pub(crate) fn left_out(query: &str, hit: &Hit, named: bool) -> bool {
    if is_holding_page(
        &hit.domain,
        hit.title.as_deref(),
        hit.description.as_deref(),
    ) && registrable_domain(query.trim()).as_deref() != Some(hit.domain.as_str())
    {
        return true;
    }
    let little_known = hit.link_score < WELL_KNOWN_LINK_SCORE && !hit.official;
    let texts = [hit.title.as_deref(), hit.description.as_deref()];
    let gambling = || {
        (is_gambling(texts) || (hit.description.is_none() && is_gambling_address(&hit.domain)))
            && !asks_about_gambling(query)
            && !named_by(query, hit)
    };
    // Not `named_by`: "kinemaster" names kinemastermodapk.com, titled
    // "Kinemaster MOD APK".
    let apk = || {
        offers_apk(texts)
            && !normalize_text(query)
                .split(' ')
                .any(|word| APK_QUERY_WORDS.contains(&word))
            && !spells_label(query, &hit.domain)
    };
    if little_known && (gambling() || apk()) {
        return true;
    }
    if little_known
        && offers_help_line(hit.title.as_deref(), hit.description.as_deref())
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
    if !hit.official && is_abuse_contact(title, hit.description.as_deref()) {
        return true;
    }
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

/// Whether a homepage titled `title` and described as `description` is
/// only an abuse contact: no description and a title part of
/// [`ABUSE_CONTACT_TITLES`] ("Report Abuse - SecureDNS93.com", "Abuse
/// Kontakt – mail.leenux.de"). A site against abuse says what it does
/// ("Elder Abuse Help Line").
pub(crate) fn is_abuse_contact(title: Option<&str>, description: Option<&str>) -> bool {
    description.is_none()
        && title.is_some_and(|title| {
            title_parts(title)
                .into_iter()
                .any(|part| ABUSE_CONTACT_TITLES.contains(&normalize_text(part).as_str()))
        })
}

/// Whether `title` offers a help line's number to call: it calls it one
/// of [`HELP_LINES`] ("Epson Printer Support Number +1-844-932-1846"),
/// and it or `description` gives the number.
pub(crate) fn offers_help_line(title: Option<&str>, description: Option<&str>) -> bool {
    let calls_it = |title: &str| {
        let title = format!(" {} ", normalize_text(title));
        HELP_LINES.iter().any(|line| {
            title.contains(&format!(" {line} number "))
                || title.contains(&format!(" {line} phone number "))
        })
    };
    title.is_some_and(calls_it)
        && [title, description]
            .into_iter()
            .flatten()
            .any(has_phone_number)
}

/// Whether `text` holds a phone number: ten digits or more with only
/// spaces, dashes, dots, brackets or a plus between them
/// ("+1-844-932-1846", "(954) 726-9525").
fn has_phone_number(text: &str) -> bool {
    let mut digits = 0;
    text.chars().any(|ch| {
        if ch.is_ascii_digit() {
            digits += 1;
        } else if !matches!(ch, ' ' | '-' | '.' | '(' | ')' | '+') {
            digits = 0;
        }
        digits >= 10
    })
}

/// Whether `query` names `hit`'s site: spells a whole label of it
/// ("mataram toto" and mataramtoto.com), or the start of one that its title
/// says as words ("lazybar" and lazybaraustralia.com, "Lazybar Casino").
/// "netflix login" does not name netflixtotoutama.com, titled "NETFLIXTOTO".
fn named_by(query: &str, hit: &Hit) -> bool {
    let query = format!(" {} ", normalize_text(query));
    let says = |title: &str| format!(" {} ", normalize_text(title)).contains(&query);
    spells_label(&query, &hit.domain)
        || (names_label(&query, &hit.domain) && hit.title.as_deref().is_some_and(says))
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

/// Whether `texts` offer an Android app's installer, an APK: "Kinemaster
/// MOD APK Download Latest Version", "Download the Snaptube APK for
/// Android". APKPure, a store, is one word.
pub(crate) fn offers_apk<'a>(texts: impl IntoIterator<Item = Option<&'a str>>) -> bool {
    texts.into_iter().flatten().any(|text| {
        normalize_text(text)
            .split(' ')
            .any(|word| word == "apk" || word == "apks")
    })
}

/// Endings only gambling sites take.
const GAMBLING_ENDINGS: &[&str] = &["bet", "casino", "poker", "bingo", "lotto"];

/// Words that, with a number right after them, make a gambling site's
/// name: "kitakaisartoto88", "pkrslots29", "qq88".
const GAMBLING_NUMBERED: &[&str] = &["toto", "slot", "slots", "qq", "gacor"];

/// Whether `domain` is named like a gambling site, which often has no
/// title or description to tell it by: on a gambling ending (161bet.bet),
/// with "togel" in its name (jalurwlatogel88.net), a gambling word
/// numbered ("kitakaisartoto88.net"), or "bet" next to a number with no
/// letter on either side ("312bet29.com", "bet365.com", not
/// tibet2025.org or 247bethesdaplumber.com).
pub(crate) fn is_gambling_address(domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    let ending = domain.rsplit('.').next().unwrap_or_default();
    let label = plumb_core::domain_label(&domain).replace(['-', '.'], "");
    let digit_at = |at: usize| label.as_bytes().get(at).is_some_and(u8::is_ascii_digit);
    let numbered = |word: &str| {
        label
            .match_indices(word)
            .any(|(at, _)| digit_at(at + word.len()))
    };
    let bet = label.match_indices("bet").any(|(at, _)| {
        let before = at.checked_sub(1).map(|b| label.as_bytes()[b]);
        let digit_before = before.is_some_and(|b| b.is_ascii_digit());
        let end = at + 3 == label.len();
        (before.is_none() || digit_before)
            && (end || digit_at(at + 3))
            && (digit_before || digit_at(at + 3))
    });
    GAMBLING_ENDINGS.contains(&ending)
        || label.contains("togel")
        || GAMBLING_NUMBERED.iter().any(|word| numbered(word))
        || bet
}

/// Whether the homepage of `domain`, titled `title` and described as
/// `description`, only says the domain is held: "Registered & Protected
/// by MarkMonitor", "Domain Registered at Safenames" (mozy.com), "This
/// domain is reserved", "Domain Parked", or "CorporationCreation.com is
/// for sale". A registrar's own homepage says more ("Domain Registration
/// in 3 minutes").
pub(crate) fn is_holding_page(
    domain: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> bool {
    let name = normalize_text(domain);
    let holding = |part: &str| {
        let part = normalize_text(part);
        HOLDING_TITLES.contains(&part.as_str())
            || HOLDING_STARTS
                .iter()
                .any(|start| part == *start || part.starts_with(&format!("{start} ")))
            || part.strip_suffix(" is for sale") == Some(name.as_str())
    };
    title.is_some_and(|title| title_parts(title).into_iter().any(holding))
        || description.is_some_and(|text| {
            let text = normalize_text(text);
            HOLDING_OPENINGS
                .iter()
                .any(|opening| text.starts_with(opening))
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
