//! A county search that names its state: the sites of that state's county
//! before those of its namesakes elsewhere.

use plumb_core::normalize_text;
use plumb_core::place::US_STATES;

use crate::Hit;

/// Words for a county: Louisiana's are parishes, Alaska's boroughs.
const COUNTY_WORDS: [&str; 3] = ["county", "parish", "borough"];

/// Puts the sites that say they are in another state than the one `query`
/// ends in after the rest, when it names a county: "franklin county sheriff
/// ohio" wants Ohio's Franklin County, not one of the 23 others that share
/// its name. Sites that say nothing of a state keep their places among the
/// rest. The sites trade scores so that scores still go down the list.
pub fn asked_state_first(query: &str, hits: &mut Vec<Hit>) {
    let Some(asked) = asked_state(query) else {
        return;
    };
    let elsewhere = |hit: &Hit| {
        let states = site_states(hit);
        !hit.named && !states.is_empty() && !states.contains(&asked)
    };
    if !hits.iter().any(elsewhere) {
        return;
    }
    let mut scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    let (mut sorted, away): (Vec<Hit>, Vec<Hit>) = std::mem::take(hits)
        .into_iter()
        .partition(|hit| !elsewhere(hit));
    sorted.extend(away);
    for (hit, score) in sorted.iter_mut().zip(scores) {
        hit.score = score;
    }
    *hits = sorted;
}

/// The code of the state `query` ends in, after a word for a county:
/// "OH" for "franklin county sheriff ohio". Not for a state named first or
/// alone, where it is a county's name ("ohio county") or a person's.
fn asked_state(query: &str) -> Option<&'static str> {
    let query = normalize_text(query);
    let words: Vec<&str> = query.split(' ').collect();
    let county = words.iter().position(|word| COUNTY_WORDS.contains(word))?;
    state_names().into_iter().find_map(|(code, name)| {
        let length = name.split(' ').count();
        let after = words.len().checked_sub(length)?;
        (after > county && words[after..].join(" ") == name).then_some(code)
    })
}

