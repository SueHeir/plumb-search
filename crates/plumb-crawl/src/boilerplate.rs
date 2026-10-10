//! Telling a homepage's boilerplate from what it says, a block of text at a
//! time, after the rules T5's authors used to clean the web for the C4
//! corpus (Raffel et al. 2020, section 2.2), with conservative rules that
//! retain brief factual text and prose about policies and programming.
//!
//! A block is the text between two elements that start a new line on
//! screen ([`crate::extract`]'s word breaks), the closest a page comes to
//! C4's lines. Menus, footers and banners in their own elements are already
//! left out; these rules catch the ones built from plain `<div>`s, the
//! cookie notices, and the "Learn more", "Shop now" and "3 min read" bits
//! between the sentences. C4 also requires a line to end in punctuation;
//! headlines and product blurbs rarely do, so that rule is not used.

/// Most words in a generic action label eligible for [`Verdict::Short`].
const MIN_WORDS: usize = 5;

/// Links in a block, nearly all of whose text is link text, that make it a
/// menu or a list of links rather than something the page says.
const MENU_LINKS: usize = 3;

/// Lowercased phrases of a policy or cookie notice, a copyright line or a
/// warning about the browser. The first six are C4's.
const NOTICE_PHRASES: &[&str] = &[
    "terms of use",
    "privacy policy",
    "cookie policy",
    "uses cookies",
    "use of cookies",
    "use cookies",
    "accept cookies",
    "accept all cookies",
    "cookie settings",
    "cookie preferences",
    "terms of service",
    "terms and conditions",
    "all rights reserved",
    "skip to main content",
    "skip to content",
    "skip navigation",
    "your browser does not support",
    "video not supported",
    "upgrade your browser",
    "lorem ipsum",
];

/// What to do with a block of a page's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Keep,
    /// A short generic action label or read-time badge. Used only
    /// when the page has no longer block.
    Short,
    Drop,
}

/// Judges `block` (whitespace collapsed, not empty). `links` is how many
/// links begin in it, and `all_links` whether nearly all its text is link
/// text.
pub(crate) fn judge(block: &str, links: usize, all_links: bool) -> Verdict {
    // Braces and copyright signs also occur in useful prose. Only reject
    // obvious code and standalone copyright notices.
    let lower = block.to_lowercase();
    let phone = block.chars().filter(char::is_ascii_digit).count() >= 7
        && block
            .chars()
            .all(|c| c.is_ascii_digit() || "+-(). ".contains(c));
    let statistic = block.chars().any(|c| c.is_ascii_digit())
        && (block.ends_with('+') || block.starts_with(['$', '€', '£']))
        && block
            .chars()
            .all(|c| c.is_ascii_digit() || "+$€£., ".contains(c));
    if !block.chars().any(char::is_alphabetic) && !phone && !statistic {
        return Verdict::Drop;
    }
    if all_links && links >= MENU_LINKS {
        return Verdict::Drop;
    }
    let notice = NOTICE_PHRASES.iter().any(|p| lower == *p)
        || [
            "skip to main content",
            "skip to content",
            "skip navigation",
            "your browser does not support",
            "video not supported",
            "upgrade your browser",
            "lorem ipsum",
        ]
        .iter()
        .any(|p| lower.starts_with(p))
        || lower
            .strip_prefix("copyright ")
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        || lower.starts_with('©')
        || ([
            "by continuing",
            "we use cookies",
            "this site",
            "this website",
            "our website",
            "you agree",
            "we and our partners",
        ]
        .iter()
        .any(|p| lower.starts_with(p))
            && NOTICE_PHRASES.iter().any(|p| lower.contains(p)))
        || [
            "enable javascript",
            "javascript is disabled",
            "javascript disabled",
            "turn on javascript",
            "javascript must be enabled",
            "you need to enable javascript",
        ]
        .iter()
        .any(|p| lower.starts_with(p))
        || (lower.starts_with("please ")
            && lower.contains("browser")
            && lower.contains("javascript"));
    let code = ["function ", "const ", "let ", "var ", "import ", "export "]
        .iter()
        .any(|p| lower.starts_with(p))
        && (block.contains('{') || block.contains(';'));
    if notice || code {
        return Verdict::Drop;
    }
    // Brief business names, opening hours, contact details and product names
    // remain useful beside longer paragraphs. Only generic action labels and
    // read-time badges use the fallback for pages with no substantive text.
    if words(block) < MIN_WORDS
        && ([
            "learn more",
            "shop now",
            "read more",
            "find out more",
            "click here",
        ]
        .contains(&lower.as_str())
            || (lower.ends_with(" min read") && lower.starts_with(|c: char| c.is_ascii_digit())))
    {
        return Verdict::Short;
    }
    Verdict::Keep
}

