//! Tool answers as short plain text, which is what the model reads: one
//! line per result with its address, no JSON punctuation, nothing empty.
//! A small local model has a few thousand tokens to spare, and the same
//! answer as pretty JSON takes three to four times as many.

use std::fmt::Write as _;

use serde_json::Value;

/// `answer` of tool `tool` as text.
pub(super) fn render(tool: &str, answer: &Value) -> String {
    let mut out = String::new();
    match tool {
        "official_site" => official_site(&mut out, answer),
        "check_lookalike" => check_lookalike(&mut out, answer),
        "search" => search(&mut out, answer),
        "site_info" => site_info(&mut out, answer),
        "facts" => facts(&mut out, answer),
        "package" => package(&mut out, answer),
        "report_finding" => {
            let _ = write!(
                out,
                "Kept. The next search for \"{}\" on this computer starts with this answer.",
                text(answer, "query").unwrap_or("")
            );
            if let Some(shared) = answer.get("shared") {
                if shared["shared"] == true {
                    let _ = write!(
                        out,
                        " Shared with other Plumb nodes for {} days: {}",
                        shared["expires_in_days"].as_u64().unwrap_or(0),
                        text(shared, "url").unwrap_or("")
                    );
                } else {
                    let _ = write!(
                        out,
                        " Not shared: {}",
                        text(shared, "why_not").unwrap_or("")
                    );
                }
            }
        }
        "read_page" => read_page(&mut out, answer),
        "relate" => relate(&mut out, answer),
        _ => out = serde_json::to_string(answer).unwrap_or_default(),
    }
    out.trim_end().to_string()
}

/// A string field, when present and not empty.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// An instant answer in one line: "12 × 7 = 84", "Time in Tokyo, Japan:
/// 9:41 PM". A sum's question already ends with "=".
pub(crate) fn answer_line(question: &str, answer: &str) -> String {
    if question.ends_with('=') {
        format!("{question} {answer}")
    } else {
        format!("{question}: {answer}")
    }
}

/// "PayPal (paypal.com, official) https://www.paypal.com/", and the
/// description on the next line, indented by `indent`.
fn site_line(out: &mut String, site: &Value, indent: &str) {
    let domain = text(site, "domain").unwrap_or("");
    let mut marks = vec![domain];
    if flag(site, "official") {
        marks.push("official");
    } else if flag(site, "well_known") {
        marks.push("well known");
    }
    match text(site, "title") {
        Some(title) if title != domain => {
            let _ = write!(out, "{title} ({})", marks.join(", "));
        }
        _ => {
            let _ = write!(out, "{}", marks.join(", "));
        }
    }
    if let Some(url) = text(site, "url") {
        let _ = write!(out, " {url}");
    }
    out.push('\n');
    if let Some(description) = text(site, "description") {
        let _ = writeln!(out, "{indent}{description}");
    }
}

/// What kind of page a page set holds, as a label.
fn set_label(set: &str) -> &str {
    match set {
        s if s.starts_with("wikipedia") => "Wikipedia",
        "wikidata" => "Wikidata",
        "stackoverflow" => "Stack Overflow",
        "stackexchange" => "Stack Exchange",
        "github" => "GitHub",
        "books" => "Book",
        "papers" => "Paper",
        other => other,
    }
}

fn page_line(out: &mut String, page: &Value) {
    if let Some(card) = page.get("package").filter(|p| p.is_object()) {
        package_line(out, card);
        return;
    }
    let set = set_label(text(page, "set").unwrap_or("Page"));
    let _ = write!(
        out,
        "[{set}] {}",
        text(page, "title").unwrap_or("(untitled)")
    );
    if let Some(description) = text(page, "description") {
        let _ = write!(out, ": {description}");
    }
    if let Some(url) = text(page, "url") {
        let _ = write!(out, " {url}");
    }
    out.push('\n');
}

