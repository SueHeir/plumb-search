//! The `official_site` tool: the official site for a name, with how sure
//! Plumb is and why.

use super::*;

/// Other candidates `official_site` lists.
const ALTERNATIVES: usize = 3;

/// How much more read ([`Hit::demand`], a log scale) the article about one
/// owner of a shared name must be than any other owner's for `official_site`
/// to take it as the one meant: about ten times the reads.
const DOMINANT_DEMAND: f32 = 0.12;

/// Most of a name's first words `official_site` looks up when nothing
/// establishes a site for the whole name.
const PREFIX_LOOKUPS: usize = 2;

impl Mcp {
    /// `official_site`: the site the name names, best first.
    pub fn official_site(&self, name: &str, options: &SearchOptions) -> Result<Value> {
        self.official_site_of(name, options, true)
    }

    /// [`Mcp::official_site`], falling back to the name's first words when
    /// `prefixes` and nothing establishes a site for the whole name.
    fn official_site_of(
        &self,
        name: &str,
        options: &SearchOptions,
        prefixes: bool,
    ) -> Result<Value> {
        let mut intent = identity::Intent::of(name);
        let asked = options;
        let options = &SearchOptions {
            country: intent
                .country
                .map(str::to_string)
                .or_else(|| asked.country.clone()),
            ..asked.clone()
        };
        let mut results = self.lookup_name(name, 1 + ALTERNATIVES, options)?;
        // A country's name can end a place's own: New Mexico is not "new"
        // in Mexico, nor New South Wales "new south" in Wales.
        let whole = identity::Intent::whole(name);
        let options = if intent.country.is_some() && whole.is_known_name(&results) {
            intent = whole;
            results = self.lookup_name(name, 1 + ALTERNATIVES, asked)?;
            asked
        } else {
            options
        };
        let country_conflict = intent.country.filter(|country| {
            asked.country.as_deref().is_some_and(|preference| preference != *country)
        }).map(|country| json!({
            "query": country,
            "preference": asked.country,
            "resolution": "The jurisdiction named in the query takes precedence over the country preference.",
        }));
        // Keep ranking within the requested entity. A popular spelling
        // suggestion or a task-word domain cannot validate another entity.
        if let Some(at) = results
            .hits
            .iter()
            .position(|hit| hit.official && intent.supports_site(hit))
            .or_else(|| {
                results
                    .hits
                    .iter()
                    .position(|hit| intent.supports_site(hit))
            })
            .or_else(|| results.hits.iter().position(|hit| intent.matches_site(hit)))
        {
            results.hits[..=at].rotate_right(1);
        }
        let did_you_mean = results.spelling.as_ref().map(|s| s.query.as_str());
        // "Pillow docs" is Pillow, wanting its docs.
        let bare = bare_name(name);
        let bare_or_name = intent.entity.as_str();
        let wants_docs = asks_for_docs(name);
        let words = intent.words.clone();
        let pages = place_pages(
            name,
            &results.hits,
            results.pages.iter().map(|p| p.hit.clone()).collect(),
        );
        // The Wikipedia article the name names, and the site Wikidata gives
        // as its item's: LifeWiki is conwaylife.com, not life-wiki.com.
        let about = answers::page_about(&results.hits, &pages)
            .filter(|page| is_article(page) && intent.matches_article(page));
        let about_site = about.and_then(|page| Some((page, website_domain(page)?)));
        let Some(top) = results.hits.first() else {
            if let Some((page, site)) = about_site {
                let (domain, url) = article_website(page, &site);
                return Ok(json!({
                    "name": name,
                    "found": true,
                    "status": "resolved",
                    "domain": domain,
                    "url": url,
                    "package_home": Value::Null,
                    "title": Value::Null,
                    "description": page.description.as_deref().map(short),
                    "confidence": "medium",
                    "why": [format!(
                        "Wikidata gives it as the official website of {}, the article this name \
                         names.",
                        page.title
                    )],
                    "alternatives": [],
                    "did_you_mean": Value::Null,
                    "country_conflict": country_conflict,
                }));
            }
            let mut why = vec!["Plumb knows no site by this name.".to_string()];
            let package_home = self
                .package_home(name, wants_docs, options)
                .or_else(|| self.package_home(bare.as_deref()?, wants_docs, options));
            if let Some((home, registry, docs, _)) = &package_home {
                let resource = if *docs { "documentation" } else { "home page" };
                why = vec![format!(
                    "The {registry} package of this name gives {home} as its {resource}."
                )];
            } else if let Some(fixed) = did_you_mean {
                why.push(format!("Did you mean {fixed:?}? Look that up instead."));
            }
            let destination = package_home
                .as_ref()
                // A cached docs URL alone does not establish affiliation
                // with the requested owner or technical resource.
                .filter(|(_, _, docs, _)| !docs)
                .and_then(|(home, _, _, _)| Some((home, registrable_domain(home)?)));
            if package_home.as_ref().is_some_and(|(_, _, docs, _)| *docs) {
                why.push("The cached package documentation has no matching indexed owner or destination evidence; treat it as an unverified hint.".to_string());
            }
            return Ok(json!({
                "name": name,
                "found": destination.is_some(),
                "status": if destination.is_some() { "resolved" } else { "unresolved" },
                "domain": destination.as_ref().map(|(_, domain)| domain),
                "url": destination.as_ref().map(|(home, _)| home),
                "confidence": if destination.is_some() { "medium" } else { "low" },
                "why": why,
                "did_you_mean": did_you_mean,
                "package_home": package_home.map(|(home, _, _, _)| home),
                "country_conflict": country_conflict,
            }));
        };
        let entity_pages: Vec<_> = pages
            .iter()
            .filter(|placed| intent.matches_article(&placed.hit.page))
            .cloned()
            .collect();
        let official = |hit: &Hit| {
            (hit.official && intent.supports_site(hit))
                || article_of(&hit.domain, &entity_pages).is_some()
        };
        let well_known = |hit: &Hit| hit.link_score >= WELL_KNOWN_LINK_SCORE;
        // A site named by the name that shows nothing of it is not the
        // site of the article the name names: "USNO" is not mo.gov, whatever
        // names it, when the article on the United States Naval Observatory
        // gives navy.mil.
        let top_shows = intent.matches_site(top);
        let about_site = about_site.or_else(|| named_article_site(&entity_pages));
        let mut why = Vec::new();
        let mut pick = top;
        let mut url = top.url.clone();
        let mut domain = top.domain.clone();
        let mut title = top.title.clone();
        let mut description = top.description.as_deref().map(short);
        let mut did_you_mean = did_you_mean;
        let mut package_home = None;
        let mut package_ambiguity = None;
        let mut verified_destination = false;
        // The destination is the website of the article the name names.
        let mut named_article = false;
        let mut confidence;
        match about_site {
            Some((page, site)) if site == top.domain => {
                why.push(format!(
                    "Wikidata gives it as the official website of {}.",
                    page.title
                ));
                named_article = true;
                // MathWorld is mathworld.wolfram.com, not wolfram.com.
                let (site_domain, site_url) = article_website(page, &site);
                if site_domain != top.domain {
                    domain = site_domain;
                    url = site_url;
                    title = None;
                    description = page.description.as_deref().map(short);
                } else if page.website.is_some() {
                    url = site_url;
                }
                confidence = "high";
                verified_destination = true;
            }
            // The article's own site, unless the first site has owner
            // evidence for exactly this entity. Popularity alone cannot
            // outweigh the article's explicit owner reference.
            Some((page, site)) if !(top.named && official(top) && intent.supports_site(top)) => {
                why.push(format!(
                    "Wikidata gives it as the official website of {}, the article this name \
                     names.",
                    page.title
                ));
                let (site_domain, site_url) = article_website(page, &site);
                match results.hits.iter().find(|hit| hit.domain == site) {
                    Some(hit) => {
                        pick = hit;
                        title = hit.title.clone();
                        description = hit.description.as_deref().map(short);
                        url = if site_domain == hit.domain && page.website.is_none() {
                            hit.url.clone()
                        } else {
                            site_url
                        };
                    }
                    None => {
                        title = None;
                        description = page.description.as_deref().map(short);
                        url = site_url;
                    }
                }
                domain = site_domain;
                did_you_mean = None;
                confidence = "high";
                verified_destination = true;
                named_article = true;
            }
            _ => {
                if official(top) {
                    why.push(match article_of(&top.domain, &entity_pages) {
                        Some(page) => format!(
                            "Wikidata gives it as the official website of {}.",
                            page.title
                        ),
                        None => "Wikidata lists it as an official website.".to_string(),
                    });
                }
                if top.named && top_shows {
                    why.push("The name is the site's own name or address.".to_string());
                }
                if well_known(top) && top_shows {
                    why.push("It is a well-known site, linked from many others.".to_string());
                }
                // How far ahead of the next site it is, as a share of its score.
                let lead = results.hits.get(1).map_or(1.0, |next| {
                    (top.score - next.score) / top.score.max(f32::EPSILON)
                });
                // Official or well known says whose site it is, not that it is
                // this name's: chess.com is not the Chess Programming Wiki. And
                // a name of several words whose site shows only some of them
                // is a guess: nssdc.ac.cn for "NSSDC planetary fact sheet".
                let shows = shows_name(top, &words);
                // Named by a name it shows nothing of is no sure thing
                // (lifewire.com for "LifeWiki"), nor is one of two official,
                // well-known sites of the name ("Elixir").
                let rival = results.hits[1..].iter().any(|hit| {
                    // A metadata-supported owner remains a namesake even
                    // when it is less popular or retrieval missed its alias.
                    official(hit) && intent.supports_site(hit)
                });
                confidence = if top.named
                    && (official(top) || well_known(top))
                    && lead >= 0.1
                    && top_shows
                    && !rival
                {
                    "high"
                } else if top_shows
                    && ((top.named && (words.len() < 2 || shows))
                        || title_has(top, bare_or_name)
                        || (official(top) && shows)
                        || (well_known(top) && lead >= 0.2 && shows))
                {
                    "medium"
                } else {
                    "low"
                };
                // An article named by the name that gives no website, and a
                // site of another name: Golly is not gollo.com.
                if let Some(page) = about {
                    if confidence != "high" && !official(top) && !title_has(top, bare_or_name) {
                        why.push(format!(
                            "Wikipedia's article {} gives no official website, and nothing ties \
                             this site to it.",
                            page.title
                        ));
                        confidence = "low";
                    }
                }
                // Another site's title is the name: "Main Page - Chess
                // Programming Wiki" for "Chess Programming Wiki".
                if !top.named && !title_has(top, bare_or_name) {
                    if let Some(hit) = results.hits[1..]
                        .iter()
                        .find(|hit| intent.matches_site(hit) && title_has(hit, bare_or_name))
                    {
                        why = vec![format!(
                            "Its title has the name; {}'s does not.",
                            top.domain
                        )];
                        pick = hit;
                        url = hit.url.clone();
                        domain = hit.domain.clone();
                        title = hit.title.clone();
                        description = hit.description.as_deref().map(short);
                        confidence = "medium";
                    }
                }
                // A guess that shows only some of the name gives way to an
                // official, well-known site that shows all of it: "Outlook
                // email" is outlook.live.com, not office.com.
                if confidence == "low" && !shows && words.iter().all(|word| word.len() >= 3) {
                    if let Some(hit) = results.hits[1..].iter().find(|hit| {
                        official(hit)
                            && well_known(hit)
                            && intent.matches_site(hit)
                            && shows_name(hit, &words)
                    }) {
                        why = vec![format!(
                            "It shows every word of the name; {} does not.",
                            top.domain
                        )];
                        pick = hit;
                        url = hit.url.clone();
                        domain = hit.domain.clone();
                        title = hit.title.clone();
                        description = hit.description.as_deref().map(short);
                        confidence = "medium";
                    }
                }
                // A guess gives way to a site whose address is a word of
                // the name: "cube20 God's number" is cube20.org, not
                // rubiks.com. One that shows the whole name keeps it: "old navy"
                // is not navy.mil.
                let label_word = |hit: &Hit| {
                    let label = letters(&domain_label(&hit.domain));
                    label.len() >= 4
                        && words.first() == Some(&label)
                        && intent.matches_country(&hit.domain, hit.country.as_deref())
                        && (intent.country.is_none() || intent.matches_site(hit))
                };
                if confidence == "low" && !shows && !label_word(top) {
                    if let Some(hit) = results.hits[1..].iter().find(|hit| label_word(hit)) {
                        why = vec![format!(
                            "Its address is {}, a word of the name.",
                            hit.domain
                        )];
                        pick = hit;
                        url = hit.url.clone();
                        domain = hit.domain.clone();
                        title = hit.title.clone();
                        description = hit.description.as_deref().map(short);
                        // A distinctive label is still only a suggestion
                        // without evidence for the complete requested entity.
                    }
                }
            }
        }
        let rivals: Vec<&str> = results
            .hits
            .iter()
            .filter(|hit| hit.domain != pick.domain && (hit.named || hit.official))
            .map(|hit| hit.domain.as_str())
            .collect();
        if !rivals.is_empty() && confidence != "high" {
            why.push(format!("Other search candidates: {}.", rivals.join(", ")));
        }
        let mut alternatives: Vec<Value> = results
            .hits
            .iter()
            .filter(|hit| hit.domain != domain)
            .filter(|hit| hit.named || official(hit) || mentions_name(hit, &words))
            .map(|hit| brief_with(hit, &pages))
            .collect();
        if confidence != "high" || wants_docs || (!verified_destination && !official(pick)) {
            // A software package of the name says where its home is:
            // FastAPI's PyPI card names fastapi.tiangolo.com.
            let found = self
                .package_home(name, wants_docs, options)
                .or_else(|| self.package_home(bare.as_deref()?, wants_docs, options));
            if let Some((home, registry, docs, owner)) = found {
                let home_domain = registrable_domain(&home);
                // Package metadata can connect a project's indexed homepage
                // to its docs, but a same-name cached docs link cannot supply
                // missing evidence for the requested owner or qualifiers.
                let docs_supported = !docs
                    || (verified_destination && owner.as_deref() == Some(domain.as_str()))
                    || results.hits.iter().any(|hit| {
                        intent.supports_site(hit)
                            && (owner.as_deref() == Some(hit.domain.as_str())
                                || (official(hit)
                                    && home_domain.as_deref() == Some(hit.domain.as_str())))
                    });
                // Rejecting an inferred owner label does not establish that
                // a same-name package is the requested entity. Keep a full
                // matching official candidate as unresolved evidence, even
                // when its title does not spell or expand the query's name.
                let conflict = results.hits.iter().find(|hit| {
                    !verified_destination
                        && hit.official
                        && intent.matches_site(hit)
                        && !intent.supports_site(hit)
                        && registrable_domain(&hit.url) == registrable_domain(&hit.domain)
                        && home_domain.as_deref() != Some(hit.domain.as_str())
                });
                if let Some(conflict) = conflict {
                    alternatives.retain(|alt| {
                        alt["domain"] != conflict.domain
                            && alt["domain"].as_str() != home_domain.as_deref()
                    });
                    alternatives.insert(
                        0,
                        json!({
                            "domain": home_domain,
                            "url": home,
                            "title": format!("{registry} package {bare_or_name}"),
                            "official": false,
                            "well_known": false,
                        }),
                    );
                    alternatives.insert(0, brief_with(conflict, &pages));
                    alternatives.truncate(ALTERNATIVES);
                    package_ambiguity = Some(format!(
                        "Both {} and the {registry} package's {home} are candidates; \
                         the package name does not resolve which entity was requested.",
                        conflict.url
                    ));
                    package_home = Some(home);
                } else if !docs_supported {
                    alternatives.retain(|alt| alt["url"] != home);
                    // Preserve indexed owner rivals ahead of an unverified
                    // metadata hint when the alternatives list is capped.
                    alternatives.push(json!({
                        "domain": home_domain,
                        "url": home,
                        "title": format!("{registry} package {bare_or_name} documentation"),
                        "official": false,
                        "well_known": false,
                    }));
                    alternatives.truncate(ALTERNATIVES);
                    why.push(format!(
                        "Cached {registry} metadata lists {home} as documentation, but indexed \
                         evidence does not bind that host to the complete requested identity."
                    ));
                    package_home = Some(home);
                } else if home_domain.as_deref() == Some(domain.as_str()) {
                    why.push(format!(
                        "The {registry} package of this name gives it as its home page."
                    ));
                    url = home;
                    confidence = "high";
                    verified_destination = true;
                    did_you_mean = None;
                } else if let Some(home_domain) = home_domain.clone().filter(|_| {
                    wants_docs
                        && docs
                        && (!well_known(pick)
                            || !intent.matches_site(pick)
                            || owner.as_deref() == Some(pick.domain.as_str()))
                }) {
                    // "Pillow docs" is the docs the package names,
                    // pillow.readthedocs.io, not python-pillow.org; a
                    // A matching well-known site keeps its own unless the
                    // package's homepage also ties its docs to that site:
                    // "Anthropic API docs" is not an SDK's unrelated docs.
                    why = vec![format!(
                        "The {registry} package of this name gives it as its documentation."
                    )];
                    alternatives.retain(|alt| alt["domain"] != home_domain.as_str());
                    alternatives.insert(0, brief_with(pick, &pages));
                    alternatives.truncate(ALTERNATIVES);
                    url = home;
                    domain = home_domain;
                    title = None;
                    description = None;
                    confidence = "medium";
                    verified_destination = true;
                    did_you_mean = None;
                } else if let Some(home_domain) =
                    home_domain.filter(|_| !docs || wants_docs).filter(|_| {
                        !verified_destination
                            && (!intent.supports_site(pick)
                                || (!official(pick) && !well_known(pick)))
                    })
                {
                    // Docs alone are no home: PyPI gives pandas.readthedocs.io
                    // for pandas, whose site is pandas.pydata.org.
                    // The best match of the words is a guess; the
                    // package's own home page is not: "FastAPI" is not
                    // xapo.com.
                    why = vec![format!(
                        "No site is called exactly this, but the {registry} package of this \
                         name gives it as its home page."
                    )];
                    alternatives.insert(0, brief_with(pick, &pages));
                    alternatives.truncate(ALTERNATIVES);
                    url = home;
                    domain = home_domain;
                    title = None;
                    description = None;
                    confidence = "medium";
                    verified_destination = true;
                    did_you_mean = None;
                } else {
                    why.push(format!(
                        "The {registry} package of this name gives {home} as its home page."
                    ));
                    package_home = Some(home);
                }
            }
        }
        if confidence == "low" {
            // A name that spells out its abbreviation: "CIAAW Commission on
            // Isotopic Abundances and Atomic Weights" is ciaaw.org.
            if let Some(hit) = self.abbreviation_site(name, options) {
                // An acronym lookup may recover an owner already described
                // by Wikidata, but cannot discard the rest of the query.
                if !hit.official || !intent.supports_site(&hit) {
                    if !alternatives.iter().any(|alt| alt["domain"] == hit.domain) {
                        alternatives.insert(0, brief_with(&hit, &pages));
                        alternatives.truncate(ALTERNATIVES);
                    }
                } else {
                    why = vec![format!(
                        "Its address is {}, an abbreviation in the name.",
                        hit.domain
                    )];
                    alternatives.retain(|alt| alt["domain"] != hit.domain);
                    if pick.domain != hit.domain {
                        alternatives.insert(0, brief_with(pick, &pages));
                        alternatives.truncate(ALTERNATIVES);
                    }
                    url = hit.url.clone();
                    domain = hit.domain.clone();
                    title = hit.title.clone();
                    description = hit.description.as_deref().map(short);
                    confidence = "medium";
                    verified_destination = true;
                    did_you_mean = None;
                }
            }
        }
        // "Python docs" is docs.python.org, a site of its own.
        let owners: Vec<&Hit> = results
            .hits
            .iter()
            .filter(|hit| official(hit) && intent.supports_site(hit))
            .collect();
        let owner_domains: std::collections::BTreeSet<_> = owners
            .iter()
            .filter_map(|hit| registrable_domain(&hit.url))
            .collect();
        // One documented owner can still be the one a shared name means:
        // the one whose article is read far more than any other owner's
        // (Chase the bank, not the town of Chase).
        let dominant = registrable_domain(&pick.url).is_some_and(|picked| {
            owner_domains.contains(&picked)
                && pick.demand.is_some_and(|read| {
                    owners.iter().all(|owner| {
                        registrable_domain(&owner.url).as_ref() == Some(&picked)
                            || owner.demand.unwrap_or(0.0) + DOMINANT_DEMAND <= read
                    })
                })
        });
        // An explicit address identifies the destination, and so does the
        // website of the article the name names: Wikipedia gives
        // "Microsoft" to the company, not to the .microsoft registry.
        // Otherwise an unqualified shared name does not identify one of
        // several documented owners, even if a same-name package or a
        // site's links favor one candidate.
        let shared = registrable_domain(&intent.entity).is_none()
            && owner_domains.len() > 1
            && !named_article;
        let owner_ambiguity = shared && !dominant;
        if shared && dominant {
            if confidence == "high" {
                confidence = "medium";
            }
            why.push(format!(
                "Other documented owners share the name ({}); the article about this one is \
                 read far more.",
                owner_domains
                    .iter()
                    .filter(|owner| registrable_domain(&pick.url).as_ref() != Some(*owner))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let ambiguous = owner_ambiguity || package_ambiguity.is_some();
        let bound = (verified_destination || intent.supports_site(pick)) && !ambiguous;
        if ambiguous {
            alternatives.retain(|alt| alt["url"] != url);
            alternatives.insert(
                0,
                json!({
                    "domain": domain,
                    "url": url,
                    "title": title,
                    "official": official(pick),
                    "well_known": well_known(pick),
                }),
            );
            alternatives.truncate(ALTERNATIVES);
        }
        if bound && !verified_destination && !official(pick) {
            // Popularity and agreeing title/domain words are useful
            // suggestions, but do not verify ownership independently.
            confidence = "low";
            why.push("The site's title or descriptive text agrees with its domain name; its ownership is not independently verified.".to_string());
        }
        if bound && wants_docs && url.contains(&format!("{domain}/")) {
            if let Some((site, _)) = plumb_core::subdomain_sites().find(|(site, parent)| {
                *parent == domain && (site.starts_with("docs.") || site.starts_with("doc."))
            }) {
                why.push(format!("Its documentation is {site}, a site of its own."));
                domain = site.to_string();
                url = format!("https://{site}/");
                title = None;
                description = None;
            }
        }
        if !bound {
            alternatives.retain(|alt| alt["domain"] != pick.domain);
            alternatives.insert(0, brief_with(pick, &pages));
            alternatives.truncate(ALTERNATIVES);
            why = vec![format!(
                "No owner evidence establishes the suggested site as the destination for {:?}; \
                 search names, titles, partial domain matches and spelling corrections alone \
                 do not establish its official destination.",
                intent.entity
            )];
            if let Some(page) = about {
                why.push(format!(
                    "Wikipedia's article {} gives no matching official destination.",
                    page.title
                ));
            }
            confidence = "low";
        } else if confidence == "low" {
            why.push("Treat this as an unverified candidate for the requested name.".to_string());
        }
        if owner_ambiguity {
            why.push(format!(
                "Several documented owners match the requested name: {}. Specify the entity or its purpose before choosing an official destination.",
                owner_domains.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(ambiguity) = package_ambiguity {
            why.push(ambiguity);
        }
        // A site that shows the whole name may be its own thing (the Chess
        // Programming Wiki is not chess.com); otherwise the name may be an
        // owner and what is wanted of it: "Home Depot coupons".
        if prefixes
            && !bound
            && !ambiguous
            && !results.hits.iter().any(|hit| intent.matches_site(hit))
        {
            if let Some(mut answer) =
                self.prefix_site(name, &intent, &results.hits, &pages, wants_docs, options)?
            {
                answer["country_conflict"] = json!(country_conflict);
                return Ok(answer);
            }
        }
        Ok(json!({
            "name": name,
            "found": bound,
            "status": if ambiguous { "ambiguous" } else if bound { "resolved" } else { "unresolved" },
            "domain": bound.then_some(domain),
            "url": bound.then_some(url),
            "package_home": package_home,
            "title": bound.then_some(title).flatten(),
            "description": bound.then_some(description).flatten(),
            "confidence": confidence,
            "why": why,
            "alternatives": alternatives,
            "did_you_mean": did_you_mean,
            "country_conflict": country_conflict,
        }))
    }

    /// The official site of the longest of `name`'s first words that surely
    /// has one, for a name nothing establishes a site for: "Home Depot
    /// coupons" is homedepot.com, the site of the Home Depot. Only first
    /// words a result shows are looked up, at most [`PREFIX_LOOKUPS`] of
    /// them, and the answer stays low confidence: the rest of the name may
    /// be a page or service of the site, or another thing.
    fn prefix_site(
        &self,
        name: &str,
        intent: &identity::Intent,
        hits: &[Hit],
        pages: &[PlacedPage],
        wants_docs: bool,
        options: &SearchOptions,
    ) -> Result<Option<Value>> {
        let words: Vec<&str> = intent.entity.split_whitespace().collect();
        // The name's own spelling of the words, when it starts with them.
        let asked: Vec<&str> = name.split_whitespace().collect();
        let spelled = |end: usize| {
            if asked.len() >= end
                && asked[..end]
                    .iter()
                    .zip(&words[..end])
                    .all(|(asked, word)| asked.to_lowercase() == **word)
            {
                asked[..end].join(" ")
            } else {
                words[..end].join(" ")
            }
        };
        let mut looked = 0;
        for end in (1..words.len()).rev() {
            let last = plumb_core::normalize_text(words[end - 1]);
            if last.trim().is_empty() || plumb_core::is_function_word(last.trim()) {
                continue;
            }
            let prefix = spelled(end);
            let part = identity::Intent::of(&prefix);
            if !hits.iter().any(|hit| part.matches_site(hit))
                && !pages
                    .iter()
                    .any(|placed| part.matches_article(&placed.hit.page))
            {
                continue;
            }
            if looked == PREFIX_LOOKUPS {
                break;
            }
            looked += 1;
            let lookup = if wants_docs {
                format!("{prefix} docs")
            } else {
                prefix.clone()
            };
            let mut answer = self.official_site_of(&lookup, options, false)?;
            if answer["found"] != true || answer["confidence"] != "high" {
                continue;
            }
            // The site must say something of the rest of the name, or the
            // rest may name another thing: the tz database is no newspaper.
            let rest = words[end..].join(" ");
            let domain = answer["domain"].as_str().unwrap_or_default();
            let mut texts: Vec<&str> = ["title", "description"]
                .iter()
                .filter_map(|field| answer[*field].as_str())
                .collect();
            for hit in hits.iter().filter(|hit| hit.domain == domain) {
                texts.extend(hit.title.as_deref());
                texts.extend(hit.description.as_deref());
                texts.extend(hit.key_pages.iter().map(|page| page.label.as_str()));
            }
            if !mentions_any(&texts, &rest) {
                continue;
            }
            let mut why = vec![json!(format!(
                "No site shows the whole name. This is the official site of {prefix:?}, its \
                 first {}; {rest:?} may be a page or service of it, or something else.",
                if end == 1 { "word" } else { "words" }
            ))];
            why.extend(answer["why"].as_array().cloned().unwrap_or_default());
            answer["name"] = json!(name);
            answer["confidence"] = json!("low");
            answer["why"] = Value::Array(why);
            return Ok(Some(answer));
        }
        Ok(None)
    }

    /// The site whose address is an abbreviation in `name` of two words or
    /// more ("NPS API developer" -> nps.gov), when a search for it finds the
    /// site named by it.
    fn abbreviation_site(&self, name: &str, options: &SearchOptions) -> Option<Hit> {
        let words: Vec<&str> = name.split_whitespace().collect();
        if words.len() < 2 {
            return None;
        }
        let abbreviation = words.iter().find(|word| {
            (2..=8).contains(&word.len())
                && word.bytes().all(|b| b.is_ascii_uppercase())
                && !WANTED_WORDS.contains(&word.to_ascii_lowercase().as_str())
        })?;
        let found = self.lookup_name(abbreviation, 1, options).ok()?;
        let top = found.hits.into_iter().next()?;
        (top.named && letters(&domain_label(&top.domain)) == abbreviation.to_ascii_lowercase())
            .then_some(top)
    }

    /// The home page (else the docs; the docs first when `wants_docs`) of
    /// the most used package called `name`, its registry's name, and whether
    /// it is the docs, and the package's homepage domain. The latter binds
    /// a docs link to a matching well-known project homepage.
    fn package_home(
        &self,
        name: &str,
        wants_docs: bool,
        options: &SearchOptions,
    ) -> Option<(String, String, bool, Option<String>)> {
        let found = self.package(name, None, options).ok()?;
        let card = found["packages"].as_array()?.iter().find(|card| {
            card["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name.trim()))
        })?;
        let (first, then) = if wants_docs {
            ("docs", "homepage")
        } else {
            ("homepage", "docs")
        };
        // A registry's own pages (docs.rs/toml) are no project's home.
        let (key, home) = [first, then]
            .iter()
            .filter_map(|key| Some((*key, card[*key].as_str()?)))
            .find(|(_, home)| {
                registrable_domain(home)
                    .is_none_or(|domain| !REGISTRY_HOSTS.contains(&domain.as_str()))
            })?;
        Some((
            home.to_string(),
            card["registry"].as_str()?.to_string(),
            key == "docs",
            card["homepage"].as_str().and_then(registrable_domain),
        ))
    }
}

/// Whether `page` is about one thing Wikidata describes: a Wikipedia
/// article or a Wikidata item.
pub(super) fn is_article(page: &Page) -> bool {
    page.set.starts_with("wikipedia-") || page.set == WIKIDATA_SET
}

/// The article among `pages` whose item Wikidata gives `domain` as the
/// official website of.
pub(super) fn article_of<'a>(domain: &str, pages: &'a [PlacedPage]) -> Option<&'a Page> {
    pages
        .iter()
        .map(|placed| &placed.hit.page)
        .find(|page| is_article(page) && page.site.as_deref() == Some(domain))
}

/// The site and address of the official website of `page`'s item, whose
/// registrable domain is `site`: its subdomain when the website is on one
/// (`mathworld.wolfram.com`), else `site`.
fn article_website(page: &Page, site: &str) -> (String, String) {
    let Some(website) = page.website.as_deref() else {
        return (site.to_string(), format!("https://{site}/"));
    };
    let host = host_of(website).unwrap_or_else(|| site.to_string());
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let domain = if host.ends_with(&format!(".{site}")) {
        host.to_string()
    } else {
        site.to_string()
    };
    (domain, website.to_string())
}

/// Hosts of package registries and the docs they build, which a package's
/// card may give as its home or docs but which are no project's own site.
const REGISTRY_HOSTS: &[&str] = &[
    "docs.rs",
    "crates.io",
    "npmjs.com",
    "npmjs.org",
    "pypi.org",
    "pkg.go.dev",
    "go.dev",
    "rubygems.org",
    "rubydoc.info",
    "nuget.org",
    "packagist.org",
    "hex.pm",
    "hexdocs.pm",
];

/// The site Wikidata gives the best article among `pages` the name names,
/// with the article.
fn named_article_site(pages: &[PlacedPage]) -> Option<(&Page, String)> {
    pages
        .iter()
        .filter(|placed| placed.hit.named && is_article(&placed.hit.page))
        .filter(|placed| !placed.hit.page.title.ends_with("(disambiguation)"))
        .max_by(|a, b| a.hit.score.total_cmp(&b.hit.score))
        .and_then(|placed| Some((&placed.hit.page, website_domain(&placed.hit.page)?)))
}

/// The registrable domain of the official website Wikidata gives `page`'s
/// item: its site, or for a person whose website is a page of a bigger
/// site, which [`Page::site`] leaves out as not theirs, that site
/// (senate.gov for sanders.senate.gov).
pub(super) fn website_domain(page: &Page) -> Option<String> {
    page.site
        .clone()
        .or_else(|| registrable_domain(page.website.as_deref()?))
}

/// Documentation navigation is separate from identity. Technical resource
/// words such as API, developer and specification still constrain the owner.
const WANTED_WORDS: &[&str] = &["docs", "documentation"];

/// The name in `asked` without what is wanted from its site after it:
/// "Pillow docs" -> "pillow", `None` when nothing is wanted.
pub(super) fn bare_name(asked: &str) -> Option<String> {
    let whole = asked
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut name = whole.clone();
    loop {
        let words: Vec<&str> = name.split_whitespace().collect();
        if words.len() < 2 {
            break;
        }
        if WANTED_WORDS.contains(&words[words.len() - 1]) {
            name = words[..words.len() - 1].join(" ");
        } else if let Some(shorter) = without_intent_words(&name) {
            name = shorter;
        } else {
            break;
        }
    }
    (name != whole && !name.is_empty()).then_some(name)
}

/// Whether `asked` wants a site's documentation: "Pillow docs".
fn asks_for_docs(asked: &str) -> bool {
    asked
        .split_whitespace()
        .any(|word| matches!(word.to_lowercase().as_str(), "docs" | "documentation"))
}

/// Whether any of `texts` has a word of `words` other than a function
/// word or one of two letters, as written or without a plural's "s".
fn mentions_any(texts: &[&str], words: &str) -> bool {
    let stem = |word: &str| word.strip_suffix('s').unwrap_or(word).to_string();
    let shown: std::collections::HashSet<String> = texts
        .iter()
        .flat_map(|text| {
            plumb_core::normalize_text(text)
                .split_whitespace()
                .map(stem)
                .collect::<Vec<_>>()
        })
        .collect();
    plumb_core::normalize_text(words)
        .split_whitespace()
        .filter(|word| word.chars().count() > 2 && !plumb_core::is_function_word(word))
        .any(|word| shown.contains(&stem(word)))
}

/// What a site shows of itself: its address, title and description, as
/// [`letters`].
fn site_letters(hit: &Hit) -> String {
    letters(&format!(
        "{} {} {}",
        hit.domain,
        hit.title.as_deref().unwrap_or(""),
        hit.description.as_deref().unwrap_or("")
    ))
}

/// Whether `hit` shows every one of the name's `words`.
fn shows_name(hit: &Hit, words: &[String]) -> bool {
    let shown = site_letters(hit);
    !words.is_empty() && words.iter().all(|word| shown.contains(word.as_str()))
}

/// Whether `hit` shows one of the name's `words` at least: an alternative
/// with none of them is noise (sleepnumber.com for "Pillow").
fn mentions_name(hit: &Hit, words: &[String]) -> bool {
    let shown = site_letters(hit);
    words.iter().any(|word| {
        word.len() >= 3 && !FILLER_WORDS.contains(&word.as_str()) && shown.contains(word.as_str())
    })
}

/// Least letters of a name a title must have whole for [`title_has`]:
/// shorter names are in too many titles.
const MIN_TITLE_NAME_LETTERS: usize = 6;

/// Whether `hit`'s title has the whole of `name` in it.
fn title_has(hit: &Hit, name: &str) -> bool {
    let name = letters(name);
    name.len() >= MIN_TITLE_NAME_LETTERS
        && hit
            .title
            .as_deref()
            .is_some_and(|title| letters(title).contains(&name))
}
