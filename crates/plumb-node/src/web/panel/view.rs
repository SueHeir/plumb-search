//! The panel page: what the node is doing, and a card for each part that
//! says how far it has come.

use std::path::Path;

use super::guard::PANEL_REFERRER_POLICY;
use super::settings_view::{
    render_credits, render_features, render_plugin_choices, render_settings,
};
use super::{
    items_in_words, PanelQuery, ADD_TO_FIREFOX_PATH, BUSY_RELOAD_SECONDS, IDLE_RELOAD_SECONDS,
    PANEL_STYLE,
};
use crate::node::backup::{self, BackupInfo};
use crate::node::features::FeatureSettings;
use crate::node::{LogEntry, LogLevel, NodeSettings, Phase, Status, Step, MB};
use crate::web::{escape_html, group_thousands, page_with_head, time_ago, time_until};

fn render_remote_control(body: &mut String, remote: &RemoteControlView, writable: bool) {
    body.push_str(
        "<p class=\"intro\">Let the Plumb Search app on another computer change this \
         node's settings, crawling and features. It can read the node's status and change \
         its settings, nothing else.</p>",
    );
    let (big, rest) = match remote.on {
        None => ("Off", "<p>No other computer can change this node.</p>"),
        Some(false) => (
            "On",
            "<p>From this computer and local networks only (home and office \
             networks, Docker, Tailscale), with the token.</p>",
        ),
        Some(true) => (
            "On, from anywhere",
            "<p>From any address, with the token. Use HTTPS in front of the node so the \
             token is not sent in the clear.</p>",
        ),
    };
    body.push_str("<section class=\"cards\">");
    card(body, "", "Remote control", big, rest);
    body.push_str("</section>");
    if remote.loopback_only {
        body.push_str(
            "<p class=\"notice\">This node only listens on this computer (127.0.0.1), so \
             other computers cannot reach it. Remote control is for nodes that serve the \
             network, such as a Docker container or a homelab server started with \
             <code>--bind 0.0.0.0:8080</code>.</p>",
        );
    }
    if !writable {
        body.push_str(
            "<p class=\"hint\">To turn remote control on for a Docker container, run \
             <code>docker exec &lt;container&gt; plumb remote-control on</code> \
             on its host. It prints the token.</p><fieldset disabled>",
        );
    }
    let turn_on = if remote.on.is_some() {
        "Make a new token"
    } else {
        "Turn on and make a token"
    };
    body.push_str(&format!(
        "<form method=\"post\" action=\"/app/remote-control\">\
         <input type=\"hidden\" name=\"action\" value=\"on\">\
         <label><input type=\"checkbox\" name=\"allow_public\" value=\"1\"{}>\
         <span>Also from public addresses and through a reverse proxy</span></label>\
         <p class=\"hint\">Leave this off unless the node is behind HTTPS. Off, only \
         this computer and local networks can use the token.</p>\
         <button type=\"submit\">{turn_on}</button></form>",
        if remote.on == Some(true) {
            " checked"
        } else {
            ""
        }
    ));
    if remote.on.is_some() {
        body.push_str(
            "<form method=\"post\" action=\"/app/remote-control\">\
             <input type=\"hidden\" name=\"action\" value=\"off\">\
             <button type=\"submit\" class=\"alt\">Turn remote control off</button></form>",
        );
    }
    if !writable {
        body.push_str("</fieldset>");
    }
}

/// What the panel says about search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    SettingUp,
    /// Searchable, without all of the seed data yet.
    Limited,
    Ready,
}

fn readiness(status: &Status) -> Readiness {
    match status.phase {
        Phase::SettingUp => Readiness::SettingUp,
        Phase::Ready if status.wikidata_missing => Readiness::Limited,
        Phase::Ready => Readiness::Ready,
    }
}

/// Whether work is under way that the panel should follow closely.
pub(super) fn busy(status: &Status) -> bool {
    status.phase != Phase::Ready
        || !matches!(status.step, Step::Idle | Step::Retrying | Step::Stopping)
}

/// Whether setup still has steps to go: the full index or the first crawl.
pub(super) fn setting_up(status: &Status) -> bool {
    status.phase != Phase::Ready || status.wikidata_missing || status.last_refresh.is_none()
}

/// Bytes in words: `850 KB`, `312 MB`, `1.4 GB`.
pub(super) fn bytes_words(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const GB: u64 = 1_000 * MB;
    if bytes >= 10 * GB {
        format!("{} GB", group_thousands(bytes / GB))
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else {
        format!("{} KB", bytes.div_ceil(KB))
    }
}

pub(in crate::web) struct PanelView<'a> {
    pub(in crate::web) status: &'a Status,
    pub(in crate::web) settings: &'a NodeSettings,
    /// Where the node's search pages are: links to them start with it.
    pub(in crate::web) origin: &'a str,
    pub(in crate::web) data_dir: Option<&'a Path>,
    pub(in crate::web) now: u64,
    pub(in crate::web) query: &'a PanelQuery,
    pub(in crate::web) active: &'a FeatureSettings,
    pub(in crate::web) saved: &'a FeatureSettings,
    pub(in crate::web) writable: bool,
    pub(in crate::web) private_ready: bool,
    /// The panel's own address, `/app` for this node and
    /// `/app/nodes/<id>` for another node controlled from here; its links
    /// and forms start with it.
    pub(in crate::web) base: &'a str,
    /// Above the title: what node this is.
    pub(in crate::web) eyebrow: &'a str,
    /// The row of nodes to switch between, as HTML; empty for none.
    pub(in crate::web) switcher: &'a str,
    /// This node's remote control, for its "Remote control" section; `None`
    /// on the panel of another node.
    pub(in crate::web) remote_control: Option<&'a RemoteControlView>,
    /// The node's activity log, newest first.
    pub(in crate::web) activity: &'a [LogEntry],
    /// This node's saved backups, for its "Backup" section; `None` on the
    /// panel of another node.
    pub(in crate::web) backups: Option<&'a [BackupInfo]>,
    /// This node's plugins, for the choice of when they run; `None` for
    /// another node's panel.
    pub(in crate::web) plugins: Option<&'a crate::plugins::Plugins>,
}

