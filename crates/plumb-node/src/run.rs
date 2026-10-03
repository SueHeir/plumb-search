//! `plumb run`: a long-running node (see [`crate::node`]) that stops cleanly
//! on Ctrl-C, or on SIGTERM as `docker stop` sends it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use tracing::warn;

use crate::cli::{Profile, RunArgs};
use crate::node::{self, NodeConfig};
use crate::web::shutdown_signal;

/// How long to wait for stray blocking work once the node has stopped.
const RUNTIME_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(args: RunArgs) -> Result<()> {
    let config = node_config(args);
    let runtime = crate::runtime()?;
    let result = runtime.block_on(async move {
        let node = node::start(config).await?;
        println!("{}", listening_message(node.addr()));
        shutdown_signal().await;
        tokio::select! {
            stopped = node.shutdown() => stopped,
            () = shutdown_signal() => {
                // Files on disk are always whole, so stopping now is safe.
                warn!("stopping right away; unfinished work goes on at the next start");
                std::process::exit(130);
            }
        }
    });
    runtime.shutdown_timeout(RUNTIME_STOP_TIMEOUT);
    result
}

/// The node settings for the arguments: the profile's defaults, with the
/// flags given on top.
fn node_config(args: RunArgs) -> NodeConfig {
    let mut config = match args.profile {
        Profile::Server => NodeConfig::server(args.data),
        Profile::Desktop => NodeConfig::desktop(args.data),
    };
    config.bind = args.bind;
    if let Some(sites) = args.sites {
        config.sites = sites;
    }
    if let Some(homepages) = args.initial_crawl {
        config.initial_crawl = homepages;
    }
    if args.no_refresh {
        config.refresh_every = None;
    } else if let Some(every) = args.refresh_hours {
        config.refresh_every = Some(every);
    }
    if let Some(homepages) = args.crawl_per_refresh {
        config.crawl_per_refresh = homepages;
    }
    if args.cc_release.is_some() {
        config.cc_release = args.cc_release;
    }
    if args.alpha.is_some() {
        config.alpha = args.alpha;
    }
    config.country = args.country;
    config
}

/// Where to point a browser. An address that listens on every interface
/// (`0.0.0.0`, `::`) is reached on this machine through loopback.
fn listening_message(addr: SocketAddr) -> String {
    let local = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        _ => {
            return format!("Plumb Search is running at http://{addr}/ (Ctrl-C to stop)");
        }
    };
    format!(
        "Plumb Search is listening on {addr}; on this machine, open http://{}/ (Ctrl-C to stop)",
        SocketAddr::new(local, addr.port())
    )
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};

    fn config(args: &[&str]) -> NodeConfig {
        let cli = Cli::try_parse_from(["plumb", "run"].iter().chain(args)).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("not run");
        };
        node_config(args)
    }

    #[test]
    fn profiles_and_overrides() {
        let server = config(&["--data", "/data", "--bind", "0.0.0.0:8080"]);
        let mut expected = NodeConfig::server("/data".into());
        expected.bind = "0.0.0.0:8080".parse().unwrap();
        assert_eq!(server, expected);

        // --bind keeps its default even for the desktop profile, whose free
        // port only suits an app that reads it back.
        let desktop = config(&["--data", "d", "--profile", "desktop"]);
        let mut expected = NodeConfig::desktop("d".into());
        expected.bind = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(desktop, expected);

        let tuned = config(&[
            "--data",
            "d",
            "--sites",
            "5000",
            "--initial-crawl",
            "0",
            "--refresh-hours",
            "2",
            "--crawl-per-refresh",
            "50",
            "--cc-release",
            "cc-main-2025-26-nov-dec-jan",
            "--alpha",
            "0.2",
        ]);
        assert_eq!(tuned.sites, 5_000);
        assert_eq!(tuned.initial_crawl, 0);
        assert_eq!(tuned.refresh_every, Some(Duration::from_secs(7_200)));
        assert_eq!(tuned.crawl_per_refresh, 50);
        assert_eq!(
            tuned.cc_release.as_deref(),
            Some("cc-main-2025-26-nov-dec-jan")
        );
        assert_eq!(tuned.alpha, Some(0.2));

        let off = config(&["--data", "d", "--no-refresh"]);
        assert_eq!(off.refresh_every, None);
        assert_eq!(off.crawl_per_refresh, 5_000);
    }

    #[test]
    fn says_where_to_browse() {
        assert_eq!(
            listening_message("127.0.0.1:8080".parse().unwrap()),
            "Plumb Search is running at http://127.0.0.1:8080/ (Ctrl-C to stop)"
        );
        assert_eq!(
            listening_message("0.0.0.0:8080".parse().unwrap()),
            "Plumb Search is listening on 0.0.0.0:8080; on this machine, \
             open http://127.0.0.1:8080/ (Ctrl-C to stop)"
        );
        assert!(listening_message("[::]:80".parse().unwrap()).contains("http://[::1]:80/"));
    }
}
