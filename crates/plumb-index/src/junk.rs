//! Sites left out of the results unless the query names them: they are no
//! use to anyone searching for something else, and some are unsafe to
//! list.
//!
//! - Sites on names reserved for tests (uniteddolls.test).
//! - Online pill shops named after a drug ("ibuprofen dosage" found
//!   celebrex365o24.com, [`health::source_quality`]).
//! - Little-known sites whose homepage is only a sign-in form. Meaning
//!   puts every sign-in page close to every "<something> login" search,
//!   so "Fidelity NetBenefits login" listed "DataHubNet - Login" on
//!   datahubnet.online and "hulu login" "Login - iShara.TV" on yablo.tv:
//!   a stranger's password box for a searcher about to type a password. One for someone else's
//!   product ("Outlook Web App" on bpl.net for "microsoft outlook",
//!   [`plumb_core::is_sign_in_portal`]) goes too. live.com, titled
//!   "Outlook", is Microsoft's.

use plumb_core::normalize_text;

use crate::names::is_reserved_name;
use crate::{health, Hit, WELL_KNOWN_LINK_SCORE};

/// Words that say a title is a sign-in form's, at either end of one of
/// its parts: "Login - QRIS", "Wealthscape Login", "Sign in to RealPay".
/// The longer ones that start with a shorter one come first.
const SIGN_IN: &[&str] = &[
    "please log in",
    "please sign in",
    "login",
    "log in",
    "logon",
    "log on",
    "signin",
    "sign in",
    "sign on",
];

/// Words after [`SIGN_IN`] at the start of a part that keep it a sign-in
/// form's title: "Login Page", "Sign in to your account", "Sign-In with
/// Ethereum", "Login or register".
const AFTER_SIGN_IN: &[&str] = &["page", "portal", "form", "to", "with", "or", "required"];

/// What separates the parts of a title: "Login | SCARLETT".
const TITLE_SEPARATORS: &[&str] = &[" - ", "|", "–", "—", ":", "·", "•", "»"];

/// Path parts and host labels of a sign-in form's address: a homepage
/// that sends its visitors to /login has nothing else to show.
const SIGN_IN_PATHS: &[&str] = &[
    "login",
    "log-in",
    "logon",
    "signin",
    "sign-in",
    "sign_in",
    "auth",
    "authorize",
    "sso",
    "openid-connect",
    "login.aspx",
    "login.php",
    "login.jsp",
    "signin.aspx",
    "wp-login.php",
];

/// Whether `hit` stays out of the results of `query`.
pub(crate) fn left_out(query: &str, hit: &Hit) -> bool {
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
    title.is_some_and(|title| plumb_core::is_sign_in_portal(&hit.domain, title))
        || (!hit.official && is_sign_in_page(&hit.url, title) && !names_label(query, &hit.domain))
}

/// Whether a site whose homepage is at `url`, titled `title`, is only a
/// sign-in form: one of the title's parts says so at one end ("Login -
/// Arvixe", "DataHubNet - Login", "Sign in to your account - Jenta Tech"),
/// or the homepage is a sign-in address (https://qrisajaib.com/login,
/// https://login.example.com/).
pub(crate) fn is_sign_in_page(url: &str, title: Option<&str>) -> bool {
    title.is_some_and(says_sign_in) || is_sign_in_address(url)
}

/// Whether a part of `title` says it is a sign-in form.
fn says_sign_in(title: &str) -> bool {
    let mut parts = vec![title.to_string()];
    for separator in TITLE_SEPARATORS {
        parts = parts
            .iter()
            .flat_map(|part| part.split(separator))
            .map(str::to_string)
            .collect();
    }
    parts.iter().any(|part| {
        let part = normalize_text(part);
        SIGN_IN.iter().any(|phrase| {
            part == *phrase
                || part.strip_suffix(phrase).is_some_and(|before| {
                    // "Free AI Chat, No Login" asks for none.
                    before.ends_with(' ')
                        && !matches!(before.split(' ').nth_back(1), Some("no" | "without"))
                })
                || part
                    .strip_prefix(phrase)
                    .and_then(|after| after.strip_prefix(' '))
                    .and_then(|after| after.split(' ').next())
                    .is_some_and(|next| AFTER_SIGN_IN.contains(&next))
        })
    })
}

/// Whether `url` is a sign-in form's address: its host starts with
/// "login." or the like, or a part of its path is "login" or the like.
fn is_sign_in_address(url: &str) -> bool {
    let Some(rest) = url.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let first_label = host.split('.').next().unwrap_or("");
    ["login", "signin", "auth", "sso"].contains(&first_label.to_ascii_lowercase().as_str())
        || path
            .split('/')
            .any(|part| SIGN_IN_PATHS.contains(&part.to_ascii_lowercase().as_str()))
}

/// Whether some of `query`'s words in a row, joined, are a label of
/// `domain` or the start of one: "wealthscape login" names wealthscape.com,
/// "us bank login" usbank.com, "hdfc login" hdfcbank.com and "uc davis
/// canvas" canvas.ucdavis.edu. Runs shorter
/// than 3 letters name nothing ("v8 login" does not name v8engines.com),
/// and nor do the [`SIGN_IN`] words.
pub(crate) fn names_label(query: &str, domain: &str) -> bool {
    // Not the ending: "net" names no .net site.
    let suffix = plumb_core::registrable_domain(domain)
        .and_then(|site| site.split_once('.').map(|(_, suffix)| format!(".{suffix}")))
        .unwrap_or_default();
    let labels: Vec<String> = domain
        .strip_suffix(&suffix)
        .unwrap_or(domain)
        .split('.')
        .map(|label| {
            label
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .collect();
    let mut query = format!(" {} ", normalize_text(query));
    for phrase in SIGN_IN {
        query = query.replace(&format!(" {phrase} "), " ");
    }
    let words: Vec<&str> = query.split(' ').filter(|word| !word.is_empty()).collect();
    (0..words.len()).any(|start| {
        let mut run = String::new();
        words[start..].iter().any(|word| {
            run.push_str(word);
            run.len() >= 3 && labels.iter().any(|label| label.starts_with(&run))
        })
    })
}