/// What the "Remote control" section shows.
pub(in crate::web) struct RemoteControlView {
    /// On, and whether from public addresses too.
    pub(in crate::web) on: Option<bool>,
    /// The node only listens on this computer, so no other can reach it.
    pub(in crate::web) loopback_only: bool,
}

pub(in crate::web) fn render_panel(view: &PanelView<'_>) -> String {
    let PanelView {
        status,
        settings,
        origin,
        data_dir,
        now,
        query,
        active,
        saved,
        writable,
        private_ready,
        base,
        eyebrow,
        switcher,
        remote_control,
        activity,
        backups,
        plugins,
    } = *view;
    let mut sections = vec![
        ("overview", "Overview"),
        ("search", "Search & browser"),
        ("resources", "Resources"),
        ("network", "Network & privacy"),
        ("integrations", "AI & plugins"),
    ];
    if remote_control.is_some() {
        sections.push(("remote", "Remote control"));
    }
    sections.push(("activity", "Activity"));
    if backups.is_some() {
        sections.push(("backup", "Backup"));
    }
    sections.push(("about", "About"));
    let section = if sections.iter().any(|(key, _)| *key == query.section) {
        query.section.as_str()
    } else {
        "overview"
    };
    let title = sections.iter().find(|(key, _)| *key == section).unwrap().1;
    let site = escape_html(origin);
    let bar = crate::web::app_bar_for("settings", true, base);
    let mut body = format!("<div class=\"wrap node-panel\">{bar}<div class=\"node-layout\"><aside class=\"node-sidebar\"><p class=\"eyebrow\">{}</p>{switcher}<nav class=\"node-nav\" aria-label=\"Node settings\">", escape_html(eyebrow));
    for (key, label) in sections.iter().copied() {
        body.push_str(&format!(
            "<a href=\"{base}?section={key}\"{}>{}</a>",
            if key == section {
                " aria-current=\"page\""
            } else {
                ""
            },
            escape_html(label)
        ));
    }
    body.push_str(&format!("</nav><a class=\"btn alt\" href=\"{site}/\">Open search</a></aside><main class=\"node-content\"><div class=\"section-heading\"><h1>{}</h1><a href=\"{base}?section={section}\">Refresh status</a></div>", escape_html(title)));
    // Only this node's own panel has a restart route; remote nodes do not.
    if active != saved && status.can_restart && writable && base == "/app" {
        body.push_str(&format!("<form method=\"post\" action=\"{base}/restart\" class=\"notice\" role=\"status\"><p>Feature changes saved. They apply once the node restarts, which takes a few seconds; search pauses meanwhile.</p><button type=\"submit\">Restart to apply</button></form>"));
    } else if active != saved {
        body.push_str("<p class=\"notice\" role=\"status\">Feature changes saved. Quit and reopen the desktop app, or restart the Docker container, to apply them. Closing the desktop window does not quit the app.</p>");
    } else if query.saved == "settings" {
        body.push_str(
            "<p class=\"notice\" role=\"status\">Resource settings saved and applied.</p>",
        );
    } else if query.saved == "setup" {
        body.push_str("<p class=\"notice\" role=\"status\">Your node is set up. It takes in the network's crawls in the background, up to the size you chose.</p>");
    } else if query.saved == "backup" {
        body.push_str("<p class=\"notice\" role=\"status\">Backup saved.</p>");
    } else if query.saved == "restored" {
        body.push_str(if status.can_restart {
            "<p class=\"notice\" role=\"status\">Backup restored. The node is restarting to \
             finish; the settings from before are in a backup marked before-restore.</p>"
        } else {
            "<p class=\"notice\" role=\"status\">Backup restored. Restart the node (for \
             Docker, the container) to finish; the settings from before are in a backup \
             marked before-restore.</p>"
        });
    } else if query.saved == "retry" {
        body.push_str("<p class=\"notice\" role=\"status\">Trying the bootstrap nodes again. Refresh the status in a few seconds.</p>");
    } else if query.saved == "plugins" {
        body.push_str("<p class=\"notice\" role=\"status\">Plugin choices saved. They apply from the next search.</p>");
    } else if query.saved == "features" {
        body.push_str("<p class=\"notice\" role=\"status\">Feature settings saved. No restart is needed because they match the running node.</p>");
    }
    if !writable {
        body.push_str("<p class=\"notice\">Viewing this node remotely. Settings are read-only. To change them, open the desktop app or connect to the node through a loopback address on its host. Docker bridge networking may also require host-side configuration.</p>");
    }
    match section {
        "overview" => {
            if !settings.setup_chosen && writable && base == "/app" {
                crate::web::setup::render_setup(&mut body, settings);
            }
            body.push_str("<p class=\"intro\">Search readiness, background work, and the resources your node is using.</p><section class=\"cards\" aria-label=\"Node overview\">");
            render_search_card(&mut body, status, origin, now, base);
            render_storage_card(&mut body, status, settings);
            render_downloads_card(&mut body, status, settings);
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_crawl_card(&mut body, status, settings, now, base);
            render_network_card(&mut body, status, active, now, base);
            if !writable {
                body.push_str("</fieldset>");
            }
            if status.meaning_work.is_some() {
                render_meaning_card(&mut body, status, active, now, base);
            }
            body.push_str("</section>");
            if setting_up(status) {
                render_steps(&mut body, status, now);
            }
            if let Some(err) = &status.last_error {
                let retry = if writable {
                    retry_button(base, "work", "Try again now")
                } else {
                    String::new()
                };
                let when = err
                    .retry_at
                    .map(|at| format!(" Plumb tries again by itself {}.", time_until(at, now)))
                    .unwrap_or_default();
                body.push_str(&format!("<div class=\"err\" role=\"alert\"><strong>Last update failed</strong><p>{}</p><p class=\"hint\">{}{when}</p>{retry}</div>", "Check your internet connection or proxy. Open Activity for technical details.", escape_html(&format!("{}.", time_ago(err.at, now)))));
            }
            if !activity.is_empty() {
                let recent: Vec<_> = activity
                    .iter()
                    .filter(|entry| {
                        status.last_error.as_ref().is_none_or(|err| {
                            entry.message != err.message
                                && entry.level != crate::node::journal::LogLevel::Error
                        })
                    })
                    .take(5)
                    .cloned()
                    .collect();
                if !recent.is_empty() {
                    body.push_str("<h2>Recent activity</h2>");
                    render_log(&mut body, &recent, now);
                }
                body.push_str(&format!(
                    "<p><a href=\"{base}?section=activity\">All activity</a></p>"
                ));
            }
        }
        "resources" => {
            body.push_str("<p class=\"intro\">Control background crawling and its download and storage budget. Changes apply immediately.</p><section class=\"cards\">");
            render_storage_card(&mut body, status, settings);
            render_downloads_card(&mut body, status, settings);
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_settings(&mut body, settings, base);
            if !writable {
                body.push_str("</fieldset>");
            }
        }
        "search" => {
            body.push_str("<p class=\"intro\">Choose how you search and connect your browser to this node.</p><section class=\"cards\">");
            render_search_card(&mut body, status, origin, now, base);
            render_meaning_card(&mut body, status, active, now, base);
            let private = if private_ready {
                "Ready"
            } else if !crate::web::private::in_build() {
                "Unavailable in this build"
            } else if !active.network && !active.private_search {
                "Off until this node joins the Plumb network"
            } else {
                "Preparing index"
            };
            let link = if private_ready {
                format!(
                    "<p><a class=\"btn alt\" href=\"{site}/private\">Open private search</a></p>"
                )
            } else {
                String::new()
            };
            card(
                &mut body,
                "",
                "Private browser search",
                private,
                &format!("<p>Rank results in your browser so your query stays there. Visitors turn it on with the Private search switch in the search page’s settings gear. It is on wherever this node has the network’s buckets.</p>{link}"),
            );
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_features(
                &mut body,
                active,
                saved,
                "search",
                base,
                status.can_restart && base == "/app",
            );
            if let Some(plugins) = plugins {
                render_plugin_choices(&mut body, plugins, base);
            }
            if !writable {
                body.push_str("</fieldset>");
            }
            render_browser(&mut body, origin);
        }
        "network" => {
            body.push_str("<p class=\"intro\">Connect to other nodes, check shared crawling, and choose optional search features.</p><section class=\"cards\">");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_network_card(&mut body, status, active, now, base);
            if !writable {
                body.push_str("</fieldset>");
            }
            render_peers(&mut body, status);
            render_network_details(&mut body, status);
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_features(
                &mut body,
                active,
                saved,
                "network",
                base,
                status.can_restart && base == "/app",
            );
            if !writable {
                body.push_str("</fieldset>");
            }
        }
        "activity" => {
            body.push_str("<p class=\"intro\">What the node did and what went wrong, newest first. It keeps the last few hundred entries.</p>");
            if activity.is_empty() {
                body.push_str("<p>Nothing yet.</p>");
            } else {
                render_log(&mut body, activity, now);
            }
            body.push_str(&format!("<p class=\"hint\">For a bug report, you can also download <a href=\"{site}/api/status\" target=\"_blank\">raw status (JSON)</a>.</p>"));
        }
        "integrations" => {
            body.push_str("<p class=\"intro\">Use your own search index from an AI app, or add sources with plugins.</p>");
            body.push_str(&format!(
                "<section class=\"cards\" aria-label=\"AI connections\">\
                 <div class=\"card\"><h3>MCP server</h3><p class=\"big\">For AI apps</p>\
                 <p>Search, official sites, packages and sourced facts. No API key.</p>\
                 <label for=\"mcp-url\">Server address</label><input id=\"mcp-url\" class=\"endpoint\" readonly value=\"{site}/mcp\">\
                 <p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/mcp.md\" target=\"_blank\">MCP setup guide ↗</a></p></div>\
                 <div class=\"card\"><h3>SearXNG-compatible search</h3><p class=\"big\">For local models</p>\
                 <p>Use this node in Open WebUI and other tools that take a SearXNG address.</p>\
                 <label for=\"search-url\">Base address</label><input id=\"search-url\" class=\"endpoint\" readonly value=\"{site}\">\
                 <p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/local-llms.md\" target=\"_blank\">App setup guides ↗</a></p></div></section>"
            ));
            body.push_str("<h2>Plugins on this node</h2><p class=\"hint\">Plugins run in a WebAssembly sandbox and can contact their declared sources. Running a plugin can send search text to that source.</p>");
            match plugins {
                Some(plugins) if plugins.list().next().is_some() => {
                    body.push_str("<ul class=\"log\">");
                    for plugin in plugins.list() {
                        body.push_str(&format!("<li><strong>{}</strong><span>{}</span></li>", escape_html(&plugin.manifest.name), escape_html(&plugin.manifest.about)));
                    }
                    body.push_str(&format!("</ul><p><a href=\"{base}?section=search#plugins\">Choose when plugins run</a></p>"));
                }
                Some(_) => body.push_str("<div class=\"notice\"><h3>No plugins installed</h3><p>Add a plugin to the plugins folder in this node’s data directory, then restart the node.</p></div>"),
                None => body.push_str("<p>Manage plugins on the remote node’s own computer.</p>"),
            }
            body.push_str("<p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/plugins.md\" target=\"_blank\">Plugin installation &amp; development ↗</a></p>");
        }
        "backup" => {
            if let Some(backups) = backups {
                render_backups(&mut body, backups, now, status.can_restart);
            }
        }
        "remote" => {
            if let Some(remote) = remote_control {
                render_remote_control(&mut body, remote, writable);
            }
        }
        _ => {
            render_about(&mut body, status, data_dir, now);
            body.push_str(&format!("<p>Desktop and Docker run the same node and settings panel.</p><p>Its data comes from Wikipedia, Stack Overflow and ecosyste.ms (CC BY-SA 4.0), OpenStreetMap (ODbL), Wikidata, OpenAlex and Open Library (CC0), GitHub, Tranco, Common Crawl and the Block List Project.</p><p><a href=\"https://github.com/SueHeir/plumb-search\" target=\"_blank\">Source code &amp; documentation ↗</a> · <a href=\"https://github.com/SueHeir/plumb-search#data-sources\" target=\"_blank\">Data sources &amp; licences ↗</a> · <a href=\"{site}/api/status\" target=\"_blank\">Diagnostic status ↗</a></p><p class=\"hint\">Desktop: use the tray or menu bar for Start at login and Quit Plumb Search.</p>"));
        }
    }
    if section == "overview" {
        let (value, label) = if query.refresh == "on" {
            ("off", "Pause automatic refresh")
        } else {
            ("on", "Enable automatic refresh")
        };
        body.push_str(&format!("<p class=\"hint\"><a href=\"{base}?section=overview&amp;refresh={value}\">{label}</a>. Automatic refresh reloads this page and may move keyboard focus.</p>"));
    }
    body.push_str("</main></div></div>");
    // Never reload forms: a timed reload discards unsaved edits and keyboard focus.
    let reload = if section == "overview" && query.saved.is_empty() && query.refresh == "on" {
        format!(
            "<meta http-equiv=\"refresh\" content=\"{}\">",
            if busy(status) {
                BUSY_RELOAD_SECONDS
            } else {
                IDLE_RELOAD_SECONDS
            }
        )
    } else {
        String::new()
    };
    let head = format!("<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">{reload}<style>{PANEL_STYLE}</style><style>{LAYOUT_STYLE}</style>");
    page_with_head(&format!("{title} - Plumb Search"), &head, &body)
}

pub(in crate::web) const LAYOUT_STYLE: &str = "";

/// A card: its class, title, headline and the HTML under them.
pub(super) fn card(body: &mut String, class: &str, title: &str, big: &str, rest: &str) {
    body.push_str(&format!(
        "<div class=\"card {class}\">\n<h3>{}</h3>\n<p class=\"big\">{}</p>\n{rest}</div>\n",
        escape_html(title),
        escape_html(big)
    ));
}

fn meter(done: u64, total: u64, label: &str) -> String {
    let max = total.max(done).max(1);
    format!(
        "<progress aria-label=\"{label}\" value=\"{}\" max=\"{max}\"></progress>\n",
        done.min(max)
    )
}

fn render_search_card(body: &mut String, status: &Status, origin: &str, _now: u64, base: &str) {
    let origin = escape_html(origin);
    let buttons = format!(
        "<div class=\"btns\"><a class=\"btn\" href=\"{origin}/\">\
         Open search</a>\
         <a class=\"btn alt\" href=\"{origin}{ADD_TO_FIREFOX_PATH}\" target=\"_blank\">\
         Add to Firefox</a></div>\n"
    );
    match readiness(status) {
        Readiness::SettingUp => {
            let mut rest = format!(
                "<p>Getting a list of popular sites and building a first index. Search is \
                 ready in a minute or two.</p>\n<p>{}</p>\n",
                escape_html(&status.detail)
            );
            if let Some(progress) = &status.progress {
                rest.push_str(&meter(progress.done, progress.total, "Setup progress"));
            }
            card(
                body,
                "search setting-up",
                "Search",
                "Setting up search",
                &rest,
            );
        }
        Readiness::Limited => {
            let why = if status.wikidata_error.is_some() {
                "Plumb could not download Wikidata's list of official websites yet and will \
                 try again, so results get better once it is in."
            } else {
                "Plumb is still adding Wikidata's list of official websites and more \
                 rankings, so results get better once that is done."
            };
            let retry = if status.wikidata_error.is_some() {
                retry_button(base, "wikidata", "Try Wikidata again now")
            } else {
                String::new()
            };
            let rest = format!(
                "<p>Search works now with the {} most popular sites. {}</p>\n{retry}{buttons}",
                group_thousands(status.sites),
                escape_html(why)
            );
            card(
                body,
                "search limited",
                "Search",
                "Limited search is ready",
                &rest,
            );
        }
        Readiness::Ready => {
            let rest = format!(
                "<p>{} items indexed.</p>\n{buttons}",
                group_thousands(status.items())
            );
            card(body, "search ready", "Search", "Search is ready", &rest);
        }
    }
}

fn render_storage_card(body: &mut String, status: &Status, settings: &NodeSettings) {
    let limit = settings.storage_limit_mb.saturating_mul(MB);
    let mut rest = if limit == 0 {
        "<p>No storage limit.</p>\n".to_string()
    } else {
        format!(
            "{}<p>of {} allowed.</p>\n",
            meter(status.disk_used, limit, "Storage used"),
            bytes_words(limit)
        )
    };
    if let Some(fill) = &status.fill {
        let detail = fill.detail.trim_end_matches('.');
        rest.push_str(&format!(
            "<p>{}Filled with the network's crawls: {} sites.{}</p>\n",
            if fill.blackhole {
                format!(
                    "Blackhole: taking all the network's data ({} of the trusted nodes' lists held). ",
                    fill.lists_done
                )
            } else {
                String::new()
            },
            group_thousands(fill.filled),
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {}.", escape_html(detail))
            }
        ));
    }
    let class = if limit > 0 && status.disk_used >= limit {
        "warn"
    } else {
        ""
    };
    card(
        body,
        class,
        "Storage",
        &bytes_words(status.disk_used),
        &rest,
    );
}