/// "[crates.io] serde 1.0.228 (2025-09-27, MIT OR Apache-2.0): A generic
/// serialization framework. Install: cargo add serde. Docs: … Code: …
/// https://crates.io/crates/serde"
fn package_line(out: &mut String, card: &Value) {
    let _ = write!(
        out,
        "[{}] {}",
        text(card, "registry").unwrap_or("Package"),
        text(card, "name").unwrap_or("")
    );
    if let Some(version) = text(card, "version") {
        let _ = write!(out, " {version}");
    }
    let when: Vec<&str> = [text(card, "released"), text(card, "license")]
        .into_iter()
        .flatten()
        .collect();
    if !when.is_empty() {
        let _ = write!(out, " ({})", when.join(", "));
    }
    if let Some(description) = text(card, "description") {
        let _ = write!(out, ": {description}");
    }
    if let Some(install) = text(card, "install") {
        let _ = write!(out, " Install: {install}.");
    }
    for (label, key) in [("Docs", "docs"), ("Code", "repo"), ("Home", "homepage")] {
        if let Some(url) = text(card, key) {
            let _ = write!(out, " {label}: {url}");
        }
    }
    if let Some(url) = text(card, "url") {
        let _ = write!(out, " {url}");
    }
    out.push('\n');
}

fn package(out: &mut String, answer: &Value) {
    let packages = list(answer, "packages");
    if packages.is_empty() {
        let _ = writeln!(
            out,
            "No package called \"{}\" among the packages Plumb knows (the most used of each registry).",
            text(answer, "name").unwrap_or("")
        );
    }
    for card in packages {
        package_line(out, card);
    }
}

fn search(out: &mut String, answer: &Value) {
    for found in list(answer, "found_before") {
        let _ = write!(
            out,
            "Found before (searched \"{}\", {}): {}",
            text(found, "query").unwrap_or(""),
            text(found, "reported").unwrap_or(""),
            text(found, "answer").unwrap_or("")
        );
        if let Some(why) = text(found, "why") {
            let _ = write!(out, " Source: {} ({why})", text(found, "url").unwrap_or(""));
        }
        out.push('\n');
    }
    for lead in list(answer, "leads") {
        let by = list(lead, "reported_by");
        let trusted = by
            .iter()
            .filter(|r| text(r, "relation") == Some("trusted"))
            .count();
        let _ = write!(
            out,
            "Lead shared by {} other Plumb node{}{} ({}; unchecked, read it first): {}",
            by.len(),
            if by.len() == 1 { "" } else { "s" },
            if trusted > 0 {
                format!(", {trusted} trusted")
            } else {
                String::new()
            },
            text(lead, "reported").unwrap_or(""),
            text(lead, "url").unwrap_or("")
        );
        if let Some(why) = text(lead, "why") {
            let _ = write!(out, " ({why})");
        }
        out.push('\n');
    }
    if let Some(a) = answer.get("answer").filter(|a| a.is_object()) {
        let _ = write!(
            out,
            "Answer: {}",
            answer_line(
                text(a, "question").unwrap_or(""),
                text(a, "answer").unwrap_or("")
            )
        );
        if let Some(note) = text(a, "note") {
            let _ = write!(out, " ({note})");
        }
        out.push('\n');
    }
    if let Some(p) = answer.get("profile").filter(|p| p.is_object()) {
        let _ = writeln!(
            out,
            "Official profile: {} on {}: {}",
            text(p, "of").unwrap_or(""),
            text(p, "service").unwrap_or(""),
            text(p, "url").unwrap_or("")
        );
    }
    if let Some(about) = answer.get("about").filter(|a| a.is_object()) {
        let _ = write!(out, "About {}", text(about, "title").unwrap_or(""));
        if let Some(description) = text(about, "description") {
            let _ = write!(out, ": {description}");
        }
        out.push('.');
        if let Some(site) = text(about, "site") {
            let _ = write!(out, " Official site: {site}");
            if let Some(country) = text(about, "country") {
                let _ = write!(out, " ({country})");
            }
            out.push('.');
        }
        let profiles: Vec<String> = list(about, "profiles")
            .iter()
            .filter_map(|p| Some(format!("{} {}", text(p, "service")?, text(p, "url")?)))
            .collect();
        if !profiles.is_empty() {
            let _ = write!(out, " Profiles: {}.", profiles.join(", "));
        }
        if let Some(article) = text(about, "article") {
            let _ = write!(out, " {article}");
        }
        out.push('\n');
    }
    if let Some(spelling) = answer.get("spelling").filter(|s| s.is_object()) {
        let query = text(spelling, "query").unwrap_or("");
        let _ = writeln!(out, "Did you mean \"{query}\"?");
    }

    let sites = list(answer, "results");
    let pages = list(answer, "pages");
    let mut n = 0;
    let mut numbered = |out: &mut String| {
        n += 1;
        let _ = write!(out, "{n}. ");
    };
    for (i, site) in sites.iter().enumerate() {
        for page in pages.iter().filter(|p| {
            p.get("about_site").is_none_or(Value::is_null)
                && p.get("position").and_then(Value::as_u64) == Some(i as u64 + 1)
        }) {
            numbered(out);
            page_line(out, page);
        }
        numbered(out);
        site_line(out, site, "   ");
        let domain = site.get("domain");
        for page in pages.iter().filter(|p| p.get("about_site") == domain) {
            out.push_str("   ");
            page_line(out, page);
        }
    }
    for page in pages.iter().filter(|p| {
        p.get("about_site").is_none_or(Value::is_null)
            && p.get("position")
                .and_then(Value::as_u64)
                .is_none_or(|at| at > sites.len() as u64)
    }) {
        numbered(out);
        page_line(out, page);
    }
    if sites.is_empty() && pages.is_empty() {
        let _ = writeln!(
            out,
            "No results for \"{}\".",
            text(answer, "query").unwrap_or("")
        );
    }
    if let Some(site_search) = answer.get("site_search").filter(|s| s.is_object()) {
        let _ = writeln!(
            out,
            "Search {} itself for \"{}\": {}",
            text(site_search, "domain").unwrap_or(""),
            text(site_search, "terms").unwrap_or(""),
            text(site_search, "url").unwrap_or("")
        );
    }
    let recent = list(answer, "recent");
    if !recent.is_empty() {
        out.push_str("Recent headlines:\n");
        for headline in recent {
            let _ = writeln!(
                out,
                "- {} ({}, {}) {}",
                text(headline, "title").unwrap_or(""),
                text(headline, "site").unwrap_or(""),
                text(headline, "published").unwrap_or(""),
                text(headline, "url").unwrap_or("")
            );
        }
    }
    for found in list(answer, "plugins") {
        let _ = writeln!(
            out,
            "From the {} plugin on this node (not Plumb's index):",
            text(found, "plugin").unwrap_or("")
        );
        for item in list(found, "results") {
            let mut about = text(item, "site").unwrap_or("").to_string();
            if let Some(published) = text(item, "published") {
                about = format!("{about}, {published}");
            }
            let _ = writeln!(
                out,
                "- {} ({about}) {}",
                text(item, "title").unwrap_or(""),
                text(item, "url").unwrap_or("")
            );
            if let Some(snippet) = text(item, "snippet") {
                let _ = writeln!(out, "  {snippet}");
            }
        }
    }
}

