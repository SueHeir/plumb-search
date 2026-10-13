//! `report_finding`, and the findings and leads a search lists.

use super::*;

/// Leads listed with a search's results.
const MAX_LEADS_LISTED: usize = 3;

/// What `report_finding` shares with other nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Share {
    /// Nothing: the finding stays on this node.
    No,
    /// The page, why it helped and the search's words as numbers.
    Page,
    /// That and the search as typed.
    PageAndQuery,
}

impl Mcp {
    /// `report_finding`: keeps what an agent found, unless its page is a
    /// look-alike of another site, and shares it as a lead when asked to and
    /// the node allows it.
    #[allow(clippy::too_many_arguments)]
    pub fn report_finding(
        &self,
        query: &str,
        url: &str,
        why: &str,
        answer: &str,
        task: Option<&str>,
        share: Share,
        options: &SearchOptions,
    ) -> Result<Value> {
        let Some(findings) = &self.findings else {
            bail!("this node keeps no findings");
        };
        let finding = Finding::new(query, url, why, answer, task, plumb_core::now_unix())?;
        let check = self.check_lookalike(&finding.url, options)?;
        if check["verdict"] == "lookalike" {
            bail!(
                "{} looks like a look-alike of another site, so it was not kept",
                finding.url
            );
        }
        findings.add(finding.clone())?;
        let mut kept = json!({
            "kept": true,
            "query": finding.query,
            "url": finding.url,
            "findings": findings.len(),
        });
        if share != Share::No {
            kept["shared"] = self.share_finding(&finding, share);
        }
        Ok(kept)
    }

    /// Shares `finding` with other nodes as a lead: what the answer says,
    /// or why it was not shared.
    fn share_finding(&self, finding: &Finding, share: Share) -> Value {
        let net = match self.net() {
            Some(net) if self.shares() => net,
            _ => {
                return json!({
                    "shared": false,
                    "why_not": "this node does not share findings; it was kept on this node only \
                         (a node run with --share-findings in the Plumb network can share them)",
                })
            }
        };
        if self.denies(&finding.url) {
            return json!({
                "shared": false,
                "why_not": "the denylist this node applies takes this page off, so it is not \
                     passed on; it was kept on this node only",
            });
        }
        let draft = plumb_net::leads::LeadDraft {
            keys: crate::findings::lead_keys(&finding.query),
            query: (share == Share::PageAndQuery).then(|| finding.query.clone()),
            url: finding.url.clone(),
            note: finding.why.clone(),
        };
        match net.share_lead_blocking(draft) {
            Ok(lead) => json!({
                "shared": true,
                "url": lead.url,
                "note": lead.note,
                "query": lead.query,
                "node": net.peer_id().to_string(),
                "expires_in_days": lead.expires.saturating_sub(lead.at) / 86_400,
            }),
            Err(err) => json!({ "shared": false, "why_not": format!("{err:#}") }),
        }
    }

    /// The pages other nodes shared for `query`, besides `known` ones and
    /// those the denylist takes off, each checked for look-alikes; at most
    /// [`MAX_LEADS_LISTED`].
    pub(super) fn leads_for(
        &self,
        query: &str,
        known: &[&str],
        now: u64,
        options: &SearchOptions,
    ) -> Vec<Value> {
        let Some(net) = self.net() else {
            return Vec::new();
        };
        let keys = crate::findings::lead_keys(query);
        if keys.topic.is_empty() {
            return Vec::new();
        }
        let found = match net.leads_blocking(keys, MAX_LEADS_LISTED + known.len()) {
            Ok(found) => found,
            Err(err) => {
                tracing::debug!("cannot list leads: {err:#}");
                return Vec::new();
            }
        };
        found
            .into_iter()
            .filter(|lead| !known.contains(&lead.url.as_str()) && !self.denies(&lead.url))
            // A page made to look like another site is no lead.
            .filter(|lead| {
                self.check_lookalike(&lead.url, options)
                    .map_or(true, |check| check["verdict"] != "lookalike")
            })
            .take(MAX_LEADS_LISTED)
            .map(|lead| {
                let newest = lead.reporters.first();
                let reporters: Vec<Value> = lead
                    .reporters
                    .iter()
                    .map(|by| {
                        json!({
                            "node": by.peer_id,
                            "relation": by.relation,
                            "why": by.note,
                            "query": by.query,
                            "reported": crate::web::time_ago(by.at, now),
                            "reported_at": by.at,
                            "expires_at": by.expires,
                        })
                    })
                    .collect();
                json!({
                    "url": lead.url,
                    "why": newest.map(|by| by.note.as_str()),
                    "reported": newest.map(|by| crate::web::time_ago(by.at, now)),
                    "verified": false,
                    "reported_by": reporters,
                })
            })
            .collect()
    }
}