fn render_downloads_card(body: &mut String, status: &Status, settings: &NodeSettings) {
    let limit = settings.download_limit_mb_per_day.saturating_mul(MB);
    let mut rest = if limit == 0 {
        "<p>No daily limit.</p>\n".to_string()
    } else {
        format!(
            "{}<p>of {} a day.</p>\n",
            meter(status.downloaded_today, limit, "Downloads today"),
            bytes_words(limit)
        )
    };
    rest.push_str(&format!(
        "<p class=\"hint\">{} since setup.</p>\n",
        bytes_words(status.downloaded_total)
    ));
    let class = if limit > 0 && status.downloaded_today >= limit {
        "warn"
    } else {
        ""
    };
    card(
        body,
        class,
        "Downloads today",
        &bytes_words(status.downloaded_today),
        &rest,
    );
}

fn render_crawl_card(
    body: &mut String,
    status: &Status,
    settings: &NodeSettings,
    now: u64,
    base: &str,
) {
    let crawling = status.phase == Phase::Ready && status.step == Step::Crawling;
    let (big, mut rest) = if crawling {
        let rest = status
            .progress
            .as_ref()
            .map(|p| {
                format!(
                    "{}<p>{} of {} homepages in this round.</p>\n",
                    meter(p.done, p.total, "Work progress"),
                    group_thousands(p.done),
                    group_thousands(p.total)
                )
            })
            .unwrap_or_default();
        ("Visiting homepages".to_string(), rest)
    } else if status.step == Step::Indexing {
        let meter = status
            .progress
            .as_ref()
            .map(|p| meter(p.done, p.total, "Work progress"))
            .unwrap_or_default();
        (
            "Rebuilding index".to_string(),
            format!("{meter}<p>{}.</p>", escape_html(&status.detail)),
        )
    } else if status.phase != Phase::Ready {
        ("Waiting for setup".to_string(), String::new())
    } else if status.last_error.is_some() {
        (
            "Retrying update".to_string(),
            "<p>The last update failed. Plumb will retry automatically.</p>".to_string(),
        )
    } else if let Some(reason) = &status.paused {
        let resumes = status
            .paused_until
            .map(|until| format!("<p>Resumes {}.</p>\n", time_until(until, now)))
            .unwrap_or_default();
        (
            "Paused".to_string(),
            format!("<p>{}.</p>\n{resumes}", escape_html(reason)),
        )
    } else if status.crawl_left > 0 {
        (
            "Waiting".to_string(),
            format!(
                "<p>{} homepages to visit once setup is done.</p>\n",
                group_thousands(status.crawl_left)
            ),
        )
    } else if let Some(next) = status.next_refresh {
        (
            "Up to date".to_string(),
            format!("<p>Next round {}.</p>\n", time_until(next, now)),
        )
    } else {
        ("Up to date".to_string(), String::new())
    };
    rest.push_str(&format!(
        "<p class=\"hint\">{} homepages visited since setup.</p>\n",
        group_thousands(status.homepages_visited)
    ));
    rest.push_str("<div class=\"btns\">");
    // Starts the next round now; while one runs, or crawling is paused,
    // there is nothing to start.
    if status.phase == Phase::Ready && !busy(status) && status.paused.is_none() {
        rest.push_str(&format!(
            "<form method=\"post\" action=\"{base}/refresh\">\
             <button type=\"submit\" class=\"alt\">Update now</button></form>\n"
        ));
    }
    rest.push_str(&pause_buttons(status, settings, base));
    rest.push_str("</div>");
    card(body, "", "Crawling", &big, &rest);
}