fn official_site(out: &mut String, answer: &Value) {
    let name = text(answer, "name").unwrap_or("");
    if !flag(answer, "found") {
        let _ = writeln!(out, "Plumb knows no site called \"{name}\".");
        why(out, answer, "why");
        if let Some(fixed) = text(answer, "did_you_mean") {
            let _ = writeln!(out, "Did you mean \"{fixed}\"?");
        }
        return;
    }
    let _ = writeln!(
        out,
        "Official site for \"{name}\": {} {} (confidence {})",
        text(answer, "domain").unwrap_or(""),
        text(answer, "url").unwrap_or(""),
        text(answer, "confidence").unwrap_or("low")
    );
    if let Some(title) = text(answer, "title") {
        let _ = write!(out, "{title}");
        if let Some(description) = text(answer, "description") {
            let _ = write!(out, ": {description}");
        }
        out.push('\n');
    }
    why(out, answer, "why");
    let others: Vec<&str> = list(answer, "alternatives")
        .iter()
        .filter_map(|site| text(site, "domain"))
        .collect();
    if !others.is_empty() {
        let _ = writeln!(out, "Other candidates: {}", others.join(", "));
    }
    if let Some(fixed) = text(answer, "did_you_mean") {
        let _ = writeln!(out, "Did you mean \"{fixed}\"?");
    }
}

fn why(out: &mut String, answer: &Value, key: &str) {
    let reasons: Vec<&str> = list(answer, key).iter().filter_map(Value::as_str).collect();
    if !reasons.is_empty() {
        let _ = writeln!(out, "Why: {}", reasons.join(" "));
    }
}

