//! A model run by an embedding server instead of in Plumb, for trying out
//! models Plumb cannot run itself yet (llama.cpp's `llama-server
//! --embedding`, Ollama, LM Studio: anything that answers OpenAI's
//! `/v1/embeddings`).
//!
//! A model directory holding [`SERVER_FILE`] instead of [`crate::MODEL_FILES`]
//! names the server, the model and how to use it:
//!
//! ```json
//! {"url": "http://127.0.0.1:8090/v1/embeddings", "model": "embeddinggemma-2",
//!  "dim": 256, "query_prefix": "task: search result | query: ",
//!  "text_prefix": "title: none | text: "}
//! ```
//!
//! `dim` keeps the first values of each vector (for models trained to allow
//! it, such as Matryoshka ones), `query_prefix` goes before searches and
//! `text_prefix` before sites' texts; all three are optional. Only plain
//! `http://` addresses work. Vectors from a server are not pinned the way
//! [`crate::Embedder`]'s own are, so they are for evals, not for sharing,
//! unless `"same_as"` names a directory of the EmbeddingGemma files Plumb
//! runs itself ([`crate::GEMMA_FILE`]) that the server runs too: the
//! vectors then carry that model's id, so a node running it in Plumb uses
//! and shares them. It needs Plumb's own `dim` and prefixes.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::gemma::{gemma_id, GEMMA_DIM, GEMMA_QUERY_PREFIX, GEMMA_TEXT_PREFIX};
use crate::{quantize, ModelId};

/// The file in a model directory that names an embedding server.
pub const SERVER_FILE: &str = "embedding-server.json";

/// An embedding server and the model it runs.
pub(crate) struct Server {
    agent: ureq::Agent,
    url: String,
    model: String,
    /// Values kept of each vector; all when `None`.
    dim: Option<usize>,
    query_prefix: String,
    text_prefix: String,
}

