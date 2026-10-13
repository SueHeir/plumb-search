//! The panel's settings: the settings form, page sets, features, credits
//! and plugins.

use super::view::card;
use crate::node::features::FeatureSettings;
use crate::node::{CrawlHours, NodeSettings, Workload};
use crate::pages::{thousands, PageSetSize, SETS, SIZE_CHOICES};
use crate::web::{escape_html, group_thousands};

pub(super) fn render_features(
    body: &mut String,
    active: &FeatureSettings,
    saved: &FeatureSettings,
    section: &str,
    base: &str,
    can_restart: bool,
) {
    let restart_hint = if can_restart {
        "Save your choices, then use Restart to apply. Search pauses briefly while the node restarts."
    } else {
        "Save your choices, then quit and reopen the app or restart the Docker container. Closing the desktop window does not quit the app."
    };
    body.push_str(&format!("<h2>Optional features</h2><p class=\"hint\">{restart_hint}</p><form method=\"post\" action=\"{base}/features\">"));
    body.push_str(&format!(
        "<input type=\"hidden\" name=\"section\" value=\"{section}\">"
    ));
    for (name, label, value, running, hint) in [
        ("network", "Join the Plumb network", saved.network, active.network, "Share crawls and search other nodes. The connection details above show the addresses this node uses."),
        ("search_by_meaning", "Search by meaning", saved.search_by_meaning, active.search_by_meaning, "Find sites by topic. Downloads a model (about 130 MB) and builds site vectors in the background."),
        ("share_popularity", "Share anonymous popularity", saved.share_popularity, active.share_popularity, "Requires the Plumb network. Reports which results are opened to help improve ranking. Off unless you enable it."),
        ("search_history", "Remember searches", saved.search_history.unwrap_or(active.search_history == Some(true)), active.search_history == Some(true), "Each browser that searches here keeps its own history on this computer: past searches, and the sites opened from them, which come first next time. Nobody sees another browser's history. Turn off on a node strangers can search."),
    ] {
        if (section == "search") != matches!(name, "search_by_meaning" | "search_history") { continue; }
        body.push_str(&format!("<div class=\"feature\"><label><input type=\"checkbox\" name=\"{name}\" value=\"1\"{}><span>{label} <span class=\"state\">· currently {}</span></span></label><p class=\"hint\">{hint}</p></div>", if value { " checked" } else { "" }, if running { "on" } else { "off" }));
    }
    if section == "network" {
        let extra: Vec<&str> = saved.extra_bootstrap().collect();
        body.push_str(&format!(
            "<div class=\"feature\"><label><input type=\"checkbox\" name=\"plumb_bootstrap\" \
             value=\"1\"{}><span>Find nodes through plumbsearch.org</span></label>\
             <p class=\"hint\">The Plumb network\u{2019}s own node introduces this one to the \
             others. Plumb nodes on this network are found without it.</p></div>",
            // On by default for a node not yet in the network.
            if saved.uses_default_bootstrap() || (!saved.network && saved.bootstrap.is_empty()) {
                " checked"
            } else {
                ""
            }
        ));
        body.push_str(&format!("<input type=\"hidden\" name=\"trust_shown\" value=\"1\"><div class=\"feature\"><label><input type=\"checkbox\" name=\"default_trust\" value=\"1\"{}><span>Trust plumbsearch.org's crawler</span></label><p class=\"hint\">Take in crawls from the plumbsearch.org node at once, so a new node fills up while the network is small. Turn off to keep only crawls a second crawler confirms.</p></div>", if saved.no_default_trust { "" } else { " checked" }));
        body.push_str(&search_scope_choice(
            saved.search_scope(),
            active.search_scope(),
        ));
        body.push_str(&credit_choices(saved, active));
        body.push_str(&format!(
            "<details{}><summary>Advanced: more bootstrap and trusted nodes</summary>\
             <label for=\"bootstrap\">Bootstrap nodes</label><p class=\"hint\" \
             id=\"bootstrap-help\">Most people never need these. One multiaddress per line, \
             such as a friend\u{2019}s node or your own server: \
             <code>/dns4/example.org/tcp/4001/p2p/12D3Koo\u{2026}</code></p>\
             <textarea id=\"bootstrap\" name=\"bootstrap\" aria-describedby=\"bootstrap-help\" \
             spellcheck=\"false\">{}</textarea>",
            if extra.is_empty() && saved.trusted.is_empty() {
                ""
            } else {
                " open"
            },
            escape_html(&extra.join("\n"))
        ));
        body.push_str(&format!("<label for=\"trusted\">Trusted nodes</label><p class=\"hint\" id=\"trusted-help\">Other node ids whose crawls are taken in at once, one per line. Only add nodes you run or know.</p><textarea id=\"trusted\" name=\"trusted\" aria-describedby=\"trusted-help\" spellcheck=\"false\">{}</textarea></details>", escape_html(&saved.trusted.join("\n"))));
    }
    body.push_str("<button type=\"submit\">Save feature settings</button></form>");
}