fn check_lookalike(out: &mut String, answer: &Value) {
    let input = text(answer, "input").unwrap_or("");
    match text(answer, "verdict").unwrap_or("unknown") {
        "lookalike" => {
            let _ = write!(out, "{input}: LOOK-ALIKE. Do not trust it");
            if let Some(real) = answer.get("imitates").filter(|r| r.is_object()) {
                let _ = write!(
                    out,
                    "; the real site is {} {}",
                    text(real, "domain").unwrap_or(""),
                    text(real, "url").unwrap_or("")
                );
            }
            out.push_str(".\n");
        }
        verdict => {
            let said = match verdict {
                "official" => "the official site",
                "known_site" => "a well-known site",
                "little_known" => "a little-known site, not a known look-alike",
                _ => "not known to Plumb",
            };
            let _ = writeln!(out, "{input}: {said} (verdict {verdict}).");
        }
    }
    why(out, answer, "reasons");
}

/// One fact a line, each with its question and where it is from:
/// "Capital of Australia: Canberra (Wikidata Q408 P36)".
fn relate(out: &mut String, answer: &Value) {
    let subject = text(answer, "subject").unwrap_or("");
    let chain: Vec<&str> = list(answer, "relation")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // "capital of headquarters of Toyota": the last step first.
    let path = chain.iter().rev().copied().collect::<Vec<_>>().join(" of ");
    if !flag(answer, "found") {
        let _ = writeln!(
            out,
            "Plumb knows nothing named {subject} to find the {path} of."
        );
        return;
    }
    let from = text(answer, "subject_title").unwrap_or(subject);
    let stated = |value: &Value| {
        if flag(value, "stated") {
            "stated in Wikidata"
        } else {
            "a learned guess"
        }
    };
    if let Some(claim) = answer.get("claim") {
        let object = text(claim, "object").unwrap_or("");
        if !flag(claim, "found") {
            let _ = writeln!(out, "Plumb knows nothing named {object} to check.");
            return;
        }
        let probability = claim["probability"].as_f64().unwrap_or(0.0);
        let _ = writeln!(
            out,
            "{path} of {from}: {} has probability {probability:.2} ({}).",
            text(claim, "object_title").unwrap_or(object),
            stated(claim)
        );
        return;
    }
    let _ = writeln!(out, "{path} of {from}, likeliest first:");
    for item in list(answer, "answers") {
        let title = text(item, "title").unwrap_or("");
        let _ = write!(out, "{title}");
        if let Some(description) = text(item, "description") {
            let _ = write!(out, ", {description}");
        }
        let probability = item["probability"].as_f64().unwrap_or(0.0);
        let _ = writeln!(
            out,
            " ({probability:.2}, {}) [Wikidata {}]",
            stated(item),
            text(item, "item").unwrap_or("")
        );
    }
}

fn facts(out: &mut String, answer: &Value) {
    if !flag(answer, "found") {
        let subject = text(answer, "subject").unwrap_or("");
        let _ = writeln!(out, "Plumb has no facts about {subject}.");
        return;
    }
    let title = text(answer, "title").unwrap_or("");
    let _ = write!(out, "{title}");
    if let Some(description) = text(answer, "description") {
        let _ = write!(out, ", {description}");
    }
    if let Some(url) = text(answer, "url") {
        let _ = write!(out, " {url}");
    }
    out.push('\n');
    let item = text(answer, "item");
    for fact in list(answer, "facts") {
        let question = text(fact, "question").unwrap_or("");
        let value = text(fact, "value").unwrap_or("");
        let _ = write!(out, "{}", answer_line(question, value));
        if let Some(note) = text(fact, "note") {
            let _ = write!(out, " ({note})");
        }
        let property = text(fact, "property").unwrap_or("");
        match item {
            Some(item) => {
                let _ = writeln!(out, " [Wikidata {item} {property}]");
            }
            None => {
                let _ = writeln!(out, " [Wikidata {property}]");
            }
        }
    }
    if let Some(url) = text(answer, "item_url") {
        let _ = writeln!(out, "Source: {url}");
    }
}

