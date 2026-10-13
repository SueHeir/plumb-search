//! Sites left out of the results unless the query names them: they are no
//! use to anyone searching for something else, and some are unsafe to
//! list.
//!
//! - Sites on names reserved for tests (uniteddolls.test).
//! - Online pill shops named after a drug ("ibuprofen dosage" found
//!   celebrex365o24.com, [`health::source_quality`]).
//! - Little-known sites whose homepage is only a sign-in form
//!   ([`is_sign_in_page`]). Meaning puts every sign-in page close to every
//!   "<something> login" search, so "Fidelity NetBenefits login" listed
//!   "DataHubNet - Login" on datahubnet.online and "hulu login" "Login -
//!   iShara.TV" on yablo.tv: a stranger's password box for a searcher
//!   about to type a password. One for someone else's product ("Outlook
//!   Web App" on bpl.net for "microsoft outlook",
//!   [`plumb_core::is_sign_in_portal`]) goes too. live.com, titled
//!   "Outlook", is Microsoft's.
//! - In a search to sign in, little-known sites with no description that
//!   only meaning found. Once the sign-in pages went, "My CardGate" on
//!   curopayments.net took their place for "chase login", "coinbase login"
//!   and "amex login", and three untitled strangers for "schwab login".
//!   Nothing ties them to the site the searcher wants to sign in to.

use plumb_core::{asks_to_sign_in, is_sign_in_page, names_label};

use crate::names::is_reserved_name;
use crate::{health, Hit, WELL_KNOWN_LINK_SCORE};

/// Whether `hit` stays out of the results of `query`.
pub(crate) fn left_out(query: &str, hit: &Hit) -> bool {
    if is_reserved_name(&hit.domain) {
        return true;
    }
    if health::source_quality(
        &hit.domain,
        hit.title.as_deref(),
        hit.description.as_deref(),
        Some(hit.link_score),
    )
    .is_some_and(|quality| quality.informational_penalty())
    {
        return true;
    }
    if hit.link_score >= WELL_KNOWN_LINK_SCORE {
        return false;
    }
    let title = hit.title.as_deref();
    let says_nothing_meaning_found = || {
        hit.description.is_none()
            && hit
                .query_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.lexical_score <= 0.0)
    };
    title.is_some_and(|title| plumb_core::is_sign_in_portal(&hit.domain, title))
        || (!hit.official
            && (is_sign_in_page(&hit.url, title)
                || (asks_to_sign_in(query) && says_nothing_meaning_found()))
            && !names_label(query, &hit.domain))
}