/// What this node earned at other nodes, and what it did for them (see
/// [`plumb_net::credits`]).
pub(super) fn render_credits(body: &mut String, credits: &plumb_net::credits::CreditStatus) {
    let total: i64 = credits.at_peers.iter().map(|at| at.credits.max(0)).sum();
    let headline = format!("{} credits at other nodes", group_thousands(total as u64));
    let mut rest = String::new();
    if credits.at_peers.is_empty() {
        rest.push_str(
            "<p>No node has said yet what this node has earned there. Nodes earn credits \
             at each other by crawling and by answering each other\u{2019}s searches.</p>",
        );
    } else {
        let mut rows = String::new();
        for at in credits.at_peers.iter().take(8) {
            let id = &at.peer_id;
            let short = id.get(id.len().saturating_sub(8)..).unwrap_or(id);
            let note = if at.counts {
                ""
            } else {
                " \u{b7} not spendable there yet"
            };
            rows.push_str(&format!(
                "<dt title=\"{}\"><code>\u{2026}{}</code></dt><dd>{} credits{note}</dd>",
                escape_html(id),
                escape_html(short),
                group_thousands(at.credits.max(0) as u64)
            ));
        }
        rest.push_str(&format!("<dl>{rows}</dl>"));
    }
    let limit = match credits.answer_per_day {
        Some(n) => format!(" of {} allowed", group_thousands(n)),
        None => String::new(),
    };
    rest.push_str(&format!(
        "<p>{} searches answered for other nodes today{limit} \u{b7} {} turned away as busy \
         \u{b7} {} answered for tokens.</p><p class=\"hint\">{} tokens held \u{b7} {} spent at \
         busy nodes \u{b7} {} nodes have credits here.</p>",
        group_thousands(credits.free_answers_today),
        group_thousands(credits.turned_away),
        group_thousands(credits.priority_answered),
        group_thousands(credits.tokens_held as u64),
        group_thousands(credits.tokens_spent),
        group_thousands(credits.in_credit_here as u64),
    ));
    card(body, "", "Credits", &headline, &rest);
}

/// How much this node helps other nodes, and whether it spends its credits
/// (see [`plumb_net::allowance`] and [`plumb_net::credits`]).
fn credit_choices(saved: &FeatureSettings, active: &FeatureSettings) -> String {
    let per_day = saved.answer_limit.and_then(|l| l.per_day);
    let running = match active.answer_limit.and_then(|l| l.per_day) {
        Some(n) => format!("{} a day", group_thousands(n)),
        None => "no limit".to_owned(),
    };
    let spend = saved.spend_credits.unwrap_or(true);
    let spending = if active.spend_credits.unwrap_or(true) {
        "on"
    } else {
        "off"
    };
    format!(
        "<input type=\"hidden\" name=\"credits_shown\" value=\"1\">\
         <div class=\"feature\"><label for=\"answer_per_day\">Searches answered for other \
         nodes each day <span class=\"state\">\u{b7} currently {running}</span></label>\
         <input id=\"answer_per_day\" name=\"answer_per_day\" inputmode=\"numeric\" \
         placeholder=\"No limit\" value=\"{}\" aria-describedby=\"answer-help\">\
         <p class=\"hint\" id=\"answer-help\">How much this node helps the network. Past \
         this many a day, it answers only nodes that spend credits they earned here by \
         crawling or answering. Searches on this node\u{2019}s own page are never limited. \
         Leave blank for no limit.</p></div>\
         <div class=\"feature\"><label><input type=\"checkbox\" name=\"spend_credits\" \
         value=\"1\"{}><span>Spend credits when a node is busy <span class=\"state\">\
         \u{b7} currently {spending}</span></span></label><p class=\"hint\">This node earns \
         credits at other nodes by crawling and answering their searches, and spends them to \
         be answered first when they are busy.</p></div>",
        per_day.map(|n| n.to_string()).unwrap_or_default(),
        if spend { " checked" } else { "" },
    )
}