fn site_info(out: &mut String, answer: &Value) {
    let domain = text(answer, "domain").unwrap_or("");
    if let Some(host) = text(answer, "host") {
        let _ = writeln!(
            out,
            "Plumb keeps no entry of its own for {host}; it is part of {domain}."
        );
    }
    if let Some(read) = answer.get("read_now") {
        let _ = write!(out, "Read now: {}", text(read, "url").unwrap_or(""));
        if let Some(title) = text(read, "title") {
            let _ = write!(out, " \"{title}\"");
        }
        out.push('\n');
        if let Some(opening) = text(read, "opening") {
            let _ = writeln!(out, "  {opening}");
        }
    }
    if let Some(error) = text(answer, "read_error") {
        let _ = writeln!(out, "Read now: {error}");
    }
    if !flag(answer, "found") {
        let _ = writeln!(out, "Plumb does not know {domain}.");
        return;
    }
    let _ = write!(out, "{domain}");
    if let Some(title) = text(answer, "title") {
        let _ = write!(out, ": {title}");
    }
    if let Some(url) = text(answer, "url") {
        let _ = write!(out, " {url}");
    }
    out.push('\n');
    if let Some(description) = text(answer, "description") {
        let _ = writeln!(out, "{description}");
    }
    let mut facts = Vec::new();
    if let Some(of) = text(answer, "official_for") {
        facts.push(format!("Wikidata gives it as the official website of {of}"));
    } else if flag(answer, "official") {
        facts.push("Wikidata lists it as an official website".to_string());
    }
    facts.push(if flag(answer, "well_known") {
        "well known".to_string()
    } else {
        "not well known".to_string()
    });
    if let Some(country) = text(answer, "country") {
        facts.push(format!("country {country}"));
    }
    let _ = writeln!(out, "{}.", facts.join("; "));
    for page in list(answer, "pages") {
        let _ = writeln!(
            out,
            "Page: {} {}",
            text(page, "title").unwrap_or(""),
            text(page, "url").unwrap_or("")
        );
    }
}

