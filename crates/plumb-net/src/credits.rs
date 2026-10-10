//! Crawl credits: what a node earns by crawling for the network, and the
//! anonymous one-time tokens it turns them into.
//!
//! **Earning.** Every node keeps its own [`Ledger`] of every crawler it
//! hears from, itself included. A crawler earns [`CREDITS_PER_CRAWL`] for
//! each homepage crawl that a crawler it trusts strictly (itself, or one
//! vouched for by matching its own crawls) also made: never for a crawl
//! that counted on its own, or only fresh keys agree with, however lenient
//! agreement is
//! ([`crate::agree`]), twice that for crawls made before
//! [`FIRST_YEAR_ENDS`] (the early-adopter head start), and loses
//! [`DISAGREE_PENALTY`] for each crawl made close in time to one two such
//! crawlers made that did not match it. A node this one trusts
//! (`NetConfig::trusted_peers`) earns the same for each crawl it takes in
//! from it, since those skip agreement. And a node earns
//! [`CREDITS_PER_ANSWER`] for each bucket request of this node's it
//! answered: work only the node that asked can vouch for, so only its
//! ledger counts it. Every node sees the same signed batches, so their
//! ledgers come out much the same for crawls, but each node only ever goes
//! by its own.
//!
//! **What tokens buy: being answered when others are not.** A node shares
//! out the requests it answers for free (see [`crate::allowance`]): a few
//! at once, so many a minute from any one address or relay, and as many a
//! day as its owner allows. Past that it says it is busy. A request that
//! carries a token of its own still gets in, up to a few more at once, so
//! the nodes that crawl and answer for the network are answered first when
//! it is swamped. Bucket requests are sent under throwaway identities, so
//! a token is the only way a request can show it comes from a node that
//! did its part, and it shows nothing more. Free searches still work, they
//! just wait or try another node. A node keeps a few tokens from each node
//! it searches, topped up in the background, and spends one only when a
//! node said it was busy.
//!
//! **Tokens.** Credits are never sent anywhere. A node asks another node,
//! the **issuer**, for tokens over its own identity; the issuer checks its
//! ledger, signs the tokens blind (a VOPRF over ristretto255, as in Privacy
//! Pass, RFC 9578) and takes [`TOKEN_PRICE`] off the asker's balance for
//! each. Later the node hands a token back to that issuer under a
//! throwaway identity: the issuer can check the token is one of its own and
//! not spent, but cannot tell which of the nodes it issued tokens to holds
//! it. A token is only good at the node that issued it, so a node's credits
//! at one issuer are what that issuer counted for it.
//!
//! The issuer proves every batch of tokens was signed with the key it
//! issued earlier ones with (the VOPRF proof), and the [`Wallet`] keeps the
//! first key it sees for each issuer and refuses tokens under another one,
//! so an issuer cannot quietly hand one node a key of its own and recognize
//! its tokens later.
//!
//! Credits and tokens cannot be given away or sold: an issuer only gives
//! tokens to the node whose balance pays for them. Likes, when they come,
//! are a separate allowance and never bought with credits.
//!
//! ```text
//! DIR/net/credits/ledger.json   every crawler's account, as this node counts it
//! DIR/net/credits/token.key     this node's issuing key (private)
//! DIR/net/credits/spent         tokens handed back to this node, 32 bytes each
//! DIR/net/credits/wallet.json   tokens this node holds, by issuer (private)
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use libp2p::PeerId;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use voprf::{
    BlindedElement, EvaluationElement, Group, Proof, Ristretto255, VoprfClient, VoprfServer,
};

use crate::agree::Verdict;

/// Credits for one confirmed homepage crawl.
pub const CREDITS_PER_CRAWL: i64 = 1;
/// Crawls made before this time (2027-10-01, a year into the network)
/// earn [`EARLY_MULTIPLIER`] times as much: the head start for early
/// adopters.
pub const FIRST_YEAR_ENDS: u64 = 1_822_348_800;
pub const EARLY_MULTIPLIER: i64 = 2;
/// Credits lost for a crawl that did not match a confirmed one made close
/// in time: a made-up page costs more than an honest one earns.
pub const DISAGREE_PENALTY: i64 = 5;
/// Credits for answering one bucket request of this node's.
pub const CREDITS_PER_ANSWER: i64 = 1;
/// Credits one token costs.
pub const TOKEN_PRICE: i64 = 1;
/// Bucket requests of ours a node must have answered before it may have
/// tokens for that work alone, without crawls that count here.
pub const MIN_ANSWERS_FOR_TOKENS: u64 = 10;
/// Most tokens issued for one request.
pub const MAX_ISSUE: usize = 64;
/// A token's random input, in bytes.
pub const TOKEN_INPUT_LEN: usize = 32;

