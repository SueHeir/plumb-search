//! Popularity reports and leads: over the gossip topic and by asking.

use std::sync::Arc;

use anyhow::{Context, Result};
use libp2p::gossipsub;
use libp2p::request_response::{self};
use plumb_core::locks::{Locked, RwLocked};
use plumb_core::now_unix;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::{Answer, Task, MAX_LEAD_LISTS, MAX_LISTS_SERVING, POPULARITY_FILE};
use crate::leads::{Inserted, Lead, LeadDraft, MAX_LISTED_LEADS};
use crate::popularity::{PopularityTable, Report};
use crate::proto::*;
use crate::reports::ReportStore;

impl Task {
    /// Counts the reports held, off the swarm task, and saves the table.
    pub(super) fn count_reports(&self, reply: Option<oneshot::Sender<Arc<PopularityTable>>>) {
        let reports = self.reports.clone();
        let popularity = self.popularity.clone();
        let path = self.config.dir.join(POPULARITY_FILE);
        let storage = self.storage.clone();
        let tx = self.answers_tx.clone();
        tokio::task::spawn_blocking(move || {
            // Counted with the store let go: STAR counting takes a while.
            let held = reports.locked().snapshots(now_unix());
            let table = crate::reports::count(held);
            let saved = serde_json::to_vec_pretty(&table)
                .context("encoding the popularity table")
                .and_then(|bytes| {
                    plumb_core::storage::write_atomic(
                        &path,
                        &path.with_extension("tmp"),
                        &bytes,
                        storage.as_ref(),
                        None,
                    )
                    .context("saving the popularity table")
                });
            if let Err(err) = saved {
                warn!("cannot save the popularity table: {err:#}");
            }
            let picks = table.picks.len();
            info!("counted the popularity reports: {picks} picks");
            let table = Arc::new(table);
            *popularity.write_locked() = table.clone();
            if let Some(reply) = reply {
                let _ = reply.send(table);
            }
            let _ = tx.send(Answer::Popularity(picks));
        });
    }

