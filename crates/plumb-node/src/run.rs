//! `plumb run`: a long-running node (see [`crate::node`]) that stops cleanly
//! on Ctrl-C, or on SIGTERM as `docker stop` sends it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use plumb_net::NetConfig;
use tracing::{info, warn};

use crate::cli::{Profile, RunArgs};
use crate::node::{self, NodeConfig};
use crate::web::shutdown_signal;

/// How long to wait for stray blocking work once the node has stopped.
const RUNTIME_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(args: RunArgs) -> Result<()> {
    if args.reseed {
        if node::request_reseed(&args.data)? {
            info!("folding the seed data into the records again once the node is up");
        } else {
            warn!("--reseed: no records yet, so the node sets up from the seed data anyway");
        }
    }
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
    if args.crawl_concurrency.is_some() {
        config.crawl_concurrency = args.crawl_concurrency;
    }
    if args.cc_release.is_some() {
        config.cc_release = args.cc_release;
    }
    if args.alpha.is_some() {
        config.alpha = args.alpha;
    }
    config.country = args.country;
    config.web_search = args.web_search.0;
    config.search_by_meaning = args.search_by_meaning;
    if args.use_system_proxy {
        config.use_system_proxy = true;
    }
    config.private_search = args.private_search;
    if args.network {
        let mut net = NetConfig::new(config.data_dir.join("net"));
        let port = args.p2p_port;
        net.listen = [
            format!("/ip4/0.0.0.0/tcp/{port}"),
            format!("/ip4/0.0.0.0/udp/{port}/quic-v1"),
            format!("/ip6/::/tcp/{port}"),
            format!("/ip6/::/udp/{port}/quic-v1"),
        ]
        .iter()
        .map(|addr| addr.parse().expect("a valid multiaddr"))
        .collect();
        if !args.no_default_bootstrap {
            net.bootstrap = plumb_net::default_bootstrap();
        }
        for addr in args.bootstrap {
            if !net.bootstrap.contains(&addr) {
                net.bootstrap.push(addr);
            }
        }
        net.external = args.public_addr;
        net.relay_server = args.relay;
        net.upnp = !args.no_upnp;
        net.local_discovery = !args.no_local_discovery;
        if args.no_default_trust {
            net.trusted_peers.clear();
        }
        net.trusted_peers.extend(args.trust_peer);
        if let Some(days) = args.keep_batches_days {
            net.keep_batches_days = days;
        }
        config.network = Some(net);
        config.share_popularity = args.share_popularity;
        config.publish_records = args.publish_records;
        config.crawl_any_site = args.crawl_any_site;
        config.crawl_with = args.crawl_with;
    }
    config
}

/// Where to point a browser. An address that listens on every interface
/// (`0.0.0.0`, `::`) is reached through loopback on the machine that runs
/// Plumb, through that machine's address from others, and through the
/// published host port when Plumb runs in a container.
fn listening_message(addr: SocketAddr) -> String {
    let local = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        _ => {
            return format!("Plumb Search is running at http://{addr}/ (Ctrl-C to stop)");
        }
    };
    let port = addr.port();
    format!(
        "Plumb Search is listening on port {port} of every network interface ({addr}). \
         Open http://{}/ on the machine it runs on, or that machine's address from \
         another; in a container, open the host port published for {port} instead. \
         (Ctrl-C to stop)",
        SocketAddr::new(local, port)
    )
}

