//! Process limits a crawl needs.

use tracing::{debug, warn};

/// Open files the crawler wants: a socket per homepage in flight plus
/// robots.txt fetches, redirects, the records journal and the index.
/// macOS refuses a soft limit above its per-process maximum (OPEN_MAX,
/// 10,240) even when the hard limit says unlimited.
#[cfg(not(target_os = "macos"))]
const WANTED_OPEN_FILES: u64 = 65_536;
#[cfg(target_os = "macos")]
const WANTED_OPEN_FILES: u64 = 10_240;

/// Raises the soft limit on open files towards [`WANTED_OPEN_FILES`], as far
/// as the hard limit allows. Services and job queues often start programs
/// with a soft limit of 1,024; a crawl with a few hundred homepages in flight
/// then runs out of sockets, nearly every site fails, and the batch looks
/// offline. Raising the soft limit up to the hard one needs no privileges.
pub(crate) fn raise_open_file_limit() {
    #[cfg(unix)]
    {
        use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
        let limit = getrlimit(Resource::Nofile);
        let Some(current) = limit.current else {
            return; // Unlimited already.
        };
        let wanted = limit
            .maximum
            .map_or(WANTED_OPEN_FILES, |max| max.min(WANTED_OPEN_FILES));
        if current >= wanted {
            return;
        }
        let raised = Rlimit {
            current: Some(wanted),
            maximum: limit.maximum,
        };
        match setrlimit(Resource::Nofile, raised) {
            Ok(()) => debug!("raised the open file limit from {current} to {wanted}"),
            Err(err) => warn!(
                "cannot raise the open file limit from {current} to {wanted} ({err}); \
                 crawls with a high --concurrency may fail as if offline"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn raises_the_soft_limit_up_to_the_hard_one() {
        use rustix::process::{getrlimit, Resource};
        super::raise_open_file_limit();
        let limit = getrlimit(Resource::Nofile);
        let wanted = limit.maximum.map_or(super::WANTED_OPEN_FILES, |max| {
            max.min(super::WANTED_OPEN_FILES)
        });
        assert!(
            limit.current.is_none_or(|current| current >= wanted),
            "{limit:?}"
        );
    }
}
