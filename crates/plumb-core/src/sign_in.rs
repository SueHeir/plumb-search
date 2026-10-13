//! Telling a sign-in form's page apart, and whether a query names a site
//! by its label: a little-known site whose homepage is only a sign-in
//! form is no use to anyone searching for another site, and a stranger's
//! password box is unsafe to list for a searcher about to type a password.

use crate::{normalize_text, registrable_domain};

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

/// Whether a site whose homepage is at `url`, titled `title`, is only a
/// sign-in form: one of the title's parts says so at one end ("Login -
/// Arvixe", "DataHubNet - Login", "Sign in to your account - Jenta Tech"),
/// or the homepage is a sign-in address (https://qrisajaib.com/login,
/// https://login.example.com/).
pub fn is_sign_in_page(url: &str, title: Option<&str>) -> bool {
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

/// Whether `query` asks to sign in somewhere: "chase login", "Xfinity
/// account sign in".
pub fn asks_to_sign_in(query: &str) -> bool {
    let query = format!(" {} ", normalize_text(query));
    SIGN_IN
        .iter()
        .any(|phrase| query.contains(&format!(" {phrase} ")))
}

/// Whether some of `query`'s words in a row, joined, are a label of
/// `domain` or the start of one: "wealthscape login" names wealthscape.com,
/// "us bank login" usbank.com, "hdfc login" hdfcbank.com and "uc davis
/// canvas" canvas.ucdavis.edu. Runs shorter
/// than 3 letters name nothing ("v8 login" does not name v8engines.com),
/// and nor do the [`SIGN_IN`] words.
pub fn names_label(query: &str, domain: &str) -> bool {
    // Not the ending: "net" names no .net site.
    let suffix = registrable_domain(domain)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_in_pages_are_told_by_title_or_address() {
        for title in [
            "Login - QRIS",
            "Login - Arvixe",
            "Login | SCARLETT",
            "Login Page",
            "Wealthscape Investor Login",
            "User Login | The Digital Jungle",
            "Sign in to your account - Jenta Tech",
            "Sign In — FAST X OTP",
            "Sign-In with Ethereum",
            "The Grid - Please log in to continue",
            "ePIS | Login",
        ] {
            assert!(
                is_sign_in_page("https://example.com/", Some(title)),
                "{title:?}"
            );
        }
        for title in [
            "Fidelity Investments",
            "Login.gov",
            "Blog on gardening",
            "How logins work",
            "Signing in Spanish",
            "Catalog online",
            "DeepSeek AI – Free AI Chat Online & DeepSeek-V4 No Login",
        ] {
            assert!(
                !is_sign_in_page("https://example.com/", Some(title)),
                "{title:?}"
            );
        }
        for url in [
            "https://qrisajaib.com/login",
            "https://www.its52.com/login.aspx",
            "https://id.koder.dev/oauth/v2/authorize?client_id=x",
            "https://login.example.com/",
            "https://example.com/users/sign_in",
        ] {
            assert!(is_sign_in_page(url, None), "{url}");
        }
        for url in [
            "https://example.com/",
            "https://example.com/blog/login-tips",
            "https://authors.example.com/",
        ] {
            assert!(!is_sign_in_page(url, None), "{url}");
        }
    }

    #[test]
    fn a_query_names_a_label_by_its_words_in_a_row() {
        assert!(names_label("wealthscape login", "wealthscape.com"));
        assert!(names_label("us bank login", "usbank.com"));
        assert!(names_label("hdfc login", "hdfcbank.com"));
        assert!(names_label("my chart sign in", "mychart.com"));
        assert!(!names_label("v8 login", "v8engines.com"));
        assert!(!names_label("fidelity netbenefits login", "login.xyz"));
        assert!(!names_label("xfinity sign in", "signin.example"));
        assert!(!names_label("hulu login", "arvixe.com"));
        assert!(names_label("uc davis canvas login", "canvas.ucdavis.edu"));
        assert!(!names_label("att net account login", "datahub.net"));
        assert!(!names_label("bank online login", "datahubnet.online"));
    }

    #[test]
    fn a_query_asks_to_sign_in_by_its_words() {
        for query in ["chase login", "Xfinity account sign in", "AOL Log-in"] {
            assert!(asks_to_sign_in(query), "{query}");
        }
        for query in ["microsoft account", "loginradius", "signing in spanish"] {
            assert!(!asks_to_sign_in(query), "{query}");
        }
    }
}