impl Server {
    /// The server named by `dir`'s [`SERVER_FILE`], if there is one, with
    /// its [`ModelId`] and vector length (asked of the server).
    pub(crate) fn load(dir: &Path) -> Result<Option<(Self, ModelId, usize)>> {
        let path = dir.join(SERVER_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("reading {}", path.display()))?;
        let text = |key: &str| config.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let url = text("url");
        let model = text("model");
        if url.is_empty() || model.is_empty() {
            bail!("{} needs a url and a model", path.display());
        }
        let dim = match config.get("dim") {
            None => None,
            Some(dim) => Some(
                dim.as_u64()
                    .filter(|&dim| dim > 0)
                    .ok_or_else(|| anyhow!("dim in {} is not a positive number", path.display()))?
                    as usize,
            ),
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        let server = Server {
            agent,
            url: url.to_string(),
            model: model.to_string(),
            dim,
            query_prefix: text("query_prefix").to_string(),
            text_prefix: text("text_prefix").to_string(),
        };
        if let Some(same_as) = config.get("same_as") {
            let same_as = Path::new(
                same_as
                    .as_str()
                    .ok_or_else(|| anyhow!("same_as in {} is not a path", path.display()))?,
            );
            if dim != Some(GEMMA_DIM)
                || server.query_prefix != GEMMA_QUERY_PREFIX
                || server.text_prefix != GEMMA_TEXT_PREFIX
            {
                bail!(
                    "{} names same_as, so it needs dim {GEMMA_DIM} and Plumb's EmbeddingGemma prefixes",
                    path.display()
                );
            }
            let id = gemma_id(same_as)
                .with_context(|| format!("reading the model in {}", same_as.display()))?;
            let len = server
                .embed("dimension check")
                .with_context(|| format!("asking the embedding server at {url}"))?
                .len();
            return Ok(Some((server, id, len)));
        }
        // The id leaves out the address, so moving the server keeps vectors.
        let mut id = Sha256::new();
        for part in [
            "embedding server",
            &server.model,
            &dim.map_or(String::new(), |dim| dim.to_string()),
            &server.query_prefix,
            &server.text_prefix,
        ] {
            id.update(Sha256::digest(part.as_bytes()));
        }
        let len = server
            .embed("dimension check")
            .with_context(|| format!("asking the embedding server at {url}"))?
            .len();
        Ok(Some((server, id.finalize().into(), len)))
    }

    /// The vector of a site's `text`.
    pub(crate) fn embed_text(&self, text: &str) -> Result<Vec<i8>> {
        self.embed(&format!("{}{text}", self.text_prefix))
    }

    /// The vector of a search `query`.
    pub(crate) fn embed_query(&self, query: &str) -> Result<Vec<i8>> {
        self.embed(&format!("{}{query}", self.query_prefix))
    }

    /// The server's vector of `input`, its first [`Server::dim`] values
    /// kept, scaled to length 1 and [`quantize`]d.
    fn embed(&self, input: &str) -> Result<Vec<i8>> {
        // Some servers refuse an empty input.
        let input = if input.trim().is_empty() { " " } else { input };
        let body = serde_json::json!({"model": self.model, "input": input}).to_string();
        let answer = self
            .agent
            .post(&self.url)
            .header("content-type", "application/json")
            .send(&body)?
            .body_mut()
            .read_to_string()?;
        let answer: serde_json::Value =
            serde_json::from_str(&answer).context("reading the embedding server's answer")?;
        let values: Vec<f32> = answer
            .pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("the embedding server's answer has no data[0].embedding"))?
            .iter()
            .map(|v| v.as_f64().map(|v| v as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| anyhow!("the embedding server gave a value that is not a number"))?;
        let kept = &values[..self.dim.unwrap_or(values.len()).min(values.len())];
        let length = kept.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !length.is_normal() {
            bail!("the embedding server gave a vector of length {length}");
        }
        let unit: Vec<f32> = kept.iter().map(|x| x / length).collect();
        Ok(quantize(&unit))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::Embedder;

    /// Serves `answers` embedding requests on a local port, each with a
    /// vector of 4 values made from the request's length; returns the
    /// address and the requests' bodies once served.
    fn fake_server(answers: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let served = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let body = loop {
                    let n = stream.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text[..end]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break text[end + 4..end + 4 + length].to_string();
                        }
                    }
                };
                let x = body.len() as f32;
                let answer =
                    serde_json::json!({"data": [{"embedding": [3.0, 4.0, x, -x]}]}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .unwrap();
                bodies.push(body);
            }
            bodies
        });
        (url, served)
    }

    #[test]
    fn a_server_embeds_with_prefixes_and_cut_vectors() {
        let (url, served) = fake_server(3);
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({"url": url, "model": "m", "dim": 2,
            "query_prefix": "query: ", "text_prefix": "text: "});
        std::fs::write(dir.path().join(SERVER_FILE), config.to_string()).unwrap();
        let embedder = Embedder::load(dir.path()).unwrap();
        assert_eq!(embedder.dim(), 2);
        // The first two values, 3 and 4, scaled to length 1.
        assert_eq!(embedder.embed("tesla").unwrap(), [76, 102]);
        assert_eq!(embedder.embed_query("car").unwrap(), [76, 102]);
        let bodies = served.join().unwrap();
        let input = |body: &str| {
            serde_json::from_str::<serde_json::Value>(body).unwrap()["input"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(input(&bodies[1]), "text: tesla");
        assert_eq!(input(&bodies[2]), "query: car");
    }

    #[test]
    fn server_ids_leave_out_the_address() {
        let ids: Vec<ModelId> = (0..2)
            .map(|_| {
                let (url, served) = fake_server(1);
                let dir = tempfile::tempdir().unwrap();
                let config = serde_json::json!({"url": url, "model": "m", "dim": 2});
                std::fs::write(dir.path().join(SERVER_FILE), config.to_string()).unwrap();
                let id = Embedder::load(dir.path()).unwrap().id();
                served.join().unwrap();
                id
            })
            .collect();
        assert_eq!(ids[0], ids[1]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SERVER_FILE), r#"{"model": "m"}"#).unwrap();
        assert!(Embedder::load(dir.path()).is_err());
    }

    #[test]
    fn a_server_running_plumbs_gemma_gives_its_id() {
        let gemma = tempfile::tempdir().unwrap();
        crate::write_test_gemma(gemma.path()).unwrap();
        let (url, served) = fake_server(1);
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({"url": url, "model": "m", "dim": GEMMA_DIM,
            "query_prefix": GEMMA_QUERY_PREFIX, "text_prefix": GEMMA_TEXT_PREFIX,
            "same_as": gemma.path()});
        std::fs::write(dir.path().join(SERVER_FILE), config.to_string()).unwrap();
        let id = Embedder::load(dir.path()).unwrap().id();
        served.join().unwrap();
        assert_eq!(id, gemma_id(gemma.path()).unwrap());
        // Only with Plumb's own prefixes.
        let config = serde_json::json!({"url": url, "model": "m", "dim": GEMMA_DIM,
            "same_as": gemma.path()});
        std::fs::write(dir.path().join(SERVER_FILE), config.to_string()).unwrap();
        assert!(Embedder::load(dir.path()).is_err());
    }
}