fn render_meaning_card(
    body: &mut String,
    status: &Status,
    active: &FeatureSettings,
    now: u64,
    base: &str,
) {
    let headline = match status.meaning_sites {
        Some(n) => format!("{} sites ready", group_thousands(n)),
        None if active.search_by_meaning => "Preparing".into(),
        None => "Off".into(),
    };
    let rest = match &status.meaning_work {
        Some(work) => {
            let mut rest = String::new();
            if let Some(p) = &work.progress {
                rest.push_str(&meter(p.done, p.total, "Work progress"));
            }
            rest.push_str(&format!("<p>{}", escape_html(&work.detail)));
            if let Some(p) = &work.progress {
                let about = if p.unit == "MB" { "about " } else { "" };
                rest.push_str(&format!(
                    ": {} of {about}{} {}",
                    group_thousands(p.done),
                    group_thousands(p.total),
                    escape_html(&p.unit)
                ));
            }
            rest.push_str(".</p>");
            if let Some(err) = &work.error {
                let retry = err
                    .retry_at
                    .map(|at| format!(" Trying again {}.", time_until(at, now)))
                    .unwrap_or_default();
                rest.push_str(&format!(
                    "<p class=\"hint\">{}{retry}</p>{}",
                    escape_html(&err.message),
                    retry_button(base, "meaning", "Try again now")
                ));
            }
            rest
        }
        None => "<p>Find sites by what they are about, even when they use other words. Turn it on under Search \
                 &amp; browser.</p>"
            .to_owned(),
    };
    let class = if status
        .meaning_work
        .as_ref()
        .is_some_and(|w| w.error.is_some())
    {
        "warn"
    } else {
        ""
    };
    card(body, class, "Search by meaning", &headline, &rest);
}