const LEDGER_FILE: &str = "ledger.json";
const KEY_FILE: &str = "token.key";
const SPENT_FILE: &str = "spent";
const WALLET_FILE: &str = "wallet.json";

/// One crawler's account, as this node counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Credits earned by confirmed crawls.
    pub earned: i64,
    /// Credits lost to crawls that did not match.
    pub lost: i64,
    /// Credits spent on tokens this node issued to the crawler.
    pub spent: i64,
    /// Crawls confirmed, and crawls that did not match.
    pub confirmed: u64,
    pub mismatched: u64,
    /// Bucket requests of this node's it answered.
    #[serde(default)]
    pub answered: u64,
}

impl Account {
    /// What is left to spend; below zero after mismatches, until earned
    /// back.
    pub fn balance(&self) -> i64 {
        self.earned - self.lost - self.spent
    }
}

/// The credits of every crawler this node heard from.
#[derive(Debug, Default)]
pub struct Ledger {
    path: Option<PathBuf>,
    accounts: HashMap<PeerId, Account>,
    dirty: bool,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    accounts: BTreeMap<String, Account>,
}

/// Credits a crawl earns, by when it was made.
pub fn credits_for(crawled_at: u64) -> i64 {
    if crawled_at < FIRST_YEAR_ENDS {
        CREDITS_PER_CRAWL * EARLY_MULTIPLIER
    } else {
        CREDITS_PER_CRAWL
    }
}

impl Ledger {
    /// An empty ledger kept only in memory.
    pub fn in_memory() -> Ledger {
        Ledger::default()
    }

    /// Reads the ledger in `dir`, or starts an empty one there.
    pub fn open(dir: &Path) -> Result<Ledger> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(LEDGER_FILE);
        let accounts = match fs::read(&path) {
            Ok(bytes) => {
                let file: LedgerFile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("reading {}", path.display()))?;
                file.accounts
                    .into_iter()
                    .filter_map(|(peer, account)| Some((peer.parse().ok()?, account)))
                    .collect()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Ledger {
            path: Some(path),
            accounts,
            dirty: false,
            budget: None,
        })
    }

    pub fn with_budget(
        mut self,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Self {
        self.budget = budget;
        self
    }

    /// Counts the crawls agreement scored.
    pub fn record(&mut self, verdicts: &[Verdict]) {
        for verdict in verdicts {
            // A crawl earns only when another crawler this node trusts
            // strictly (itself included) made the same crawl, so a lone
            // node, or fresh keys agreeing with each other, mint nothing.
            // A crawl costs only when two such crawlers say otherwise.
            // A trusted node's crawls are taken in without a witness.
            let needed = if verdict.trusted {
                0
            } else if verdict.agreed {
                1
            } else {
                2
            };
            if verdict.witnesses < needed {
                continue;
            }
            let account = self.accounts.entry(verdict.crawler).or_default();
            if verdict.agreed {
                account.earned += credits_for(verdict.crawled_at);
                account.confirmed += 1;
            } else {
                account.lost += DISAGREE_PENALTY;
                account.mismatched += 1;
            }
            self.dirty = true;
        }
    }

    /// Credits each of `peers` for answering one bucket request of ours
    /// (a peer listed twice answered twice).
    pub fn record_answers(&mut self, peers: &[PeerId]) {
        for peer in peers {
            let account = self.accounts.entry(*peer).or_default();
            account.earned += CREDITS_PER_ANSWER;
            account.answered += 1;
            self.dirty = true;
        }
    }

    /// Nodes with credits left here.
    pub fn in_credit(&self) -> usize {
        self.accounts.values().filter(|a| a.balance() > 0).count()
    }

    pub fn account(&self, peer: &PeerId) -> Account {
        self.accounts.get(peer).copied().unwrap_or_default()
    }

    /// Crawlers with an account.
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Tokens `peer` can have now, at most `wanted`. None for a crawler
    /// whose crawls do not count (too new, or distrusted: `counts`).
    pub fn issuable(&self, peer: &PeerId, counts: bool, wanted: usize) -> usize {
        if !counts {
            return 0;
        }
        let balance = self.account(peer).balance().max(0);
        let affordable = usize::try_from(balance / TOKEN_PRICE).unwrap_or(usize::MAX);
        affordable.min(wanted).min(MAX_ISSUE)
    }

    /// Takes `tokens` tokens' worth off `peer`'s balance.
    pub fn charge(&mut self, peer: &PeerId, tokens: usize) {
        let account = self.accounts.entry(*peer).or_default();
        account.spent += TOKEN_PRICE * tokens as i64;
        self.dirty = true;
    }

    /// Writes the ledger if it changed since it was last written.
    pub fn save(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let file = LedgerFile {
            accounts: self
                .accounts
                .iter()
                .map(|(peer, account)| (peer.to_string(), *account))
                .collect(),
        };
        let bytes = serde_json::to_vec(&file).context("encoding the ledger")?;
        if let Some(budget) = &self.budget {
            plumb_core::storage::write_atomic(
                path,
                &path.with_extension("quota.tmp"),
                &bytes,
                Some(budget),
                None,
            )?;
        } else {
            write_atomic(path, &bytes, false)?;
        }
        self.dirty = false;
        Ok(())
    }
}

