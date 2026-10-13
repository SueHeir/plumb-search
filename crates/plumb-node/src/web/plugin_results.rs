//! What plugins add to a results page: their results, offers, buttons
//! and notes.

use std::fmt::Write as _;

use plumb_index::SearchOptions;

use super::html::{escape_html, http_url, search_link, time_ago};
use crate::plugins::PluginResults;

/// What one of the node's plugins found, as an item of the results
/// list after the first result, named for the plugin so that nobody
/// takes it for Plumb's own. Plugin text is as untrusted as any record's,
/// and is escaped the same way.
/// A plugin's block of results. With `token` (the page is for the node's
/// owner), its results' buttons are forms that press them.
pub(crate) fn render_plugin(
    found: &PluginResults,
    now: u64,
    query: &str,
    token: Option<&str>,
) -> String {
    let mut items = String::new();
    for item in &found.results {
        let magnet = item.url.starts_with("magnet:?");
        let Some(href) = (if magnet {
            Some(item.url.clone())
        } else {
            http_url(&item.url)
        }) else {
            continue;
        };
        let mut meta = if magnet {
            "Magnet link".to_string()
        } else {
            escape_html(&item.site)
        };
        if let Some(at) = item.published {
            let _ = write!(meta, " &middot; {}", time_ago(at, now));
        }
        let snippet = item
            .snippet
            .as_deref()
            .map(|s| format!("<p class=\"d\">{}</p>", escape_html(s)))
            .unwrap_or_default();
        // Only pictures the node fetched itself, carried in the page.
        let image = item
            .image
            .as_deref()
            .filter(|i| i.starts_with("data:image/"))
            .map(|i| format!("<img class=\"pim\" src=\"{}\" alt=\"\">", escape_html(i)))
            .unwrap_or_default();
        let badge = item
            .badge
            .as_deref()
            .map(|b| format!(" <span class=\"pb\">{}</span>", escape_html(b)))
            .unwrap_or_default();
        let mut buttons = token
            .map(|token| action_forms(&found.plugin, &item.actions, token, query, true))
            .unwrap_or_default();
        if !buttons.is_empty() {
            buttons = format!("<div class=\"pa\">{buttons}</div>");
        }
        let _ = write!(
            items,
            "<li>{image}<a href=\"{}\" rel=\"noreferrer\">{}</a>{badge}{snippet}\
             <div class=\"m\">{meta}</div>{buttons}</li>",
            escape_html(&href),
            escape_html(&item.title),
        );
    }
    if items.is_empty() {
        return String::new();
    }
    format!(
        "<li class=\"news plugin\"><p class=\"nh\">From {} <span class=\"m\">plugin on this node</span></p>\
         <ol>{items}</ol></li>\n",
        escape_html(&found.name)
    )
}

/// Links that run the plugins a search fits but did not run, each with
/// its keyword: "Show results from YouTube".
pub(crate) fn render_offers(
    offers: &[crate::plugins::Offer],
    query: &str,
    options: &SearchOptions,
) -> String {
    if offers.is_empty() {
        return String::new();
    }
    let links: Vec<String> = offers
        .iter()
        .map(|offer| {
            format!(
                "<a href=\"{}\">Show results from {}</a>",
                escape_html(&format!(
                    "{}&run={}",
                    search_link("/search", query, options, false),
                    url::form_urlencoded::byte_serialize(offer.plugin.as_bytes())
                        .collect::<String>()
                )),
                escape_html(&offer.name)
            )
        })
        .collect();
    format!(
        "<li class=\"news plugin offer\"><p class=\"nh\">{} <span class=\"m\">plugin on this node</span></p></li>\n",
        links.join(" · ")
    )
}

/// The forms that press `actions` of plugin `plugin` on the results page
/// for `query`. With `ran`, the plugin's results are on the page, and the
/// way back runs it again: one only offered would be gone.
pub(crate) fn action_forms(
    plugin: &str,
    actions: &[crate::plugins::PluginAction],
    token: &str,
    query: &str,
    ran: bool,
) -> String {
    let run = if ran {
        format!(
            "<input type=\"hidden\" name=\"run\" value=\"{}\">",
            escape_html(plugin)
        )
    } else {
        String::new()
    };
    let mut forms = String::new();
    for action in actions {
        let _ = write!(
            forms,
            "<form method=\"post\" action=\"/plugins/act\">\
             <input type=\"hidden\" name=\"plugin\" value=\"{}\">\
             <input type=\"hidden\" name=\"data\" value=\"{}\">\
             <input type=\"hidden\" name=\"token\" value=\"{}\">\
             <input type=\"hidden\" name=\"q\" value=\"{}\">{run}\
             <button type=\"submit\">{}</button></form>",
            escape_html(plugin),
            escape_html(&action.data),
            escape_html(token),
            escape_html(query),
            escape_html(&action.label),
        );
    }
    forms
}

/// What plugins say about one of the node's results: each one's badge,
/// named for the plugin, and, with `token` (the page is for the node's
/// owner), its buttons. Empty when they say nothing.
pub(crate) fn render_notes(
    notes: &[crate::plugins::ResultNote],
    query: &str,
    token: Option<&str>,
) -> String {
    let mut line = String::new();
    for note in notes {
        if let Some(badge) = &note.badge {
            let _ = write!(
                line,
                "<span class=\"pb\" title=\"From the {name} plugin on this node\">{name}: {}</span>",
                escape_html(badge),
                name = escape_html(&note.name),
            );
        }
        if let Some(token) = token {
            line.push_str(&action_forms(
                &note.plugin,
                &note.actions,
                token,
                query,
                false,
            ));
        }
    }
    if line.is_empty() {
        return line;
    }
    format!("<div class=\"pn\">{line}</div>")
}