fn read_page(out: &mut String, answer: &Value) {
    if let Some(title) = text(answer, "title") {
        let _ = writeln!(out, "{title}");
    }
    let _ = writeln!(out, "{}", text(answer, "url").unwrap_or(""));
    if let Some(site) = answer.get("site").filter(|s| s.is_object()) {
        if text(site, "verdict") == Some("lookalike") {
            let _ = write!(out, "WARNING: this site is a look-alike");
            if let Some(real) = text(site, "imitates") {
                let _ = write!(out, " of {real}");
            }
            out.push_str("; do not trust it.\n");
        }
    }
    out.push('\n');
    let sections = list(answer, "outline");
    if !sections.is_empty() {
        let length = answer.get("length").and_then(Value::as_u64).unwrap_or(0);
        let _ = writeln!(
            out,
            "Outline of {length} characters. To read a section, call read_page with its start."
        );
        for section in sections {
            let level = section.get("level").and_then(Value::as_u64).unwrap_or(0) as usize;
            let start = section.get("start").and_then(Value::as_u64).unwrap_or(0);
            let heading = text(section, "heading").unwrap_or("");
            let _ = write!(out, "start={start} ");
            if level == 0 {
                out.push_str("(before the first heading)");
            } else {
                let _ = write!(out, "{} {heading}", "#".repeat(level));
            }
            match text(section, "opening").filter(|o| !o.is_empty()) {
                Some(opening) => {
                    let _ = writeln!(out, ": {opening}");
                }
                None => out.push('\n'),
            }
        }
        if flag(answer, "truncated") {
            out.push_str("[The page is very long; only its first few megabytes were read.]\n");
        }
        return;
    }
    if answer.get("found").and_then(Value::as_bool) == Some(false) {
        out.push_str("[The words to find are not on the page; this part starts where asked.]\n\n");
    }
    let _ = writeln!(out, "{}", text(answer, "text").unwrap_or("(no text)"));
    let length = answer.get("length").and_then(Value::as_u64).unwrap_or(0);
    let start = answer.get("start").and_then(Value::as_u64).unwrap_or(0);
    let end = answer.get("end").and_then(Value::as_u64).unwrap_or(length);
    if let Some(next) = answer.get("next_start").and_then(Value::as_u64) {
        let _ = writeln!(
            out,
            "\n[Characters {start} to {end} of {length}. For more, call read_page again with start={next}.]"
        );
    } else if start > 0 {
        let _ = writeln!(out, "\n[Characters {start} to {end} of {length}: the end.]");
    }
    if flag(answer, "truncated") {
        out.push_str("[The page is very long; only its first few megabytes were read.]\n");
    }
    let links = list(answer, "links");
    if !links.is_empty() {
        out.push_str("\nLinks:\n");
        for link in links {
            let _ = writeln!(
                out,
                "- {}: {}",
                text(link, "text").unwrap_or(""),
                text(link, "url").unwrap_or("")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn search_lists_one_line_per_result_with_pages_in_place() {
        let answer = json!({
            "query": "paypal",
            "answer": null,
            "about": {
                "title": "PayPal", "description": "American payments company",
                "site": "paypal.com", "country": "United States",
                "article": "https://en.wikipedia.org/wiki/PayPal",
            },
            "results": [
                { "domain": "paypal.com", "url": "https://www.paypal.com/", "title": "PayPal",
                  "description": "Send money.", "official": true, "well_known": true },
                { "domain": "paypal.me", "url": "https://paypal.me/", "title": null,
                  "description": null, "official": false, "well_known": false },
            ],
            "pages": [
                { "title": "PayPal", "url": "https://en.wikipedia.org/wiki/PayPal",
                  "description": "American payments company", "set": "wikipedia-en",
                  "about_site": "paypal.com", "position": 1 },
                { "title": "How do I refund?", "url": "https://stackoverflow.com/q/1",
                  "description": null, "set": "stackoverflow", "about_site": null, "position": 2 },
            ],
            "site_search": null,
            "spelling": null,
            "recent": [{ "title": "PayPal launches X", "url": "https://news.example/a",
                         "site": "news.example", "published": "2 hours ago" }],
        });
        assert_eq!(
            render("search", &answer),
            "About PayPal: American payments company. Official site: paypal.com (United \
             States). https://en.wikipedia.org/wiki/PayPal\n\
             1. PayPal (paypal.com, official) https://www.paypal.com/\n   Send money.\n   \
             [Wikipedia] PayPal: American payments company https://en.wikipedia.org/wiki/PayPal\n\
             2. [Stack Overflow] How do I refund? https://stackoverflow.com/q/1\n\
             3. paypal.me https://paypal.me/\n\
             Recent headlines:\n\
             - PayPal launches X (news.example, 2 hours ago) https://news.example/a"
        );
    }

    #[test]
    fn search_leads_with_the_answer() {
        let answer = json!({
            "query": "12*7",
            "answer": { "kind": "calculation", "question": "12 × 7 =", "answer": "84" },
            "results": [], "pages": [],
        });
        assert_eq!(
            render("search", &answer),
            "Answer: 12 × 7 = 84\nNo results for \"12*7\"."
        );
    }

    #[test]
    fn read_page_says_where_the_next_part_starts() {
        let answer = json!({
            "url": "https://example.com/", "title": "Example", "text": "Hello",
            "start": 0, "end": 5, "length": 12, "more": true, "next_start": 5,
            "truncated": false, "site": { "verdict": "lookalike", "imitates": "paypal.com" },
        });
        assert_eq!(
            render("read_page", &answer),
            "Example\nhttps://example.com/\nWARNING: this site is a look-alike of paypal.com; do \
             not trust it.\n\nHello\n\n[Characters 0 to 5 of 12. For more, call read_page again \
             with start=5.]"
        );
    }

    #[test]
    fn read_page_outlines_list_where_sections_start() {
        let answer = json!({
            "url": "https://example.com/", "title": "Example", "length": 900,
            "truncated": false,
            "outline": [
                { "level": 0, "heading": "", "start": 0, "opening": "Intro." },
                { "level": 1, "heading": "Usage", "start": 8, "opening": "Run it." },
                { "level": 2, "heading": "Flags", "start": 40, "opening": "" },
            ],
        });
        assert_eq!(
            render("read_page", &answer),
            "Example\nhttps://example.com/\n\nOutline of 900 characters. To read a section, call \
             read_page with its start.\nstart=0 (before the first heading): Intro.\nstart=8 # \
             Usage: Run it.\nstart=40 ## Flags"
        );
    }

    #[test]
    fn lookalikes_are_spelled_out() {
        let answer = json!({
            "input": "paypal-login.us", "verdict": "lookalike",
            "reasons": ["Its address borrows the name of paypal.com."],
            "imitates": { "domain": "paypal.com", "url": "https://www.paypal.com/" },
        });
        assert_eq!(
            render("check_lookalike", &answer),
            "paypal-login.us: LOOK-ALIKE. Do not trust it; the real site is paypal.com \
             https://www.paypal.com/.\nWhy: Its address borrows the name of paypal.com."
        );
    }
}