/// A token: a random input and the issuer's VOPRF output for it. Spent by
/// handing it back to the issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub input: ByteBuf,
    pub output: ByteBuf,
}

impl Token {
    /// What the issuer remembers a spent token by.
    fn id(&self) -> [u8; 32] {
        Sha256::digest(&self.input).into()
    }
}

/// The issuer's answer to a request for tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issued {
    /// The issuer's public key.
    pub key: ByteBuf,
    /// One evaluated element for each blinded one, in order.
    pub evaluated: Vec<ByteBuf>,
    /// Proves all of them were made with `key`.
    pub proof: ByteBuf,
}

/// This node as an issuer: its key and the tokens handed back to it.
pub struct Issuer {
    server: VoprfServer<Ristretto255>,
    spent: HashSet<[u8; 32]>,
    spent_path: Option<PathBuf>,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("spent", &self.spent.len())
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// An issuer with a fresh key, kept only in memory.
    pub fn in_memory() -> Result<Issuer> {
        let server = VoprfServer::new(&mut OsRng).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Issuer {
            server,
            spent: HashSet::new(),
            spent_path: None,
        })
    }

    /// Reads the issuing key and the spent tokens in `dir`, making the key
    /// when there is none.
    pub fn open(dir: &Path) -> Result<Issuer> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key_path = dir.join(KEY_FILE);
        let server = match fs::read(&key_path) {
            Ok(bytes) => VoprfServer::<Ristretto255>::deserialize(&bytes)
                .map_err(|e| anyhow::anyhow!("{} is not a token key: {e:?}", key_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let server = VoprfServer::<Ristretto255>::new(&mut OsRng)
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                write_atomic(&key_path, &server.serialize(), true)?;
                server
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", key_path.display())),
        };
        let spent_path = dir.join(SPENT_FILE);
        let spent = match fs::read(&spent_path) {
            Ok(bytes) => bytes.as_chunks::<32>().0.iter().copied().collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", spent_path.display()))
            }
        };
        Ok(Issuer {
            server,
            spent,
            spent_path: Some(spent_path),
        })
    }

    /// The public key, as sent to askers.
    pub fn public_key(&self) -> Vec<u8> {
        <Ristretto255 as Group>::serialize_elem(self.server.get_public_key()).to_vec()
    }

    /// Signs `blinded` tokens, at most [`MAX_ISSUE`]. The caller has
    /// checked and charged the asker's balance.
    pub fn issue(&self, blinded: &[ByteBuf]) -> Result<Issued> {
        ensure!(!blinded.is_empty(), "no tokens asked for");
        ensure!(blinded.len() <= MAX_ISSUE, "too many tokens asked for");
        let elements = blinded
            .iter()
            .map(|b| BlindedElement::<Ristretto255>::deserialize(b))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("not a blinded token: {e:?}"))?;
        let result = self
            .server
            .batch_blind_evaluate(&mut OsRng, &elements)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(Issued {
            key: ByteBuf::from(self.public_key()),
            evaluated: result
                .messages
                .iter()
                .map(|m| ByteBuf::from(m.serialize().to_vec()))
                .collect(),
            proof: ByteBuf::from(result.proof.serialize().to_vec()),
        })
    }

    /// Whether `token` is one of ours and not spent yet; marks it spent
    /// when it is. A token that cannot be noted as spent on disk is
    /// refused: taken, it could be spent again after a restart.
    pub fn redeem(&mut self, token: &Token) -> bool {
        if token.input.len() != TOKEN_INPUT_LEN {
            return false;
        }
        let Ok(expected) = self.server.evaluate(&token.input) else {
            return false;
        };
        if !bool::from(expected.as_slice().ct_eq(&token.output)) {
            return false;
        }
        let id = token.id();
        if !self.spent.insert(id) {
            return false;
        }
        if let Some(path) = &self.spent_path {
            let written = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| f.write_all(&id));
            if let Err(err) = written {
                tracing::warn!("cannot note a spent token in {}: {err}", path.display());
                self.spent.remove(&id);
                return false;
            }
        }
        true
    }

    /// Tokens handed back to this node so far.
    pub fn redeemed(&self) -> usize {
        self.spent.len()
    }
}