/// The choice of which nodes network searches ask (see
/// [`plumb_net::scope`]), `saved` checked, saying which one is `running`.
fn search_scope_choice(saved: plumb_net::SearchScope, running: plumb_net::SearchScope) -> String {
    use plumb_net::SearchScope;
    let mut html = format!(
        "<fieldset class=\"feature scope\"><legend>Search the network through \
         <span class=\"state\">\u{b7} currently {}</span></legend>",
        running.label().to_lowercase()
    );
    for scope in SearchScope::ALL {
        let hint = match scope {
            SearchScope::Trusted => {
                "Only the nodes you trust: plumbsearch.org unless turned off, \
                                     and the trusted nodes below."
            }
            SearchScope::FriendsOfFriends => {
                "Your trusted nodes and the nodes they trust. The \
                                              default."
            }
            SearchScope::Anyone => {
                "Any Plumb node. Finds the most, from nodes you know nothing \
                                    about."
            }
        };
        html.push_str(&format!(
            "<label><input type=\"radio\" name=\"search_from\" value=\"{}\"{}><span>{}</span>\
             </label><p class=\"hint\">{hint}</p>",
            scope.as_str(),
            if scope == saved { " checked" } else { "" },
            scope.label()
        ));
    }
    html.push_str("</fieldset>");
    html
}

pub(super) fn render_settings(body: &mut String, settings: &NodeSettings, base: &str) {
    let checked = |on: bool| if on { " checked" } else { "" };
    let limit = |mb: u64| {
        if mb == 0 {
            String::new()
        } else {
            mb.to_string()
        }
    };
    body.push_str(&format!(
        "<h2>Crawling &amp; limits</h2>\n<form method=\"post\" action=\"{base}/settings\">\n\
         <label><input type=\"checkbox\" name=\"background_updates\" value=\"1\"{}>\
         <span>Keep the index up to date in the background</span></label>\n\
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn what sites \
         are about and find new sites, then rebuilds its index. Search keeps working when this is \
         off.</p>\n<fieldset class=\"workload\"><legend>Workload</legend>\n",
        checked(settings.background_updates)
    ));
    for (workload, label, hint) in [
        (
            Workload::Light,
            "Light",
            "4 homepages at a time, 100 MB of downloads a day, 1 GB of disk. For a laptop or \
             a slow connection.",
        ),
        (
            Workload::Balanced,
            "Balanced",
            "16 homepages at a time, 500 MB a day, 2 GB of disk.",
        ),
        (
            Workload::Full,
            "Full",
            "32 homepages at a time and no limits. For a server or homelab.",
        ),
        (
            Workload::Custom,
            "Custom",
            "16 homepages at a time, with the limits below.",
        ),
    ] {
        body.push_str(&format!(
            "<label><input type=\"radio\" name=\"workload\" value=\"{}\"{}><span>{label} \
             <span class=\"state\">{hint}</span></span></label>\n",
            workload.name(),
            checked(settings.workload == workload)
        ));
    }
    body.push_str(&format!(
        "</fieldset>\n\
         <label>Download limit <input type=\"number\" name=\"download_limit_mb_per_day\" \
         min=\"0\" step=\"1\" value=\"{}\" placeholder=\"none\"> MB per UTC day</label>\n\
         <p class=\"hint\">Crawling pauses for the rest of the UTC day once it is reached. Empty \
         for no limit. Used with Custom; a preset sets it.</p>\n\
         <label>Storage limit <input type=\"number\" name=\"storage_limit_mb\" min=\"0\" \
         step=\"1\" value=\"{}\" placeholder=\"none\"> MB</label>\n\
         <p class=\"hint\">Crawling pauses while the data folder is bigger. Empty for no \
         limit. Used with Custom; a preset sets it.</p>\n\
         <input type=\"hidden\" name=\"fill_shown\" value=\"1\">\
         <label><input type=\"checkbox\" name=\"fill_from_network\" value=\"1\"{}>\
         <span>Fill free space with the network's crawls</span></label>\n\
         <p class=\"hint\">Asks a node you trust for the sites it has crawled, most popular \
         first, until 90% of the storage limit (or of what this machine's memory can index) \
         is used. In the network only.</p>\n\
         <input type=\"hidden\" name=\"focus_shown\" value=\"1\">\
         <label for=\"focus_topics\">Focus topics</label>\n\
         <textarea id=\"focus_topics\" name=\"focus_topics\" rows=\"3\" \
         placeholder=\"games\">{}</textarea>\n\
         <p class=\"hint\">One per line. This node crawls sites about them first and twice as \
         often, and keeps more of them when the storage limit is tight, so the more nodes \
         focus on a topic, the better Plumb knows it. Other nodes can tell from what this \
         node crawls. The interests on a browser\u{2019}s About you page stay private: they \
         only change which sites this node keeps.</p>\n",
        limit(settings.download_limit_mb_per_day),
        limit(settings.storage_limit_mb),
        checked(settings.fill_from_network),
        escape_html(&settings.focus_topics.join("\n"))
    ));
    let hours = settings
        .crawl_hours
        .unwrap_or(CrawlHours { from: 22, to: 7 });
    let options = |selected: u8| {
        (0..24u8)
            .map(|h| {
                format!(
                    "<option value=\"{h}\"{}>{h:02}:00</option>",
                    if h == selected { " selected" } else { "" }
                )
            })
            .collect::<String>()
    };
    let (hour, minute, _) = crate::node::schedule::local_time();
    let page_sets = render_page_sets(settings);
    body.push_str(&format!(
        "<label><input type=\"checkbox\" name=\"crawl_hours\" value=\"1\"{}>\
         <span>Only crawl between <select name=\"crawl_from\" aria-label=\"From\">{}</select> \
         and <select name=\"crawl_to\" aria-label=\"Until\">{}</select></span></label>\n\
         <p class=\"hint\">On the node\u{2019}s clock, which reads {hour:02}:{minute:02} now. \
         Overnight hours work too, such as 22:00 to 07:00.</p>\n\
         {page_sets}<button type=\"submit\">Save settings</button>\n</form>\n",
        checked(settings.crawl_hours.is_some()),
        options(hours.from),
        options(hours.to)
    ));
}

