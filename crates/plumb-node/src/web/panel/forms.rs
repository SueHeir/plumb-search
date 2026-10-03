//! Recoverable form errors. These pages retain submitted values and post
//! back to the selected node, without changing settings or storing drafts.
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use super::{
    escape_html, page_with_head, panel_headers, FeaturesForm, SettingsForm, LAYOUT_STYLE,
    PANEL_REFERRER_POLICY, PANEL_STYLE,
};

pub(in crate::web) fn settings_error(
    status: StatusCode,
    form: &SettingsForm,
    base: &str,
    message: &str,
) -> Response {
    let mut fields = checkbox(
        "background_updates",
        "Keep the index up to date in the background",
        form.background_updates.is_some(),
    );
    for (name, label, value, hint) in [
        (
            "download_limit_mb_per_day",
            "Download limit (MB per UTC day)",
            &form.download_limit_mb_per_day,
            "Use a whole number. Empty or 0 means no daily limit.",
        ),
        (
            "storage_limit_mb",
            "Storage limit (MB)",
            &form.storage_limit_mb,
            "Use a whole number. Empty or 0 means no storage limit.",
        ),
    ] {
        let invalid = if super::parse_limit(value).is_none() {
            " aria-invalid=\"true\""
        } else {
            ""
        };
        // A number input silently discards invalid text, hiding what needs fixing.
        fields.push_str(&format!("<label for=\"{name}\">{label}</label><input id=\"{name}\" name=\"{name}\" type=\"text\" inputmode=\"numeric\" value=\"{}\" aria-describedby=\"{name}-help\"{invalid}><p id=\"{name}-help\" class=\"hint\">{hint}</p>", escape_html(value)));
    }
    retry_page(
        status,
        base,
        "resources",
        "settings",
        "Resource settings",
        message,
        &fields,
    )
}

pub(in crate::web) fn features_error(
    status: StatusCode,
    form: &FeaturesForm,
    base: &str,
    message: &str,
) -> Response {
    let section = if form.section == "search" {
        "search"
    } else {
        "network"
    };
    let mut fields = format!("<input type=\"hidden\" name=\"section\" value=\"{section}\">");
    if section == "search" {
        fields.push_str(&checkbox(
            "search_by_meaning",
            "Search by meaning",
            form.search_by_meaning.is_some(),
        ));
        fields.push_str(&checkbox(
            "private_search",
            "Private browser search",
            form.private_search.is_some(),
        ));
    } else {
        fields.push_str(&checkbox(
            "network",
            "Join the Plumb network",
            form.network.is_some(),
        ));
        fields.push_str(&checkbox(
            "share_popularity",
            "Share anonymous popularity (requires the Plumb network)",
            form.share_popularity.is_some(),
        ));
        fields.push_str(&format!("<label for=\"bootstrap\">Bootstrap nodes</label><p id=\"bootstrap-help\" class=\"hint\">One multiaddress per line. Leave empty to use nearby-node discovery.</p><textarea id=\"bootstrap\" name=\"bootstrap\" spellcheck=\"false\" aria-describedby=\"bootstrap-help\">{}</textarea>", escape_html(&form.bootstrap)));
    }
    retry_page(
        status,
        base,
        section,
        "features",
        "Feature settings",
        message,
        &fields,
    )
}

fn checkbox(name: &str, label: &str, checked: bool) -> String {
    format!("<label class=\"recovery-toggle\"><input type=\"checkbox\" name=\"{name}\" value=\"1\"{}><span>{label}</span></label>", if checked { " checked" } else { "" })
}

fn retry_page(
    status: StatusCode,
    base: &str,
    section: &str,
    action: &str,
    title: &str,
    message: &str,
    fields: &str,
) -> Response {
    let base = escape_html(base);
    let heading = if status == StatusCode::BAD_REQUEST {
        "Check your settings"
    } else {
        "Could not confirm the save"
    };
    let body = format!("<main class=\"wrap node-panel form-recovery\"><a href=\"{base}?section={section}\">Back to the panel</a><h1>{title}</h1><div class=\"err\" role=\"alert\"><h2>{heading}</h2><p>{}</p></div><p>Your entries are kept below. Correct them and save again, or return to the panel to review the node’s saved settings.</p><form method=\"post\" action=\"{base}/{action}\">{fields}<div class=\"btns\"><button type=\"submit\">Save again</button><a class=\"btn alt\" href=\"{base}?section={section}\">Cancel</a></div></form></main>", escape_html(message));
    let head = format!("<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\"><style>{PANEL_STYLE}{LAYOUT_STYLE}.form-recovery h1{{margin-top:1rem}}.form-recovery input[aria-invalid]{{border-color:var(--err)}}.form-recovery form input[type=text]{{width:100%}}.form-recovery .btns{{align-items:baseline}}.form-recovery .recovery-toggle{{display:flex;flex-wrap:nowrap;align-items:baseline;gap:.6rem}}.form-recovery .recovery-toggle input{{flex-shrink:0}}</style>");
    (
        status,
        panel_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(page_with_head(title, &head, &body)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn text(response: Response) -> String {
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::REFERRER_POLICY], "same-origin");
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn resource_errors_preserve_input_and_selected_node_without_script() {
        for base in ["/app", "/app/nodes/test-node"] {
            let form = SettingsForm {
                background_updates: Some("1".into()),
                download_limit_mb_per_day: "250".into(),
                storage_limit_mb: "\"><script>alert(1)</script>".into(),
            };
            let body = text(settings_error(
                StatusCode::BAD_REQUEST,
                &form,
                base,
                "Invalid storage limit",
            ))
            .await;
            assert!(body.contains("name=\"background_updates\" value=\"1\" checked"));
            assert!(body.contains("value=\"250\""));
            assert!(body.contains("&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
            assert!(body.contains("aria-invalid=\"true\""));
            assert!(body.contains(&format!("action=\"{base}/settings\"")));
            assert!(body.contains(&format!("href=\"{base}?section=resources\"")));
            assert!(!body.contains("http-equiv=\"refresh\""));
            assert!(!body.contains("<script>"));
        }
    }

    #[tokio::test]
    async fn feature_errors_preserve_section_checkboxes_and_raw_addresses() {
        let mut form = FeaturesForm {
            section: "network".into(),
            network: Some("1".into()),
            bootstrap: "/ip4/127.0.0.1/tcp/4001\nnot-an-address\n".into(),
            ..Default::default()
        };
        let body = text(features_error(
            StatusCode::BAD_REQUEST,
            &form,
            "/app/nodes/homelab",
            "Invalid address",
        ))
        .await;
        assert!(body.contains(&format!(">{}</textarea>", form.bootstrap)));
        assert!(body.contains("action=\"/app/nodes/homelab/features\""));
        assert!(body.contains("name=\"network\" value=\"1\" checked"));
        assert!(!body.contains("name=\"share_popularity\" value=\"1\" checked"));
        form.section = "search".into();
        form.private_search = Some("1".into());
        let body = text(features_error(
            StatusCode::BAD_GATEWAY,
            &form,
            "/app",
            "Could not confirm save",
        ))
        .await;
        assert!(body.contains("name=\"section\" value=\"search\""));
        assert!(body.contains("name=\"private_search\" value=\"1\" checked"));
        assert!(!body.contains("name=\"bootstrap\""));
    }
}