/// Tokens asked for and not yet signed: what is needed to finish them.
pub struct Pending {
    inputs: Vec<[u8; TOKEN_INPUT_LEN]>,
    clients: Vec<VoprfClient<Ristretto255>>,
    /// To send to the issuer.
    pub blinded: Vec<ByteBuf>,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("tokens", &self.inputs.len())
            .finish_non_exhaustive()
    }
}

impl Pending {
    /// `n` fresh tokens to ask an issuer to sign, at most [`MAX_ISSUE`].
    pub fn new(n: usize) -> Result<Pending> {
        let n = n.min(MAX_ISSUE);
        let mut inputs = Vec::with_capacity(n);
        let mut clients = Vec::with_capacity(n);
        let mut blinded = Vec::with_capacity(n);
        for _ in 0..n {
            let mut input = [0u8; TOKEN_INPUT_LEN];
            OsRng.fill_bytes(&mut input);
            let result = VoprfClient::<Ristretto255>::blind(&input, &mut OsRng)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            inputs.push(input);
            blinded.push(ByteBuf::from(result.message.serialize().to_vec()));
            clients.push(result.state);
        }
        Ok(Pending {
            inputs,
            clients,
            blinded,
        })
    }

    /// The tokens, once `issued` checks out against them. An issuer may
    /// sign fewer than asked (what the balance allows): the first ones.
    pub fn finish(mut self, issued: &Issued) -> Result<Vec<Token>> {
        let n = issued.evaluated.len();
        ensure!(n <= self.inputs.len(), "more tokens than asked for");
        if n == 0 {
            return Ok(Vec::new());
        }
        let key = <Ristretto255 as Group>::deserialize_elem(&issued.key)
            .map_err(|e| anyhow::anyhow!("not a token key: {e:?}"))?;
        let messages = issued
            .evaluated
            .iter()
            .map(|m| EvaluationElement::<Ristretto255>::deserialize(m))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("not a signed token: {e:?}"))?;
        let proof = Proof::<Ristretto255>::deserialize(&issued.proof)
            .map_err(|e| anyhow::anyhow!("not a proof: {e:?}"))?;
        let inputs: Vec<&[u8]> = self.inputs[..n].iter().map(|i| i.as_slice()).collect();
        self.clients.truncate(n);
        let outputs =
            VoprfClient::batch_finalize(&inputs, &self.clients, &messages, &proof, key)
                .map_err(|e| anyhow::anyhow!("the issuer's proof does not check out: {e:?}"))?;
        let mut tokens = Vec::with_capacity(n);
        for (input, output) in self.inputs[..n].iter().zip(outputs) {
            let output = output.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            tokens.push(Token {
                input: ByteBuf::from(input.to_vec()),
                output: ByteBuf::from(output.to_vec()),
            });
        }
        Ok(tokens)
    }
}

/// Tokens this node holds, by issuer.
#[derive(Debug, Default)]
pub struct Wallet {
    path: Option<PathBuf>,
    issuers: BTreeMap<String, Held>,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Held {
    /// The issuer's key, as first seen.
    key: ByteBuf,
    tokens: Vec<Token>,
}

impl Wallet {
    /// An empty wallet kept only in memory.
    pub fn in_memory() -> Wallet {
        Wallet::default()
    }

