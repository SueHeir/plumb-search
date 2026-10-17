//! The `site_info` and `facts` tools: what Plumb knows about one site, and
//! what Wikidata says about the thing a name names.

use super::*;

impl Mcp {
    /// `site_info`: one site's entry. A subdomain that is not a site of its
    /// own (spec.commonmark.org) or a page of the site (nps.gov/yose/) is
    /// said to be part of the site, and read now when the node reads pages,
    /// as is the front page of a site the index has no title or description
    /// of.
    pub fn site_info(&self, input: &str, options: &SearchOptions) -> Result<Value> {
        let Some(domain) = registrable_domain(input) else {
            bail!("{input:?} is not a web address or domain name");
        };
        let host = host_of(input).unwrap_or_else(|| domain.clone());
        let bare_host = host
            .strip_prefix("www.")
            .or_else(|| host.strip_prefix("m."))
            .unwrap_or(&host);
        let folded = bare_host != domain;
        let asked_page = asked_page(input);
        let results = self.lookup_name(&domain, 3, options)?;
        let hit = results.hits.iter().find(|hit| hit.domain == domain);
        let mut answer = match hit {
            None => json!({ "domain": domain, "found": false }),
            Some(hit) => {
                let pages: Vec<Value> = results
                    .pages
                    .iter()
                    .filter(|placed| placed.under.as_deref() == Some(domain.as_str()))
                    .map(|placed| {
                        json!({ "title": placed.hit.page.title, "url": placed.hit.page.url })
                    })
                    .collect();
                // The index says official when it has Wikidata's description of
                // the site; an article giving it as its item's website says so
                // too (nps.gov, the National Park Service's).
                let official_for = article_of(&domain, &results.pages);
                json!({
                    "domain": domain,
                    "found": true,
                    "url": hit.url,
                    "title": hit.title,
                    "description": hit.description.as_deref().map(short),
                    "official": hit.official || official_for.is_some(),
                    "official_for": official_for.map(|page| page.title.clone()),
                    "well_known": hit.link_score >= WELL_KNOWN_LINK_SCORE,
                    "popularity": round(hit.link_score),
                    "country": hit.country,
                    "searchable": search_template_for(&domain).is_some(),
                    "pages": pages,
                })
            }
        };
        let fields = answer.as_object_mut().expect("an object");
        if folded {
            fields.insert("host".into(), json!(bare_host));
            fields.insert("part_of".into(), json!(domain));
        }
        let untold = hit.is_none_or(|hit| hit.title.is_none() && hit.description.is_none());
        let to_read = match (&asked_page, folded) {
            (Some(page), _) => Some(page.clone()),
            (None, true) => Some(format!("https://{host}/")),
            (None, false) if untold => Some(format!("https://{domain}/")),
            (None, false) => None,
        };
        if let (Some(reader), Some(address)) = (&self.reader, to_read) {
            match reader.front(&address) {
                Ok(read) => {
                    fields.insert("read_now".into(), read);
                }
                Err(err) => {
                    fields.insert(
                        "read_error".into(),
                        json!(format!("{address} could not be read: {err:#}")),
                    );
                }
            }
        }
        Ok(answer)
    }

