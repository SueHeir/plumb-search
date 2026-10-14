//! The `check_lookalike` tool: whether an address is the real site or one
//! made to look like another, and which site it imitates.

use super::*;

/// Most searches one `check_lookalike` runs.
const MAX_LOOKALIKE_SEARCHES: usize = 6;

impl Mcp {
    /// `check_lookalike`: whether `input` is a real site or imitates one.
    pub fn check_lookalike(&self, input: &str, options: &SearchOptions) -> Result<Value> {
        let Some(host) = host_of(input) else {
            bail!("{input:?} is not a web address or domain name");
        };
        let Some(domain) = registrable_domain(&host) else {
            bail!("{host:?} has no registrable domain");
        };
        let mut searches = 0;
        // The site itself: a typed hostname names it.
        let own = self.lookup(&domain, 1, options)?;
        searches += 1;
        let site = own.hits.into_iter().find(|hit| hit.domain == domain);
        let site_trusted = site
            .as_ref()
            .is_some_and(|hit| hit.official || hit.link_score >= WELL_KNOWN_LINK_SCORE);

        // What its names spell: the label ("paypal-login" -> "paypal
        // login"), then each word of it and of any subdomain
        // ("paypal.com.secure-check.io" -> "paypal"). Of the sites they
        // find, one with the host's own name on another ending comes
        // first (shop.paypal.de is paypal.com's namesake, not shop.app's),
        // then the one whose name comes first in the host: bbc-news-live.com
        // is the BBC's, not live.com's.
        let mut imitated: Option<((bool, usize), Hit)> = None;
        let squashed_host = squash(&host);
        let label = squash(&domain_label(&domain));
        let mut queries: std::collections::VecDeque<String> =
            name_queries(&host, &domain).into_iter().collect();
        while let Some(query) = queries.pop_front() {
            if searches >= MAX_LOOKALIKE_SEARCHES {
                break;
            }
            searches += 1;
            let results = self.lookup(&query, 1, options)?;
            // A typo of a name ("twiter") is searched as typed; the name
            // it is a typo of is looked up next.
            if let Some(spelling) = results.spelling {
                queries.push_front(spelling.query);
            }
            let Some(top) = results.hits.into_iter().next() else {
                continue;
            };
            if top.domain == domain || !(top.official || top.link_score >= WELL_KNOWN_LINK_SCORE) {
                continue;
            }
            let Some(at) = borrowed_at(&squashed_host, &label, &top.domain, top.title.as_deref())
            else {
                continue;
            };
            let rank = (!same_name(&domain, &top.domain), at);
            if imitated
                .as_ref()
                .is_none_or(|(best, hit)| (rank, -top.link_score) < (*best, -hit.link_score))
            {
                imitated = Some((rank, top));
            }
        }
        let imitated = imitated.map(|(_, hit)| hit);

        let mut reasons = Vec::new();
        if host != domain {
            reasons.push(format!("{host} is part of {domain}."));
        }
        let verdict = match (&site, &imitated) {
            (Some(site), _) if site.official => {
                reasons.push("Wikidata lists it as an official website.".to_string());
                "official"
            }
            (Some(_), Some(other)) if site_trusted => {
                reasons.push(format!(
                    "It is a well-known site in its own right, though its name is close to {}.",
                    other.domain
                ));
                "known_site"
            }
            (Some(_), None) if site_trusted => {
                reasons.push("It is a well-known site, linked from many others.".to_string());
                "known_site"
            }
            (_, Some(other)) => {
                // An exact brand label on another suffix or hosting tenant
                // can also be a legitimate regional/product/service host.
                // Service words do not prove affiliation or impersonation.
                if same_name(&domain, &other.domain) {
                    reasons.push(format!(
                        "Its name resembles {}, but Plumb has no owner reference establishing \
                         affiliation or evidence confirming impersonation.",
                        other.domain
                    ));
                    "suspected"
                } else {
                    reasons.push(format!(
                        "Its address borrows the name of {}, a far better-known site.",
                        other.domain
                    ));
                    if site.is_none() {
                        reasons.push("Plumb does not list it as a site of its own.".to_string());
                    } else {
                        reasons.push("Few other sites link to it.".to_string());
                    }
                    "lookalike"
                }
            }
            (Some(_), None) => {
                reasons.push(
                    "Plumb knows the site but it is little known; nothing suggests it imitates \
                     another site."
                        .to_string(),
                );
                "little_known"
            }
            (None, None) => {
                reasons.push(
                    "Plumb does not know this site. That alone proves nothing either way."
                        .to_string(),
                );
                "unknown"
            }
        };
        Ok(json!({
            "input": input,
            "host": host,
            "domain": domain,
            "verdict": verdict,
            // True for a suspected look-alike too, as 0.2.0 said of an
            // exact name on another ending: apps that check this before
            // entering credentials keep stopping at paypal.co.
            "lookalike": matches!(verdict, "lookalike" | "suspected"),
            "suspected": verdict == "suspected",
            "reasons": reasons,
            "imitates": matches!(verdict, "lookalike" | "suspected").then(|| imitated.as_ref().map(brief)).flatten(),
            "site": site.as_ref().map(brief),
        }))
    }
}

