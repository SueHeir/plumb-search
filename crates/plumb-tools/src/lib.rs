//! `plumb-tools`: the commands a node never runs. They make the project's
//! page sets ([`fetch`], [`build`]) and map ([`map_fetch`]), measure and
//! train ranking ([`eval`], [`eval_labels`], [`train_rank`]), do a node's
//! work by hand ([`crawl`], [`search`], [`embed`]), and look into records
//! files and a node's data ([`link_rank`], [`top_sites`], [`fact_trust`],
//! [`terms`], [`summaries`], [`storage`], [`dead_sites`]). [`set`] writes
//! and signs sets in the data format, page sets as items with
//! [`page_items`], which nodes only check and read. [`pir`] is the table a
//! private information retrieval server would answer from, which no node
//! runs yet, and [`relations`] an experiment in following Wikidata facts by
//! arithmetic on article vectors. The node itself is
//! [`plumb_node`].

use anyhow::Result;

pub mod build;
pub mod cli;
pub mod crawl;
pub mod dead_sites;
pub mod embed;
pub mod eval;
pub mod eval_labels;
pub mod fact_trust;
pub mod fetch;
pub mod link_rank;
pub mod map_fetch;
pub mod page_items;
pub mod pir;
pub mod relations;
pub mod search;
pub mod set;
pub mod site_rules;
pub mod storage;
pub mod summaries;
pub mod terms;
pub mod top_sites;
pub mod train_rank;

use cli::{Cli, Command};

/// Runs one `plumb-tools` subcommand.
pub fn run(cli: Cli) -> Result<()> {
    plumb_node::set_up_allocator();
    plumb_node::limits::raise_open_file_limit();
    match cli.command {
        Command::FetchData(args) => fetch::run(args),
        Command::FetchPages(args) => fetch::run_pages(args),
        Command::FetchMap(args) => map_fetch::run(args),
        Command::FetchProfiles(args) => fetch::run_profiles(args),
        Command::FetchFacts(args) => fetch::run_facts(args),
        Command::FetchLeads(args) => fetch::run_leads(args),
        Command::Eval(args) => eval::run(args),
        Command::CheckLabels(args) => eval_labels::run(args),
        Command::TrainRank(args) => train_rank::run(args),
        Command::FetchText(args) => terms::run_fetch_text(args),
        Command::Terms(args) => terms::run_terms(args),
        Command::Summaries(args) => summaries::run(args),
        Command::Storage(args) => storage::run(args),
        Command::LinkRank(args) => link_rank::run(&args),
        Command::FactTrust(args) => fact_trust::run(&args),
        Command::TopSites(args) => top_sites::run(&args),
        Command::Set(args) => set::run(args),
        Command::Crawl(args) => crawl::run(args),
        Command::Index(args) => search::run_index(args),
        Command::Search(args) => search::run_search(args),
        Command::Spelling(args) => search::run_spelling(&args),
        Command::Embed(args) => embed::run(args),
        Command::DeadSites(args) => dead_sites::run(&args),
        Command::Relations(args) => relations::run(&args),
        Command::Relate(args) => relations::run_relate(&args),
    }
}