    /// `facts`: what Wikidata says about the thing `subject` names, each
    /// fact with the item and property it comes from, so a model can cite
    /// it. With `about` ("ceo", "population"), only the facts of that kind.
    pub fn facts(
        &self,
        subject: &str,
        about: Option<&str>,
        options: &SearchOptions,
    ) -> Result<Value> {
        use plumb_core::facts::{fact_asked, FactKind, KINDS};
        // The kinds asked for, and whether only the first of them a page
        // has is wanted.
        let (kinds, first_only): (Option<Vec<FactKind>>, bool) = match about {
            None => (None, false),
            Some(about) => {
                let key = about.to_lowercase().replace([' ', '_'], "-");
                let asked = |question: String| fact_asked(&question).map(|q| q.kinds);
                let kinds = match FactKind::from_key(&key) {
                    // Then the kinds the question asks for after it:
                    // "height of mount everest" is its elevation, as a
                    // mountain has no height.
                    Some(kind) => Some((
                        asked(format!("{about} of {subject}"))
                            .filter(|kinds| kinds.first() == Some(&kind))
                            .unwrap_or_else(|| vec![kind]),
                        true,
                    )),
                    None => asked(format!("{about} of {subject}"))
                        .or_else(|| asked(format!("{subject} {about}")))
                        .map(|kinds| (kinds, false)),
                };
                let Some((kinds, first_only)) = kinds else {
                    return Ok(json!({
                        "subject": subject, "found": false, "status": "unsupported_property",
                        "about": about, "supported": KINDS.iter().map(|k| k.key()).collect::<Vec<_>>(),
                    }));
                };
                (Some(kinds), first_only)
            }
        };
        let found = self.backend.entities(subject, 20, options)?;
        let page = match answers::resolve_entity(subject, &found, kinds.as_deref()) {
            answers::EntityResolution::Resolved(page) => page,
            answers::EntityResolution::Unresolved => {
                return Ok(json!({
                    "subject": subject, "found": false, "status": "unresolved_entity",
                }))
            }
            answers::EntityResolution::Ambiguous(candidates) => {
                return Ok(json!({
                    "subject": subject, "found": false, "status": "ambiguous_entity",
                    "candidates": candidates.iter().take(5).map(|page| json!({
                        "item": page.item, "title": page.title,
                        "description": page.description.as_deref().map(short), "url": page.url,
                    })).collect::<Vec<_>>(),
                }))
            }
        };
        let kinds = kinds.map(|kinds| {
            match kinds
                .iter()
                .find(|kind| page.facts.iter().any(|fact| fact.kind == **kind))
            {
                Some(&first) if first_only => vec![first],
                _ => kinds,
            }
        });
        let wanted = |kind: &FactKind| kinds.as_ref().is_none_or(|kinds| kinds.contains(kind));
        let item_url = page
            .item
            .as_deref()
            .map(|item| format!("https://www.wikidata.org/wiki/{item}"));
        let mut facts = Vec::new();
        for kind in KINDS.iter().copied().filter(wanted) {
            let values: Vec<&str> = page
                .facts
                .iter()
                .filter(|fact| fact.kind == kind)
                .map(|fact| fact.value.as_str())
                .collect();
            let Some((value, note)) = answers::fact_text(kind, &values) else {
                continue;
            };
            facts.push(json!({
                "kind": kind.key(),
                "question": kind.question(&page.title),
                "value": value,
                "note": note,
                "property": kind.property(),
                "source": item_url.as_ref().map(|url| format!("{url}#{}", kind.property())),
                "raw_values": values,
                "unit": kind.unit(),
                "valid_from": Value::Null,
                "valid_to": Value::Null,
                "observation_year": page.facts.iter().find(|fact| fact.kind == kind).and_then(|fact| fact.observation_year()),
                "retrieved_at": Value::Null,
                "statement": Value::Null,
                "freshness": "unknown",

            }));
        }
        let missing: Vec<_> = kinds
            .as_ref()
            .into_iter()
            .flatten()
            .filter(|kind| !page.facts.iter().any(|fact| fact.kind == **kind))
            .map(|kind| kind.key())
            .collect();
        Ok(json!({
            "subject": subject,
            "found": !facts.is_empty(),
            "status": if facts.is_empty() { "missing_enrichment" } else { "ok" },
            "coverage": if facts.is_empty() || !missing.is_empty() { "partial" } else { "available" },
            "missing_properties": missing,
            "freshness": "unknown",
            "title": page.title,
            "description": page.description.as_deref().map(short),
            "url": page.url,
            "item": page.item,
            "item_url": item_url,
            "from": "Wikidata",
            "facts": facts,
        }))
    }
}

/// The page `input` asks about when it is a web address with a path,
/// query or fragment past the front page: `https://www.nps.gov/yose/`.
pub(super) fn asked_page(input: &str) -> Option<String> {
    let input = input.trim();
    let address = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url = url::Url::parse(&address).ok()?;
    let front = matches!(url.path(), "" | "/") && url.query().is_none();
    (!front).then(|| url.to_string())
}

fn round(score: f32) -> f64 {
    (f64::from(score) * 100.0).round() / 100.0
}