/// The page sets part of the settings form: how many pages of each set
/// (Wikipedia articles) to list with the sites.
fn render_page_sets(settings: &NodeSettings) -> String {
    let mut out = String::from(
        "<fieldset class=\"workload page-sets\"><legend>Page sets</legend>\n\
         <input type=\"hidden\" name=\"page_sets_shown\" value=\"1\">\n\
         <p class=\"hint\">Single pages, such as Wikipedia articles, listed with the sites. \
         Only each page's title and one-line description are kept, about 100 bytes a page.</p>\n",
    );
    for set in SETS {
        let current = settings.page_sets.size(set.id);
        let mut choices = vec![PageSetSize::Auto];
        choices.extend_from_slice(SIZE_CHOICES);
        if !choices.contains(&current) {
            choices.push(current);
        }
        let options: String = choices
            .iter()
            .map(|size| {
                format!(
                    "<option value=\"{size}\"{}>{}</option>",
                    if *size == current { " selected" } else { "" },
                    escape_html(&size.words())
                )
            })
            .collect();
        let kept = current.pages(settings.storage_limit_mb).min(set.pages);
        out.push_str(&format!(
            "<div class=\"page-set\"><label>{} <select name=\"page_set.{}\">{options}</select></label>\n\
             <p class=\"hint\">Keeps {} pages now, about {} MB.</p></div>\n",
            escape_html(set.name),
            escape_html(set.id),
            if kept == set.pages {
                format!("all of them, about {}", thousands(set.pages))
            } else {
                thousands(kept)
            },
            (kept * set.bytes_per_page).div_ceil(1_000_000)
        ));
    }
    out.push_str("</fieldset>\n");
    out
}

/// For each plugin a search can fit without a keyword (by its `ids` or
/// `hints`), the choice of what such a search does with it.
pub(super) fn render_plugin_choices(
    body: &mut String,
    plugins: &crate::plugins::Plugins,
    base: &str,
) {
    use crate::plugins::Suggest;
    let fitting: Vec<_> = plugins.list().filter(|p| p.manifest.can_fit()).collect();
    if fitting.is_empty() {
        return;
    }
    body.push_str(&format!(
        "<h2 id=\"plugins\">Plugins</h2><p class=\"hint\">When a search fits a plugin, such as a search \
         about a band for a music plugin, it can run on its own, or show a link that runs it, \
         which saves a source’s daily quota. Its keywords always run it.</p>\
         <form method=\"post\" action=\"{base}/plugins\">"
    ));
    for plugin in fitting {
        let chosen = plugins.suggest(plugin);
        let keywords = plugin
            .manifest
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>();
        let mut options = String::new();
        for suggest in Suggest::ALL {
            let label = match suggest {
                Suggest::Automatic => "Run it when a search fits",
                Suggest::Button => "Show a link to its results when a search fits",
                Suggest::Keywords => "Only when a search has one of its keywords",
            };
            options.push_str(&format!(
                "<option value=\"{}\"{}>{label}</option>",
                suggest.as_str(),
                if suggest == chosen { " selected" } else { "" }
            ));
        }
        let id = format!("plugin-{}", plugin.id);
        let keywords = if keywords.is_empty() {
            String::new()
        } else {
            format!("Keywords: {}. ", keywords.join(", "))
        };
        body.push_str(&format!(
            "<div class=\"feature\"><label for=\"{id}\">{}</label> \
             <select id=\"{id}\" name=\"{}\">{options}</select>\
             <p class=\"hint\">{}{}</p></div>",
            escape_html(&plugin.manifest.name),
            escape_html(&plugin.id),
            escape_html(&keywords),
            escape_html(&plugin.manifest.about),
        ));
    }
    body.push_str("<p><button type=\"submit\">Save plugin choices</button></p></form>");
}