/// `plumb remote-control`: turns remote control on or off in a node's data
/// directory. A running node notices at its next request.
pub fn remote_control(args: crate::cli::RemoteControlArgs) -> Result<()> {
    use crate::cli::RemoteControlAction;
    use crate::node::control;
    use anyhow::Context as _;
    let dir = &args.data;
    if !dir.is_dir() {
        anyhow::bail!(
            "{} is not a directory; pass the node's data directory, as given to plumb run --data",
            dir.display()
        );
    }
    match args.action {
        RemoteControlAction::On { allow_public } => {
            let token = control::turn_on(dir, allow_public)
                .with_context(|| format!("turning remote control on in {}", dir.display()))?;
            println!("Remote control is on. The token, shown only this once:\n\n  {token}\n");
            println!(
                "In the Plumb Search app on another computer, choose \"Connect to a node\" and \
                 enter this node's address (such as http://192.168.1.20:8080) and the token."
            );
            if allow_public {
                println!("It works from any address. Keep the node behind HTTPS.");
            } else {
                println!("It works from this computer and local networks only.");
            }
        }
        RemoteControlAction::Off => {
            if control::turn_off(dir)? {
                println!("Remote control is off.");
            } else {
                println!("Remote control was already off.");
            }
        }
        RemoteControlAction::Status => match control::load(dir)? {
            None => println!("Remote control is off."),
            Some(on) if on.allow_public => println!("Remote control is on, from any address."),
            Some(_) => println!("Remote control is on, from this computer and local networks."),
        },
    }
    Ok(())
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
        assert!(!off.use_system_proxy);
        assert!(config(&["--data", "d", "--use-system-proxy"]).use_system_proxy);
        assert_eq!(off.network, None);
        assert!(!off.private_search);
        assert!(config(&["--data", "d", "--private-search"]).private_search);
    }

    #[test]
    fn network_flags() {
        let node = config(&[
            "--data",
            "d",
            "--network",
            "--p2p-port",
            "4100",
            "--bootstrap",
            "/dns4/plumbsearch.org/tcp/4001/p2p/12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg",
            "--public-addr",
            "/ip4/203.0.113.7/tcp/4100",
            "--relay",
            "--trust-peer",
            "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg",
        ]);
        let net = node.network.unwrap();
        // The plumbsearch.org node by default, and the one given.
        let trusted: Vec<String> = net.trusted_peers.iter().map(|p| p.to_string()).collect();
        assert_eq!(
            trusted,
            [
                plumb_net::node::DEFAULT_TRUSTED_PEERS[0],
                "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg"
            ]
        );
        let none = config(&["--data", "d", "--network", "--no-default-trust"]);
        assert!(none.network.unwrap().trusted_peers.is_empty());
        assert_eq!(net.listen[0].to_string(), "/ip4/0.0.0.0/tcp/4100");
        // The network's own first nodes, then the one given.
        assert_eq!(net.bootstrap.len(), plumb_net::DEFAULT_BOOTSTRAP.len() + 1);
        assert_eq!(
            net.bootstrap[0].to_string(),
            plumb_net::DEFAULT_BOOTSTRAP[0]
        );
        let apart = config(&["--data", "d", "--network", "--no-default-bootstrap"]);
        assert!(apart.network.unwrap().bootstrap.is_empty());
        let given_twice = config(&[
            "--data",
            "d",
            "--network",
            "--bootstrap",
            plumb_net::DEFAULT_BOOTSTRAP[0],
        ]);
        assert_eq!(
            given_twice.network.unwrap().bootstrap.len(),
            plumb_net::DEFAULT_BOOTSTRAP.len()
        );
        assert_eq!(net.external.len(), 1);
        assert!(net.relay_server && net.upnp);
        assert!(!node.share_popularity);
        assert!(config(&["--data", "d", "--network", "--share-popularity"]).share_popularity);
        let parse = |args: &[&str]| Cli::try_parse_from(["plumb", "run"].iter().chain(args));
        assert!(
            parse(&["--data", "d", "--relay"]).is_err(),
            "--relay needs --public-addr"
        );
        assert!(parse(&["--data", "d", "--bootstrap", "/ip4/1.2.3.4/tcp/1"]).is_err());
        assert!(
            parse(&["--data", "d", "--network", "--trust-peer", "not-a-peer"]).is_err(),
            "a trusted peer must be a node id"
        );
        assert!(
            parse(&["--data", "d", "--share-popularity"]).is_err(),
            "--share-popularity needs --network"
        );
        let peer = "12D3KooWDHxYtCdqrfk6QM21uNnSYcg38K18rDE4hPv71HKQGUxT";
        let any = config(&[
            "--data",
            "d",
            "--network",
            "--crawl-any-site",
            "--crawl-with",
            peer,
            "--keep-batches-days",
            "10",
            "--crawl-concurrency",
            "64",
        ]);
        assert!(any.crawl_any_site);
        assert_eq!(any.crawl_with, vec![peer.parse().unwrap()]);
        assert_eq!(any.network.unwrap().keep_batches_days, 10);
        assert_eq!(any.crawl_concurrency, Some(64));
        assert_eq!(net.keep_batches_days, 35);
        assert!(parse(&["--data", "d", "--network", "--crawl-with", peer]).is_err());
        assert!(parse(&["--data", "d", "--crawl-any-site"]).is_err());
    }

    #[test]
    fn says_where_to_browse() {
        assert_eq!(
            listening_message("127.0.0.1:8080".parse().unwrap()),
            "Plumb Search is running at http://127.0.0.1:8080/ (Ctrl-C to stop)"
        );
        assert_eq!(
            listening_message("0.0.0.0:8080".parse().unwrap()),
            "Plumb Search is listening on port 8080 of every network interface \
             (0.0.0.0:8080). Open http://127.0.0.1:8080/ on the machine it runs on, or \
             that machine's address from another; in a container, open the host port \
             published for 8080 instead. (Ctrl-C to stop)"
        );
        let v6 = listening_message("[::]:80".parse().unwrap());
        assert!(v6.contains("([::]:80). Open http://[::1]:80/ on"), "{v6}");
    }
}