    pub(super) fn lock_reports(&self) -> std::sync::MutexGuard<'_, ReportStore> {
        self.reports.locked()
    }

    /// Keeps a report if it checks out and is new. Returns `None` for one
    /// that does not check out, else whether it was new.
    fn take_report(&mut self, report: &Report) -> Option<bool> {
        if let Err(err) = report.check(now_unix()) {
            debug!("refused a report: {err:#}");
            return None;
        }
        let new = match self.lock_reports().insert(report) {
            Ok(new) => new,
            Err(err) => {
                warn!("cannot keep a report: {err:#}");
                false
            }
        };
        if new {
            self.recount = true;
        }
        Some(new)
    }

    /// A report handed to this node, straight or sealed through a relay:
    /// keeps it and, if new, passes it on. Returns whether it was taken.
    pub(super) fn take_submitted(&mut self, report: &Report) -> bool {
        let taken = self.take_report(report) == Some(true);
        if taken {
            // Nodes that miss it get it when they next ask for the week's
            // reports.
            let data = serde_json::to_vec(report).expect("reports encode");
            let topic = self.report_topic.clone();
            if let Err(err) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                debug!("cannot pass a report on yet: {err}");
            }
        }
        taken
    }

    /// A report passed on over gossip.
    pub(super) fn on_gossip_report(&mut self, data: &[u8]) -> gossipsub::MessageAcceptance {
        let Ok(report) = serde_json::from_slice::<Report>(data) else {
            return gossipsub::MessageAcceptance::Reject;
        };
        match self.take_report(&report) {
            Some(_) => gossipsub::MessageAcceptance::Accept,
            None => gossipsub::MessageAcceptance::Reject,
        }
    }

    pub(super) fn on_report_event(
        &mut self,
        event: request_response::Event<ReportRequest, ReportResponse>,
    ) {
        match event {
            request_response::Event::Message {
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => match request {
                ReportRequest::Submit(report) => {
                    let taken = self.take_submitted(&report);
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .reports
                        .send_response(channel, ReportResponse::Taken(taken));
                }
                ReportRequest::List { epoch } => {
                    if self.lists_serving >= MAX_LISTS_SERVING {
                        let _ = self
                            .swarm
                            .behaviour_mut()
                            .reports
                            .send_response(channel, ReportResponse::Reports(Vec::new()));
                        return;
                    }
                    self.lists_serving += 1;
                    let reports = self.reports.clone();
                    let tx = self.answers_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        // Copied out with the store let go.
                        let held = reports.locked().snapshot(epoch);
                        let list = crate::reports::list(held, MAX_LISTED_REPORTS);
                        let _ = tx.send(Answer::Report(channel, ReportResponse::Reports(list)));
                    });
                }
            },
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => {
                self.report_lists.remove(&request_id);
                let ReportResponse::Reports(reports) = response else {
                    return;
                };
                let mut new = 0;
                for report in reports.iter().take(MAX_LISTED_REPORTS) {
                    if self.take_report(report) == Some(true) {
                        new += 1;
                    }
                }
                if new > 0 {
                    info!("caught up on {new} popularity reports from {peer}");
                }
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                self.report_lists.remove(&request_id);
                debug!("report request to {peer} failed: {error}");
            }
            _ => {}
        }
    }

    /// Signs and keeps a lead of this node's, and passes it on.
    pub(super) fn share_lead(&mut self, draft: LeadDraft) -> Result<Lead> {
        let now = now_unix();
        let lead = Lead::sign(&self.key, draft, now)?;
        let me = *self.swarm.local_peer_id();
        if self.leads.insert(lead.clone(), me, now)? == Inserted::TooMany {
            anyhow::bail!(
                "this node shared {} leads in the last day; share more tomorrow",
                crate::leads::MAX_LEADS_PER_DAY
            );
        }
        self.leads_shared += 1;
        info!("shared a lead: {}", lead.url);
        self.announce_lead(lead.clone());
        Ok(lead)
    }

    /// Passes a lead of this node's on over gossip, or again once a node
    /// that takes leads is there; others ask for it when they next list
    /// this node's leads.
    pub(super) fn announce_lead(&mut self, lead: Lead) {
        if lead.expired(now_unix()) {
            return;
        }
        let data = serde_json::to_vec(&lead).expect("leads encode");
        let topic = self.lead_topic.clone();
        match self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
            Ok(_) => {}
            Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {
                debug!("no node to pass a lead on to yet; trying again later");
                self.unannounced_leads.push(lead);
            }
            Err(err) => debug!("cannot pass a lead on: {err}"),
        }
    }

    /// Keeps another node's lead if it checks out. `None` for one that does
    /// not, else what the store did with it.
    fn take_lead(&mut self, lead: Lead) -> Option<Inserted> {
        let now = now_unix();
        let reporter = match lead.check(now) {
            Ok(reporter) => reporter,
            Err(err) => {
                debug!("refused a lead: {err:#}");
                return None;
            }
        };
        if self.source.denies(&lead.url) {
            debug!("refused a lead the denylist takes off");
            return None;
        }
        match self.leads.insert(lead, reporter, now) {
            Ok(inserted) => Some(inserted),
            Err(err) => {
                warn!("cannot keep a lead: {err:#}");
                Some(Inserted::Known)
            }
        }
    }

    /// A lead passed on over gossip: passed on further only when taken, so
    /// copies, and leads past a node's daily limit, stop here.
    pub(super) fn on_gossip_lead(&mut self, data: &[u8]) -> gossipsub::MessageAcceptance {
        let Ok(lead) = serde_json::from_slice::<Lead>(data) else {
            return gossipsub::MessageAcceptance::Reject;
        };
        // It may have expired on the way; and a node with another
        // denylist, or none, did nothing wrong in passing it on.
        if lead.expired(now_unix()) || self.source.denies(&lead.url) {
            return gossipsub::MessageAcceptance::Ignore;
        }
        match self.take_lead(lead) {
            Some(Inserted::Taken) => gossipsub::MessageAcceptance::Accept,
            Some(Inserted::Known | Inserted::TooMany) => gossipsub::MessageAcceptance::Ignore,
            None => gossipsub::MessageAcceptance::Reject,
        }
    }

    /// Asks connected nodes not asked yet for the newest leads they hold,
    /// [`MAX_LEAD_LISTS`] at a time; the rest are asked as answers come.
    pub(super) fn list_more_leads(&mut self) {
        while self.lead_lists.len() < MAX_LEAD_LISTS {
            let Some(peer) = self
                .lead_peers
                .iter()
                .find(|peer| !self.lead_listing.contains(*peer))
                .copied()
            else {
                return;
            };
            self.lead_listing.insert(peer);
            let id = self
                .swarm
                .behaviour_mut()
                .leads
                .send_request(&peer, LeadRequest {});
            self.lead_lists.insert(id);
        }
    }

    /// Any node may ask for the leads held: they were shared to be found.
    pub(super) fn on_lead_event(
        &mut self,
        event: request_response::Event<LeadRequest, LeadResponse>,
    ) {
        match event {
            request_response::Event::Message {
                message: request_response::Message::Request { channel, .. },
                ..
            } => {
                let mut leads = self.leads.list(MAX_LISTED_LEADS);
                leads.retain(|lead| !self.source.denies(&lead.url));
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .leads
                    .send_response(channel, LeadResponse { leads });
            }
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => {
                self.lead_lists.remove(&request_id);
                let mut new = 0;
                for lead in response.leads.into_iter().take(MAX_LISTED_LEADS) {
                    if self.take_lead(lead) == Some(Inserted::Taken) {
                        new += 1;
                    }
                }
                if new > 0 {
                    info!("caught up on {new} leads from {peer}");
                }
                self.list_more_leads();
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                self.lead_lists.remove(&request_id);
                debug!("asking {peer} for leads failed: {error}");
                self.list_more_leads();
            }
            _ => {}
        }
    }
}