    /// Reads the wallet in `dir`, or starts an empty one there.
    pub fn open(dir: &Path) -> Result<Wallet> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(WALLET_FILE);
        let issuers = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Wallet {
            path: Some(path),
            issuers,
            budget: None,
        })
    }

    pub fn with_budget(
        mut self,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Self {
        self.budget = budget;
        self
    }

    /// Keeps tokens `issuer` signed with `key`. Refused when the issuer
    /// signed earlier tokens with another key.
    pub fn add(&mut self, issuer: &PeerId, key: &[u8], tokens: Vec<Token>) -> Result<()> {
        let held = self.issuers.entry(issuer.to_string()).or_default();
        if held.key.is_empty() {
            held.key = ByteBuf::from(key.to_vec());
        } else if held.key.as_slice() != key {
            bail!("{issuer} signed tokens with a new key; refusing them");
        }
        held.tokens.extend(tokens);
        self.save()
    }

    /// Tokens held for `issuer`.
    pub fn held(&self, issuer: &PeerId) -> usize {
        self.issuers
            .get(&issuer.to_string())
            .map_or(0, |h| h.tokens.len())
    }

    /// Tokens held, all issuers.
    pub fn total(&self) -> usize {
        self.issuers.values().map(|h| h.tokens.len()).sum()
    }

    /// Takes one token of `issuer`'s to spend.
    pub fn take(&mut self, issuer: &PeerId) -> Option<Token> {
        let token = self.issuers.get_mut(&issuer.to_string())?.tokens.pop()?;
        if let Err(err) = self.save() {
            tracing::warn!("cannot save the wallet: {err:#}");
        }
        Some(token)
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(&self.issuers).context("encoding the wallet")?;
        if let Some(budget) = &self.budget {
            plumb_core::storage::write_atomic_private(
                path,
                &path.with_extension("quota.tmp"),
                &bytes,
                Some(budget),
            )?;
            Ok(())
        } else {
            write_atomic(path, &bytes, true)
        }
    }
}

/// What a node's credits look like, for `GET /api/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditStatus {
    /// This node's own balance, as it counts its own crawls.
    pub balance: i64,
    /// Its crawls confirmed by another crawler, and those that did not
    /// match.
    pub confirmed_crawls: u64,
    pub mismatched_crawls: u64,
    /// Crawlers this node keeps an account for.
    pub accounts: usize,
    /// Tokens this node issued to others, and that were handed back.
    pub tokens_issued: u64,
    pub tokens_redeemed: usize,
    /// Bucket requests this node answered while busy, for tokens.
    #[serde(default)]
    pub priority_answered: u64,
    /// Tokens this node's own searches spent at busy nodes.
    #[serde(default)]
    pub tokens_spent: u64,
    /// Tokens this node holds, from all issuers.
    pub tokens_held: usize,
    /// Bucket requests answered for free today (UTC) for nodes this node
    /// does not trust, and the owner's daily limit on them, if any.
    #[serde(default)]
    pub free_answers_today: u64,
    #[serde(default)]
    pub answer_per_day: Option<u64>,
    /// Bucket requests turned away as busy since start.
    #[serde(default)]
    pub turned_away: u64,
    /// Nodes with credits left here.
    #[serde(default)]
    pub in_credit_here: usize,
    /// This node's credits at each node it searches, as each last said,
    /// most first.
    #[serde(default)]
    pub at_peers: Vec<CreditsAtPeer>,
}

/// This node's credits as one other node counts them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditsAtPeer {
    pub peer_id: String,
    pub credits: i64,
    /// Its work counts there, so it can have tokens for them.
    pub counts: bool,
}

fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    fn verdict(crawler: PeerId, crawled_at: u64, agreed: bool) -> Verdict {
        Verdict {
            crawler,
            crawled_at,
            agreed,
            witnesses: 2,
            trusted: false,
        }
    }

    #[test]
    fn guarded_credit_and_private_wallet_replacements_preserve_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::open(dir.path()).unwrap();
        let peer = PeerId::random();
        ledger.accounts.insert(
            peer,
            Account {
                earned: 10,
                ..Account::default()
            },
        );
        ledger.dirty = true;
        ledger.save().unwrap();
        let prior = fs::read(dir.path().join(LEDGER_FILE)).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), 1).unwrap();
        let mut ledger = ledger.with_budget(Some(budget.clone()));
        ledger.accounts.get_mut(&peer).unwrap().earned = 20;
        ledger.dirty = true;
        assert!(ledger.save().is_err());
        assert_eq!(fs::read(dir.path().join(LEDGER_FILE)).unwrap(), prior);
        assert!(ledger.dirty);
        budget.set_limit(1024 * 1024);
        ledger.save().unwrap();
        assert!(!ledger.dirty);
        let wallet = Wallet::open(dir.path())
            .unwrap()
            .with_budget(Some(budget.clone()));
        wallet.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join(WALLET_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn a_trusted_nodes_crawls_earn_without_witnesses() {
        let t = peer();
        let mut ledger = Ledger::in_memory();
        let mut taken_in = verdict(t, FIRST_YEAR_ENDS + 10, true);
        taken_in.witnesses = 0;
        taken_in.trusted = true;
        ledger.record(&[taken_in]);
        assert_eq!(ledger.account(&t).balance(), CREDITS_PER_CRAWL);
        assert_eq!(ledger.account(&t).confirmed, 1);
    }

    #[test]
    fn answering_earns_and_buys_tokens() {
        let a = peer();
        let mut ledger = Ledger::in_memory();
        ledger.record_answers(&[a, a, a]);
        assert_eq!(ledger.account(&a).answered, 3);
        assert_eq!(ledger.account(&a).balance(), 3 * CREDITS_PER_ANSWER);
        assert_eq!(ledger.in_credit(), 1);
        assert_eq!(ledger.issuable(&a, true, 10), 3);
        ledger.charge(&a, 3);
        assert_eq!(ledger.in_credit(), 0);
    }

    #[test]
    fn a_crawl_nobody_else_confirmed_earns_nothing() {
        let a = peer();
        let mut ledger = Ledger::in_memory();
        let mut alone = verdict(a, 0, true);
        alone.witnesses = 0;
        let mut against = verdict(peer(), 0, false);
        against.witnesses = 1;
        ledger.record(&[alone, against]);
        assert!(ledger.is_empty(), "neither earns nor costs");
        assert_eq!(ledger.account(&a).earned, 0);
        assert_eq!(ledger.account(&a).confirmed, 0);
    }

    #[test]
    fn confirmed_crawls_earn_and_mismatches_cost() {
        let a = peer();
        let mut ledger = Ledger::in_memory();
        let late = FIRST_YEAR_ENDS + 10;
        ledger.record(&[verdict(a, late, true), verdict(a, late, true)]);
        assert_eq!(ledger.account(&a).balance(), 2);
        ledger.record(&[verdict(a, late, false)]);
        assert_eq!(ledger.account(&a).balance(), 2 - DISAGREE_PENALTY);
        assert_eq!(ledger.issuable(&a, true, 10), 0);
        assert_eq!(ledger.account(&a).confirmed, 2);
        assert_eq!(ledger.account(&a).mismatched, 1);
    }

    #[test]
    fn early_crawls_earn_double() {
        let a = peer();
        let mut ledger = Ledger::in_memory();
        ledger.record(&[verdict(a, FIRST_YEAR_ENDS - 1, true)]);
        assert_eq!(ledger.account(&a).earned, 2);
        ledger.record(&[verdict(a, FIRST_YEAR_ENDS, true)]);
        assert_eq!(ledger.account(&a).earned, 3);
    }

    #[test]
    fn tokens_only_for_crawlers_that_count_and_can_pay() {
        let a = peer();
        let mut ledger = Ledger::in_memory();
        let late = FIRST_YEAR_ENDS + 10;
        ledger.record(&vec![verdict(a, late, true); 5]);
        assert_eq!(ledger.issuable(&a, false, 10), 0);
        assert_eq!(ledger.issuable(&a, true, 10), 5);
        assert_eq!(ledger.issuable(&a, true, 3), 3);
        ledger.charge(&a, 4);
        assert_eq!(ledger.issuable(&a, true, 10), 1);
        assert_eq!(ledger.issuable(&peer(), true, 10), 0);
    }

    #[test]
    fn ledger_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let a = peer();
        let mut ledger = Ledger::open(dir.path()).unwrap();
        ledger.record(&[verdict(a, 0, true)]);
        ledger.charge(&a, 1);
        ledger.save().unwrap();
        let ledger = Ledger::open(dir.path()).unwrap();
        assert_eq!(ledger.account(&a).earned, 2);
        assert_eq!(ledger.account(&a).spent, 1);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn tokens_are_issued_blind_and_spent_once() {
        let mut issuer = Issuer::in_memory().unwrap();
        let pending = Pending::new(4).unwrap();
        let issued = issuer.issue(&pending.blinded).unwrap();
        let tokens = pending.finish(&issued).unwrap();
        assert_eq!(tokens.len(), 4);
        // The issuer never saw the inputs it signed.
        for token in &tokens {
            assert!(!issued
                .evaluated
                .iter()
                .any(|e| e.as_slice() == token.input.as_slice()));
        }
        assert!(issuer.redeem(&tokens[0]));
        assert!(!issuer.redeem(&tokens[0]), "spent twice");
        assert!(issuer.redeem(&tokens[1]));
        assert_eq!(issuer.redeemed(), 2);
    }

    #[test]
    fn forged_and_foreign_tokens_are_refused() {
        let mut issuer = Issuer::in_memory().unwrap();
        let other = Issuer::in_memory().unwrap();
        let pending = Pending::new(1).unwrap();
        let foreign = pending.finish(&other.issue(&Pending::new(1).unwrap().blinded).unwrap());
        // A proof made for other blinded tokens does not check out.
        assert!(foreign.is_err());
        let pending = Pending::new(1).unwrap();
        let issued = other.issue(&pending.blinded).unwrap();
        let theirs = pending.finish(&issued).unwrap();
        assert!(!issuer.redeem(&theirs[0]), "another issuer's token");
        let mut forged = theirs[0].clone();
        forged.output = ByteBuf::from(vec![0u8; 64]);
        assert!(!issuer.redeem(&forged));
    }

    #[test]
    fn fewer_tokens_than_asked_still_check_out() {
        let issuer = Issuer::in_memory().unwrap();
        let pending = Pending::new(5).unwrap();
        let issued = issuer.issue(&pending.blinded[..2]).unwrap();
        assert_eq!(pending.finish(&issued).unwrap().len(), 2);
    }

    #[test]
    fn issuer_key_and_spent_tokens_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut issuer = Issuer::open(dir.path()).unwrap();
        let pending = Pending::new(2).unwrap();
        let issued = issuer.issue(&pending.blinded).unwrap();
        let tokens = pending.finish(&issued).unwrap();
        assert!(issuer.redeem(&tokens[0]));
        let key = issuer.public_key();
        drop(issuer);
        let mut issuer = Issuer::open(dir.path()).unwrap();
        assert_eq!(issuer.public_key(), key);
        assert!(!issuer.redeem(&tokens[0]), "spent before the restart");
        assert!(issuer.redeem(&tokens[1]));
    }

    #[test]
    fn a_token_that_cannot_be_noted_as_spent_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut issuer = Issuer::open(dir.path()).unwrap();
        let pending = Pending::new(1).unwrap();
        let issued = issuer.issue(&pending.blinded).unwrap();
        let tokens = pending.finish(&issued).unwrap();
        // The spent list cannot be appended to.
        fs::create_dir(dir.path().join(SPENT_FILE)).unwrap();
        assert!(!issuer.redeem(&tokens[0]));
        assert_eq!(issuer.redeemed(), 0);
        fs::remove_dir(dir.path().join(SPENT_FILE)).unwrap();
        assert!(issuer.redeem(&tokens[0]));
    }

    #[test]
    fn wallet_pins_each_issuers_key() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer::in_memory().unwrap();
        let other = Issuer::in_memory().unwrap();
        let who = peer();
        let mut wallet = Wallet::open(dir.path()).unwrap();
        let pending = Pending::new(3).unwrap();
        let issued = issuer.issue(&pending.blinded).unwrap();
        wallet
            .add(&who, &issued.key, pending.finish(&issued).unwrap())
            .unwrap();
        assert_eq!(wallet.held(&who), 3);
        let pending = Pending::new(1).unwrap();
        let issued = other.issue(&pending.blinded).unwrap();
        let tokens = pending.finish(&issued).unwrap();
        assert!(wallet.add(&who, &issued.key, tokens).is_err());
        assert!(wallet.take(&who).is_some());
        let wallet = Wallet::open(dir.path()).unwrap();
        assert_eq!(wallet.held(&who), 2);
        assert_eq!(wallet.total(), 2);
    }
}