/// The states' codes and names as [`normalize_text`] writes them, the
/// longest names first, so "west virginia" is not taken for Virginia.
fn state_names() -> Vec<(&'static str, String)> {
    let mut names: Vec<(&'static str, String)> = US_STATES
        .iter()
        .map(|(code, name)| (*code, normalize_text(name)))
        .collect();
    names.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    names
}

/// The codes of the states a site says it is in: by name in its title or
/// description ("county in Idaho, United States"), by code after a comma
/// or "County" ("Jefferson County, IN"), or by its domain
/// (franklincounty.ky.gov, lincolncountynv.org). A state's name before a
/// word for a county is a county's: "Washington County" is in Oregon too.
fn site_states(hit: &Hit) -> Vec<&'static str> {
    let raw = format!(
        "{} {}",
        hit.title.as_deref().unwrap_or(""),
        hit.description.as_deref().unwrap_or("")
    );
    let mut states = Vec::new();
    let mut found = |code: &'static str| {
        if !states.contains(&code) {
            states.push(code);
        }
    };
    let names = state_names();
    let mut text = format!(" {} ", normalize_text(&raw));
    for (code, name) in &names {
        let needle = format!(" {name} ");
        while let Some(at) = text.find(&needle) {
            let next = text[at + needle.len()..].split(' ').next().unwrap_or("");
            if !COUNTY_WORDS.contains(&next) {
                found(code);
            }
            text.replace_range(at..at + needle.len() - 1, " _");
        }
    }
    // A code only where addresses write one, as "IN" and "OR" are words.
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    for pair in tokens.windows(2) {
        let word = pair[1].trim_end_matches(|c: char| !c.is_alphanumeric());
        let after = pair[0].ends_with(',') || pair[0].eq_ignore_ascii_case("county");
        if let Some(&(code, _)) = US_STATES.iter().find(|(code, _)| *code == word) {
            if after {
                found(code);
            }
        }
    }
    let spells = |text: &str| {
        names
            .iter()
            .find(|(code, name)| text == code.to_ascii_lowercase() || text == name.replace(' ', ""))
            .map(|(code, _)| *code)
    };
    let labels: Vec<&str> = hit.domain.split('.').collect();
    // A state's own part of .gov or .us: franklincounty.ky.gov,
    // madisonco.virginia.gov, co.lincoln.or.us.
    if let [.., state, "gov" | "us"] = labels[..] {
        if let Some(code) = spells(state) {
            found(code);
        }
    }
    // Or what follows "county" in the name: lincolncountynv.org.
    let label = labels.first().unwrap_or(&"").replace('-', "");
    if let Some(code) = label
        .rfind("county")
        .and_then(|at| spells(&label[at + "county".len()..]))
    {
        found(code);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(domain: &str, title: &str, description: &str, score: f32) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(title.into()),
            description: (!description.is_empty()).then(|| description.into()),
            score,
            text_score: 0.5,
            link_score: 0.3,
            placing_text_score: None,
            country: None,
            named: false,
            official: true,
            key_pages: Vec::new(),
            demand: None,
            missing_words: true,
            query_evidence: None,
        }
    }

    fn domains(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.domain.as_str()).collect()
    }

    #[test]
    fn a_county_search_lists_its_states_county_first() {
        // As plumbsearch.org listed them for "lincoln county sheriff oregon".
        let mut hits = vec![
            site(
                "lincolncountynv.org",
                "Lincoln County, Nevada Government -",
                "county in Nevada, United States",
                0.9,
            ),
            site("lincolncountywv.org", "Lincoln County WV | Home", "", 0.8),
            site(
                "lincolncity.org",
                "Lincoln City",
                "city in Lincoln County, Oregon, United States",
                0.7,
            ),
            site("lincolncountyid.us", "Lincoln County", "", 0.6),
            site("washingtoncounty.in.gov", "Washington County", "", 0.5),
            site(
                "mcso.us",
                "Home | Multnomah County Sheriff's Office",
                "",
                0.4,
            ),
            site("washingtoncountyor.gov", "Washington County, OR", "", 0.3),
        ];
        asked_state_first("lincoln county sheriff oregon", &mut hits);
        assert_eq!(
            domains(&hits),
            [
                "lincolncity.org",
                "mcso.us",
                "washingtoncountyor.gov",
                "lincolncountynv.org",
                "lincolncountywv.org",
                "lincolncountyid.us",
                "washingtoncounty.in.gov",
            ]
        );
        let scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
        assert_eq!(scores, [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3]);
    }

    #[test]
    fn only_a_state_after_a_county_is_asked_for() {
        assert_eq!(asked_state("franklin county sheriff ohio"), Some("OH"));
        assert_eq!(
            asked_state("Jefferson County clerk, West Virginia"),
            Some("WV")
        );
        assert_eq!(asked_state("orleans parish louisiana"), Some("LA"));
        // A county named for a state, a state first, and none at all.
        for query in [
            "ohio county",
            "washington county jail",
            "virginia beach county",
            "tom holland oregon",
        ] {
            assert_eq!(asked_state(query), None, "{query}");
        }
    }

    #[test]
    fn a_site_says_its_state_by_name_code_or_domain() {
        let states = |domain: &str, title: &str, description: &str| {
            site_states(&site(domain, title, description, 1.0))
        };
        assert_eq!(
            states("franklincounty.ky.gov", "Franklin County | KENTUCKY", ""),
            ["KY"]
        );
        assert_eq!(
            states("example.org", "Lincoln County WV | Home", ""),
            ["WV"]
        );
        assert_eq!(
            states("example.org", "Franklin County AL Sheriff's Office", ""),
            ["AL"]
        );
        assert_eq!(
            states("example.org", "Clay County Info", "Piggott, AR"),
            ["AR"]
        );
        assert_eq!(
            states("madisonco.virginia.gov", "Madison County", ""),
            ["VA"]
        );
        assert_eq!(states("co.lincoln.or.us", "Lincoln County", ""), ["OR"]);
        assert_eq!(states("franklincountyohio.gov", "Home", ""), ["OH"]);
        assert_eq!(
            states("washco-md.net", "Washington County", ""),
            Vec::<&str>::new()
        );
        assert_eq!(
            states("example.org", "Welcome to West Virginia", ""),
            ["WV"]
        );
        // A county named for a state, and words that are codes.
        assert!(states("example.org", "Washington County", "").is_empty());
        assert!(states("example.org", "Life IN the city OR the county", "").is_empty());
    }
}