/// Words in `text`, counting two characters of a script written without
/// spaces (Chinese, Japanese, Thai...) as one word.
fn words(text: &str) -> usize {
    let spaceless = text.chars().filter(|&c| is_spaceless(c)).count();
    text.split_whitespace().count() + spaceless / 2
}

fn is_spaceless(c: char) -> bool {
    matches!(c,
        '\u{0E00}'..='\u{0EFF}' // Thai, Lao
        | '\u{1000}'..='\u{109F}' // Myanmar
        | '\u{1780}'..='\u{17FF}' // Khmer
        | '\u{3040}'..='\u{30FF}' // Hiragana, Katakana
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK
        | '\u{F900}'..='\u{FAFF}' // CJK compatibility
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_are_kept() {
        let block = "The Apache Software Foundation provides software for the public good.";
        assert_eq!(judge(block, 0, false), Verdict::Keep);
        // A headline that is a link, with no full stop.
        let headline = "Suspected Second-generation Planet Solves NASA Hubble Cold Case";
        assert_eq!(judge(headline, 1, true), Verdict::Keep);
        let rust = "Use Rust to supercharge your JavaScript, one module at a time.";
        assert_eq!(judge(rust, 0, false), Verdict::Keep);
    }

    #[test]
    fn buttons_labels_and_dates_are_short() {
        for block in ["Learn more", "Shop now", "3 min read"] {
            assert_eq!(judge(block, 1, true), Verdict::Short, "{block}");
        }
    }

    #[test]
    fn brief_facts_and_prose_about_policies_code_and_copyright_are_kept() {
        for block in [
            "Motherboards",
            "1800+",
            "Privacy policy generator for small businesses and growing teams.",
            "Copyright law explained with practical examples for artists.",
            "Open daily",
            "555-0100",
            "We help businesses write a privacy policy that customers understand.",
            "Our JavaScript tools support browser applications with accessible user interfaces.",
            "Explore sets {a, b, c} using interactive mathematics lessons.",
            "Learn how copyright © protects the art you create.",
        ] {
            assert_eq!(judge(block, 0, false), Verdict::Keep, "{block}");
        }
    }

    #[test]
    fn notices_warnings_and_code_are_dropped() {
        for block in [
            "We use cookies to improve your experience on our site.",
            "By continuing you agree to our Terms of Use and Privacy Policy.",
            "© 2026 Acme Corporation",
            "Copyright 2026 Acme Corporation. Some rights reserved.",
            "Please make sure your browser supports JavaScript and cookies.",
            "You need to enable JavaScript to run this app.",
            "Your browser does not support the video tag.",
            "function init() { return 1; }",
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit.",
            "12:13 4:55 1:57 →",
        ] {
            assert_eq!(judge(block, 0, false), Verdict::Drop, "{block}");
        }
    }

    #[test]
    fn menus_of_links_are_dropped() {
        let menu = "Appliances Bath Building Materials Lumber Cleaning Home Décor Lighting";
        assert_eq!(judge(menu, 8, true), Verdict::Drop);
        // The same words with most of the text outside links are kept.
        assert_eq!(judge(menu, 8, false), Verdict::Keep);
    }

    #[test]
    fn spaceless_scripts_count_by_characters() {
        let chinese = "我们为全世界的用户提供安全可靠的搜索服务。";
        assert_eq!(judge(chinese, 0, false), Verdict::Keep);
        assert_eq!(judge("登录", 0, false), Verdict::Keep);
    }
}