/// What to search for to find the site a host name borrows from: its
/// label as words (without words like "login" at the end), then the other
/// words of the label and the subdomains, longest first.
pub(super) fn name_queries(host: &str, domain: &str) -> Vec<String> {
    let label = domain_label(domain);
    let label_words = label.replace(['-', '_'], " ");
    let mut queries = vec![without_intent_words(&label_words).unwrap_or(label_words)];
    if label.contains("xn--") {
        queries.insert(0, squash(&label));
    }
    let subdomains = host.strip_suffix(domain).unwrap_or("");
    let mut words: Vec<String> = subdomains
        .split(['.', '-', '_'])
        .chain(label.split(['-', '_']))
        .map(str::to_ascii_lowercase)
        .filter(|word| word.len() >= 4 && !FILLER_WORDS.contains(&word.as_str()))
        .collect();
    words.sort_by_key(|word| std::cmp::Reverse(word.len()));
    let mut seen: HashSet<String> = queries.iter().cloned().collect();
    for word in words {
        if seen.insert(word.clone()) {
            queries.push(word);
        }
    }
    queries
}

/// Letters and digits only, lowercase, with digits and letter pairs that
/// pass for letters read as them: `paypa1` -> `paypal`, `rnicrosoft` ->
/// `microsoft`.
pub(super) fn squash(text: &str) -> String {
    let (unicode, _) = idna::domain_to_unicode(text);
    let plain: String = unicode
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '0' => 'o',
            '1' => 'l',
            '3' => 'e',
            '5' => 's',
            // Common Cyrillic/Greek homographs; retain the literal ASCII
            // hostname for ownership checks, using this only for resemblance.
            'а' | 'α' => 'a',
            'е' | 'ε' => 'e',
            'о' | 'ο' => 'o',
            'р' | 'ρ' => 'p',
            'с' => 'c',
            'у' | 'ү' => 'y',
            'х' | 'χ' => 'x',
            'і' | 'ι' => 'i',
            'ј' => 'j',
            'ѕ' => 's',
            'к' | 'κ' => 'k',
            'м' => 'm',
            'т' | 'τ' => 't',
            c => c,
        })
        .filter(char::is_ascii_alphanumeric)
        .collect();
    plain.replace("rn", "m").replace("vv", "w")
}

/// Where a host whose letters are `host` and whose registrable label's
/// are `label` borrows the name of the site `real`, titled `title`, if it
/// does: where it spells out the real site's label (paypal-login.us,
/// paypal.com.example.io) or, for a site on another's domain that its
/// title calls by its first name in capitals, that name (AWS of
/// aws.amazon.com in aws-console-login.com, but not Maps of
/// maps.google.com), or where its label sits when that is a typo of it
/// (twiter.com, paypa1.com).
pub(super) fn borrowed_at(
    host: &str,
    label: &str,
    real: &str,
    title: Option<&str>,
) -> Option<usize> {
    let real_label = domain_label(real);
    let acronym = real_label
        .split_once('.')
        .map(|(first, _)| first)
        .filter(|first| {
            let first = first.to_ascii_uppercase();
            title.is_some_and(|title| {
                title
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|word| word == first)
            })
        });
    let real = squash(&real_label);
    let spelled = std::iter::once(real.clone())
        .chain(acronym.map(squash))
        .filter(|name| name.len() >= 3)
        .filter_map(|name| host.find(name.as_str()))
        .min();
    let allowed = if real.len() >= 8 { 2 } else { 1 };
    let typo = real.len() >= 4 && edit_distance(label, &real) <= allowed;
    spelled.or(typo.then(|| host.rfind(label).unwrap_or(0)))
}

/// Whether `domain` has the name of `other` on another ending: paypal.de
/// and paypal.com, or aws.dev and aws.amazon.com.
fn same_name(domain: &str, other: &str) -> bool {
    let (label, other) = (domain_label(domain), domain_label(other));
    label == other
        || other
            .split_once('.')
            .is_some_and(|(first, _)| first == label)
}

/// Levenshtein distance, counting a swap of neighbours as one edit.
pub(super) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    rows[0] = (0..=b.len()).collect();
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}