/// A form with one button that tries `what` again.
fn retry_button(base: &str, what: &str, label: &str) -> String {
    format!(
        "<form method=\"post\" action=\"{base}/retry\"><input type=\"hidden\" name=\"what\" \
         value=\"{what}\"><button type=\"submit\" class=\"alt\">{label}</button></form>"
    )
}

/// Activity log entries, newest first as given.
fn render_log(body: &mut String, entries: &[LogEntry], now: u64) {
    body.push_str("<ol class=\"log\">");
    for entry in entries {
        let (class, label) = match entry.level {
            LogLevel::Info => ("info", ""),
            LogLevel::Warning => ("warning", "<strong>Warning:</strong> "),
            LogLevel::Error => ("error", "<strong>Error:</strong> "),
        };
        let at = chrono::DateTime::from_timestamp(entry.at as i64, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        body.push_str(&format!(
            "<li class=\"{class}\"><time title=\"{at}\">{}</time><span>{label}{}</span></li>",
            escape_html(&time_ago(entry.at, now)),
            escape_html(&entry.message)
        ));
    }
    body.push_str("</ol>");
}

fn render_backups(body: &mut String, backups: &[BackupInfo], now: u64, can_restart: bool) {
    let what: Vec<&str> = backup::FILES.iter().map(|(_, what)| *what).collect();
    body.push_str(&format!(
        "<p class=\"intro\">A backup keeps what cannot be downloaded again: {}. Sites and \
         the index are left out; a restored node rebuilds them. Backups hold this node\u{2019}s \
         keys, so keep them private.</p>\
         <form method=\"post\" action=\"/app/backup\"><button type=\"submit\">Make a backup \
         now</button></form>",
        escape_html(&what.join(", ").to_lowercase())
    ));
    body.push_str("<h2>Saved backups</h2>");
    if backups.is_empty() {
        body.push_str("<p>None yet.</p>");
    } else {
        body.push_str("<ol class=\"log backups\">");
        for b in backups {
            let name = escape_html(&b.name);
            body.push_str(&format!(
                "<li><span><code>{name}</code> <small>{} \u{b7} {} KB</small></span>\
                 <span class=\"btns\"><a class=\"btn alt\" href=\"/app/backups/{name}\" \
                 target=\"_blank\">Download</a><form method=\"post\" \
                 action=\"/app/backups/restore\"><input type=\"hidden\" name=\"name\" \
                 value=\"{name}\"><button type=\"submit\" class=\"alt\">Restore</button>\
                 </form></span></li>",
                escape_html(&time_ago(b.created_at, now)),
                b.bytes.div_ceil(1000)
            ));
        }
        body.push_str("</ol>");
    }
    let restart_hint = if can_restart {
        "then restarts this node automatically to apply the restored settings."
    } else {
        "then needs you to restart the app or container to apply the restored settings."
    };
    body.push_str(&format!(
        "<h2>Restore from a file</h2><form method=\"post\" action=\"/app/restore\" \
         enctype=\"multipart/form-data\"><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required><span>I understand that restore replaces the current keys and settings. Keep a backup of the current keys before continuing.</span></label><label>Backup file <input type=\"file\" \
         name=\"backup\" accept=\".json,application/json\" required></label>\
         <p class=\"hint\">Restoring replaces this node\u{2019}s settings and keys with the \
         backup\u{2019}s, after saving the current ones as a backup marked before-restore, \
         {restart_hint}</p><button type=\"submit\">Restore</button></form>",
    ));
}

/// "Pause for an hour" and "Pause until tomorrow" while background work may
/// run, "Resume" while someone paused it.
fn pause_buttons(status: &Status, settings: &NodeSettings, base: &str) -> String {
    let paused_by_hand = settings.paused_until.is_some_and(|until| {
        status.paused_until == Some(until) && status.paused.as_deref() == Some("Paused by you")
    });
    let buttons = if paused_by_hand {
        "<button type=\"submit\" name=\"until\" value=\"resume\">Resume now</button>"
    } else if status.background_updates && status.paused.is_none() {
        "<button type=\"submit\" name=\"until\" value=\"hour\" class=\"alt\">Pause for an \
         hour</button><button type=\"submit\" name=\"until\" value=\"tomorrow\" \
         class=\"alt\">Pause until tomorrow</button>"
    } else {
        return String::new();
    };
    format!("<form method=\"post\" action=\"{base}/pause\" class=\"btns\">{buttons}</form>\n")
}

fn render_network_card(
    body: &mut String,
    status: &Status,
    active: &FeatureSettings,
    now: u64,
    base: &str,
) {
    let retry = format!(
        "<form method=\"post\" action=\"{base}/network/retry\">\
         <button type=\"submit\" class=\"alt\">Try again now</button></form>"
    );
    let no_bootstrap = if active.bootstrap.is_empty() {
        "<p class=\"hint\">No bootstrap nodes are set, so only Plumb nodes on this \
         network can be found. Turn on \u{201c}Find nodes through plumbsearch.org\u{201d} \
         under Network &amp; privacy.</p>"
    } else {
        ""
    };
    let (class, headline, rest) = match &status.network {
        Some(net) if net.connected_peers > 0 => {
            let n = net.connected_peers;
            let relayed = net
                .peers
                .iter()
                .filter(|p| p.route == plumb_net::Route::Relayed)
                .count();
            let direct = n.saturating_sub(net.nearby_peers + relayed);
            let mut parts = Vec::new();
            for (count, words) in [
                (net.nearby_peers, "on this network"),
                (direct, "over the internet"),
                (relayed, "through a relay"),
            ] {
                if count > 0 {
                    parts.push(format!("{count} {words}"));
                }
            }
            let reach = if net.nat == "public" {
                "Other nodes can reach this one directly."
            } else if !net.relays.is_empty() {
                "Other nodes reach this one through a relay, so no port needs opening."
            } else {
                "Other nodes cannot reach this one yet; it still searches and shares through \
                 the connections it opens."
            };
            (
                "ready",
                format!("{n} {}", if n == 1 { "node" } else { "nodes" }),
                format!(
                    "<p>Connected: {}.</p><p>Searches ask {}: {} connected{}.</p>\
                     <p class=\"hint\">{reach} Shared crawl batches: {} received, {} \
                     published.</p>",
                    parts.join(", "),
                    net.search_scope.label().to_lowercase(),
                    net.search_peers,
                    match net.search_scope {
                        plumb_net::SearchScope::FriendsOfFriends =>
                            format!(", {} known through trusted nodes", net.friends_of_friends),
                        _ => String::new(),
                    },
                    net.batches_received,
                    net.batches_published
                ),
            )
        }
        Some(net) => match &net.problem {
            Some(problem) => (
                "warn",
                "Can\u{2019}t connect".to_owned(),
                format!(
                    "<p>{}</p><p class=\"hint\">Last tried {}. Nodes on this network are \
                     still found without it.</p>{no_bootstrap}{retry}",
                    escape_html(&problem.message),
                    time_ago(problem.at, now)
                ),
            ),
            None => {
                let since = net
                    .alone_since
                    .map(|at| format!(" Looking since {}.", time_ago(at, now)))
                    .unwrap_or_default();
                (
                    "limited",
                    "Looking for nodes".to_owned(),
                    format!(
                        "<p>Asking the bootstrap nodes for other Plumb nodes, and looking on \
                         this network.{since}</p>{no_bootstrap}{retry}"
                    ),
                )
            }
        },
        None if active.network => (
            "",
            "Starting".to_owned(),
            "<p>The Plumb network is enabled and is starting.</p>".to_owned(),
        ),
        None => (
            "",
            "Off".to_owned(),
            "<p>This node searches its own index. Turn on \u{201c}Join the Plumb \
             network\u{201d} under Network &amp; privacy to share crawls with others.</p>"
                .to_owned(),
        ),
    };
    card(body, class, "Plumb network", &headline, &rest);
}

/// The nodes this node is connected to, and how it reaches each.
fn render_peers(body: &mut String, status: &Status) {
    let Some(net) = &status.network else {
        return;
    };
    if net.peers.is_empty() {
        return;
    }
    let mut rows = String::new();
    for peer in &net.peers {
        let id = &peer.peer_id;
        // The end of a peer id tells nodes apart; the start is the same
        // for every Ed25519 key.
        let short = id.get(id.len().saturating_sub(8)..).unwrap_or(id);
        let route = match peer.route {
            plumb_net::Route::Nearby => "On this network",
            plumb_net::Route::Direct => "Over the internet",
            plumb_net::Route::Relayed => "Through a relay",
        };
        let mut roles = Vec::new();
        if peer.bootstrap {
            roles.push("bootstrap node");
        }
        if peer.relay {
            roles.push("relays for this node");
        }
        let roles = if roles.is_empty() {
            String::new()
        } else {
            format!(" · {}", roles.join(" · "))
        };
        rows.push_str(&format!(
            "<dt title=\"{}\"><code>\u{2026}{}</code></dt><dd>{route}{roles}</dd>",
            escape_html(id),
            escape_html(short)
        ));
    }
    let more = net.connected_peers.saturating_sub(net.peers.len());
    let more = if more > 0 {
        format!("<p class=\"hint\">And {more} more.</p>")
    } else {
        String::new()
    };
    card(
        body,
        "search",
        "Connected nodes",
        &format!("{} connected", net.connected_peers),
        &format!("<dl>{rows}</dl>{more}"),
    );
}

fn render_network_details(body: &mut String, status: &Status) {
    let Some(net) = &status.network else {
        return;
    };
    if !net.listening.is_empty() {
        card(
            body,
            "search",
            "Listening addresses",
            "Network connections",
            &format!(
                "<p class=\"msg\">{}</p>",
                escape_html(&net.listening.join("\n"))
            ),
        );
    }
    let agreement = &net.agreement;
    card(body, "", "Crawl agreement", &format!("{} sites confirmed", group_thousands(agreement.confirmed_sites as u64)), &format!("<p>{} waiting for agreement · {} disputed.</p><p class=\"hint\">{} trusted crawlers · {} distrusted.</p>", agreement.pending_sites, agreement.disputed_sites, agreement.vouched_crawlers, agreement.distrusted_crawlers));
    render_credits(body, &net.credits);
    card(
        body,
        "",
        "Popularity sharing",
        &format!("{} reports sent", net.reports_sent),
        &format!(
            "<p>{} reports held · {} popular picks.</p>",
            net.reports_held, net.popular_picks
        ),
    );
    card(body, "", "Relay activity", &format!("{} requests relayed", net.requests_relayed), &format!("<p>{} bucket requests answered.</p><p class=\"hint\">Reachability: {}. {} relay reservations.</p>", net.buckets_served, escape_html(&net.nat), net.relays.len()));
    card(
        body,
        "search",
        "Node identity",
        "Node id",
        &format!(
            "<p class=\"msg\">{}</p><p class=\"msg\">{}</p>",
            escape_html(&net.peer_id),
            if net.reachable_at.is_empty() {
                "No public address yet".into()
            } else {
                escape_html(&net.reachable_at.join("\n"))
            }
        ),
    );
}

/// One line of the setup steps: done, under way, waiting or to do.
fn step_item(body: &mut String, state: &str, text: &str, note: Option<String>) {
    let icon = match state {
        "done" => "&#10003;",
        "now" => "&#9679;",
        "wait" => "&#8987;",
        _ => "&#9675;",
    };
    let note = note
        .map(|note| format!("<small>{}</small>", escape_html(&note)))
        .unwrap_or_default();
    body.push_str(&format!(
        "<li class=\"{state}\"><span class=\"i\" aria-hidden=\"true\">{icon}</span>\
         <span>{}{note}</span></li>\n",
        escape_html(text)
    ));
}

fn render_steps(body: &mut String, status: &Status, now: u64) {
    body.push_str("<h2>Setup</h2>\n<ol class=\"steps\">\n");
    let ready = status.phase == Phase::Ready;
    let setting_up_step = |steps: &[Step]| !ready && steps.contains(&status.step);

    let list = if setting_up_step(&[Step::Downloading, Step::Starting, Step::Retrying]) {
        "now"
    } else {
        "done"
    };
    step_item(body, list, "Get a list of popular sites", None);
    let first = if ready {
        "done"
    } else if setting_up_step(&[Step::Ingesting, Step::Indexing]) {
        "now"
    } else {
        "todo"
    };
    step_item(body, first, "Build a first index: limited search", None);

    let (seed, note) = if !ready {
        ("todo", None)
    } else if !status.wikidata_missing {
        ("done", None)
    } else if let Some(err) = &status.wikidata_error {
        let when = err
            .retry_at
            .map(|at| format!(" Plumb will try again {}.", time_until(at, now)))
            .unwrap_or_default();
        ("wait", Some(format!("Could not download it yet.{when}")))
    } else {
        ("now", Some(status.detail.clone()))
    };
    step_item(
        body,
        seed,
        "Add Wikidata's official websites and more rankings: full search",
        note,
    );

    let crawling = ready && status.step == Step::Crawling;
    let (crawl, note) = if crawling {
        let note = status.progress.as_ref().map(|p| {
            format!(
                "{} of {} homepages",
                group_thousands(p.done),
                group_thousands(p.total)
            )
        });
        ("now", note)
    } else if let Some(reason) = status.paused.as_ref().filter(|_| status.crawl_left > 0) {
        ("wait", Some(format!("{reason}.")))
    } else if status.last_refresh.is_some() && status.crawl_left == 0 {
        ("done", None)
    } else {
        ("todo", None)
    };
    step_item(
        body,
        crawl,
        "Visit homepages to learn what each site is about",
        note,
    );
    body.push_str("</ol>\n");
}

fn render_browser(body: &mut String, origin: &str) {
    let origin = escape_html(origin);
    body.push_str(&format!(
        "<h2>Search from your browser</h2>\n\
         <p>Plumb searches in your web browser. Add it to Firefox with the button above, or \
         add a search engine in another browser's settings with this address:</p>\n\
         <p><code>{origin}/search?q=%s</code></p>\n\
         <p class=\"hint\">Your country and other search options are on the search page.</p>\n"
    ));
}

pub(super) fn render_about(body: &mut String, status: &Status, data_dir: Option<&Path>, now: u64) {
    body.push_str("<dl>\n");
    let mut row = |name: &str, value: String| {
        body.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>\n",
            escape_html(name),
            escape_html(&value)
        ));
    };
    row("Version", status.version.clone());
    row("Items indexed", items_in_words(status));
    if let Some(last) = status.last_refresh {
        row("Last updated", time_ago(last, now));
    }
    if let Some(dir) = data_dir {
        row("Data folder", dir.display().to_string());
    }
    body.push_str("</dl>\n");
}
