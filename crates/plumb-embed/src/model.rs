//! The embedding model.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use sha2::{Digest, Sha256};
use tokenizers::{Tokenizer, TruncationParams};

use crate::gemma::{
    gemma_id, is_gemma_dir, Gemma, GEMMA_DIM, GEMMA_QUERY_PREFIX, GEMMA_TEXT_PREFIX,
};
use crate::server::Server;

/// The model: BAAI's small English embedding model, 384 values per text.
pub const MODEL_NAME: &str = "BAAI/bge-small-en-v1.5";
/// Where the model's files are downloaded from: each of [`MODEL_FILES`]
/// appended.
pub const MODEL_BASE_URL: &str = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/";
/// The model's files, in the order [`model_id`] hashes them.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
/// Most tokens of a text the model reads; [`crate::MAX_TEXT_WORDS`] words
/// fit with room to spare.
pub const MAX_TOKENS: usize = 256;

/// SHA-256 of the SHA-256s of a model's [`MODEL_FILES`], in order.
pub type ModelId = [u8; 32];

/// The [`ModelId`] of the model files in `dir`.
pub fn model_id(dir: &Path) -> Result<ModelId> {
    let mut id = Sha256::new();
    for name in MODEL_FILES {
        let path = dir.join(name);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        id.update(Sha256::digest(&bytes));
    }
    Ok(id.finalize().into())
}

/// `values` (a unit vector) rounded to one byte each: `x * 127`, rounded.
pub fn quantize(values: &[f32]) -> Vec<i8> {
    values
        .iter()
        .map(|&x| (x * 127.0).round().clamp(-127.0, 127.0) as i8)
        .collect()
}

/// Turns texts into vectors with the model.
pub struct Embedder {
    runner: Runner,
    id: ModelId,
    dim: usize,
}

/// What runs the model. One per process, so its size does not matter.
#[allow(clippy::large_enum_variant)]
enum Runner {
    /// Plumb itself: the pinned model.
    Bert {
        model: BertModel,
        tokenizer: Tokenizer,
    },
    /// An embedding server ([`crate::SERVER_FILE`]), for trying other models.
    Server(Server),
    /// EmbeddingGemma 2, from its GGUF file ([`crate::GEMMA_FILE`]).
    Gemma(Gemma),
}

impl Embedder {
    /// Loads the model whose [`MODEL_FILES`] are in `dir`, or the
    /// embedding server `dir`'s [`crate::SERVER_FILE`] names.
    pub fn load(dir: &Path) -> Result<Self> {
        if is_gemma_dir(dir) {
            let id = gemma_id(dir)?;
            let gemma = Gemma::load(dir)?;
            let dim = GEMMA_DIM.min(gemma.dim());
            return Ok(Embedder {
                runner: Runner::Gemma(gemma),
                id,
                dim,
            });
        }
        if let Some((server, id, dim)) = Server::load(dir)? {
            return Ok(Embedder {
                runner: Runner::Server(server),
                id,
                dim,
            });
        }
        let id = model_id(dir)?;
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        };
        let weights = read(MODEL_FILES[2])?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &Device::Cpu)
            .context("reading the model's weights")?;
        Self::from_parts(&read(MODEL_FILES[0])?, &read(MODEL_FILES[1])?, vb, id)
    }

    /// The model of `config` (its `config.json`), `tokenizer` (its
    /// `tokenizer.json`) and weights `vb`, known as `id`.
    pub(crate) fn from_parts(
        config: &[u8],
        tokenizer: &[u8],
        vb: VarBuilder,
        id: ModelId,
    ) -> Result<Self> {
        let config: Config =
            serde_json::from_slice(config).context("reading the model's config")?;
        let mut tokenizer = Tokenizer::from_bytes(tokenizer)
            .map_err(|err| anyhow!("reading the model's tokenizer: {err}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("setting up the model's tokenizer: {err}"))?;
        tokenizer.with_padding(None);
        let dim = config.hidden_size;
        let model = BertModel::load(vb, &config).context("loading the model")?;
        Ok(Embedder {
            runner: Runner::Bert { model, tokenizer },
            id,
            dim,
        })
    }

    /// The [`ModelId`] of the model.
    pub fn id(&self) -> ModelId {
        self.id
    }

    /// Values per vector.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Most words of a site's text the model is given.
    pub fn text_words(&self) -> usize {
        match &self.runner {
            Runner::Server(server) => server.text_words(),
            _ => crate::MAX_TEXT_WORDS,
        }
    }

    /// The text of `record` this model embeds ([`crate::site_text_words`]).
    pub fn site_text(&self, record: &plumb_core::SiteRecord) -> String {
        crate::site_text_words(record, self.text_words())
    }

    /// The vector of a site's `text`: the model's output for its first
    /// token (how BAAI's models are meant to be used), scaled to length 1
    /// and [`quantize`]d. An empty text gives the vector of no words.
    pub fn embed(&self, text: &str) -> Result<Vec<i8>> {
        let (model, tokenizer) = match &self.runner {
            Runner::Bert { model, tokenizer } => (model, tokenizer),
            Runner::Server(server) => return server.embed_text(text),
            Runner::Gemma(gemma) => return gemma.embed(GEMMA_TEXT_PREFIX, text),
        };
        let encoding = tokenizer
            .encode(text, true)
            .map_err(|err| anyhow!("splitting text into tokens: {err}"))?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            bail!("the tokenizer gave no tokens");
        }
        let device = &model.device;
        let input = Tensor::new(ids, device)?.unsqueeze(0)?;
        let types = input.zeros_like()?;
        let mask = input.ones_like()?;
        let output = model.forward(&input, &types, Some(&mask))?;
        let first: Vec<f32> = output.get(0)?.get(0)?.to_vec1()?;
        // Summed in order on one thread, so the length is the same anywhere.
        let length = first.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !length.is_normal() {
            bail!("the model gave a vector of length {length}");
        }
        let unit: Vec<f32> = first.iter().map(|x| x / length).collect();
        Ok(quantize(&unit))
    }

    /// Whether searches may be embedded after BGE's instruction for search
    /// queries: only the pinned model was trained with it; other models
    /// have their own query prefix ([`Embedder::embed_query`]).
    pub fn takes_query_instruction(&self) -> bool {
        matches!(self.runner, Runner::Bert { .. })
    }

    /// The vector of a search `query`: as [`Embedder::embed`] for the
    /// pinned model; after the server's query prefix for a server.
    pub fn embed_query(&self, query: &str) -> Result<Vec<i8>> {
        match &self.runner {
            Runner::Bert { .. } => self.embed(query),
            Runner::Server(server) => server.embed_query(query),
            Runner::Gemma(gemma) => gemma.embed(GEMMA_QUERY_PREFIX, query),
        }
    }
}

/// Words the test model knows.
const TEST_WORDS: &[&str] = &[
    "electric", "car", "cars", "maker", "bank", "credit", "union", "news", "paper", "airline",
    "flights", "tesla", "solar", "energy", "american", "the", "of", "and",
];

/// The `config.json` and `tokenizer.json` of a tiny BERT model with a
/// word-level vocabulary of [`TEST_WORDS`].
pub(crate) fn tiny_config_and_tokenizer() -> (String, String) {
    let config = serde_json::json!({
        "vocab_size": TEST_WORDS.len() + 5,
        "hidden_size": 32,
        "num_hidden_layers": 2,
        "num_attention_heads": 4,
        "intermediate_size": 64,
        "hidden_act": "gelu",
        "hidden_dropout_prob": 0.0,
        "max_position_embeddings": 512,
        "type_vocab_size": 2,
        "initializer_range": 0.02,
        "layer_norm_eps": 1e-12,
        "pad_token_id": 0,
    });
    let mut vocab = serde_json::Map::new();
    for (i, word) in ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]
        .iter()
        .chain(TEST_WORDS)
        .enumerate()
    {
        vocab.insert(word.to_string(), i.into());
    }
    let tokenizer = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": {"type": "BertNormalizer", "clean_text": true,
            "handle_chinese_chars": true, "strip_accents": null, "lowercase": true},
        "pre_tokenizer": {"type": "BertPreTokenizer"},
        "post_processor": {"type": "BertProcessing", "sep": ["[SEP]", 3], "cls": ["[CLS]", 2]},
        "decoder": null,
        "model": {"type": "WordPiece", "unk_token": "[UNK]",
            "continuing_subword_prefix": "##", "max_input_chars_per_word": 100,
            "vocab": vocab},
    });
    (config.to_string(), tokenizer.to_string())
}

/// Writes a tiny model with random weights to `dir`, in the files
/// [`Embedder::load`] reads, for tests that need no download. Its vectors
/// mean nothing.
#[doc(hidden)]
pub fn write_test_model(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let (config, tokenizer) = tiny_config_and_tokenizer();
    std::fs::write(dir.join(MODEL_FILES[0]), &config)?;
    std::fs::write(dir.join(MODEL_FILES[1]), tokenizer)?;
    let config: Config = serde_json::from_str(&config)?;
    let weights = candle_nn::VarMap::new();
    BertModel::load(
        VarBuilder::from_varmap(&weights, DType::F32, &Device::Cpu),
        &config,
    )?;
    weights.save(dir.join(MODEL_FILES[2]))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cosine;
    use candle_core::Shape;
    use candle_nn::var_builder::SimpleBackend;

    /// Weights from a fixed sequence of numbers seeded by each tensor's
    /// name, so tests need no download.
    struct FixedWeights;

    impl SimpleBackend for FixedWeights {
        fn get(
            &self,
            shape: Shape,
            name: &str,
            _: candle_nn::Init,
            dtype: DType,
            dev: &Device,
        ) -> candle_core::Result<Tensor> {
            let mut state = name.bytes().fold(0x9e37_79b9_7f4a_7c15_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
            });
            let values: Vec<f32> = (0..shape.elem_count())
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.2
                })
                .collect();
            Tensor::from_vec(values, shape, dev)?.to_dtype(dtype)
        }

        fn get_unchecked(&self, name: &str, _: DType, _: &Device) -> candle_core::Result<Tensor> {
            candle_core::bail!("no shape for {name}")
        }

        fn contains_tensor(&self, _: &str) -> bool {
            true
        }
    }

    /// A tiny BERT model with made-up weights and a word-level vocabulary.
    pub(crate) fn tiny_embedder() -> Embedder {
        let (config, tokenizer) = tiny_config_and_tokenizer();
        let vb = VarBuilder::from_backend(Box::new(FixedWeights), DType::F32, Device::Cpu);
        Embedder::from_parts(config.as_bytes(), tokenizer.as_bytes(), vb, [7; 32]).unwrap()
    }

    #[test]
    fn the_same_text_gives_the_same_bytes() {
        let embedder = tiny_embedder();
        assert_eq!(embedder.dim(), 32);
        let a = embedder.embed("American electric car maker").unwrap();
        assert_eq!(a.len(), 32);
        // Again, after other texts, and from a second copy of the model.
        embedder.embed("the bank of credit").unwrap();
        assert_eq!(embedder.embed("American electric car maker").unwrap(), a);
        assert_eq!(
            tiny_embedder()
                .embed("American electric car maker")
                .unwrap(),
            a
        );
        // Case is the tokenizer's business; other words differ.
        assert_eq!(embedder.embed("american ELECTRIC car maker").unwrap(), a);
        let b = embedder.embed("credit union bank").unwrap();
        assert_ne!(a, b);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        // Long texts are cut, not refused.
        let long = "electric car ".repeat(MAX_TOKENS);
        assert_eq!(embedder.embed(&long).unwrap().len(), 32);
        assert_eq!(embedder.embed("").unwrap().len(), 32);
    }

    #[test]
    fn quantizing_rounds_to_bytes() {
        assert_eq!(
            quantize(&[1.0, -1.0, 0.5, 0.0, 0.003_9, 2.0]),
            [127, -127, 64, 0, 0, 127]
        );
    }

    #[test]
    fn the_test_model_loads_from_files() {
        let dir = tempfile::tempdir().unwrap();
        write_test_model(dir.path()).unwrap();
        let embedder = Embedder::load(dir.path()).unwrap();
        assert_eq!(embedder.id(), model_id(dir.path()).unwrap());
        let a = embedder.embed("electric car maker").unwrap();
        assert_eq!(
            Embedder::load(dir.path())
                .unwrap()
                .embed("electric car maker")
                .unwrap(),
            a
        );
    }

    #[test]
    fn model_ids_hash_every_file() {
        let dir = tempfile::tempdir().unwrap();
        for name in MODEL_FILES {
            std::fs::write(dir.path().join(name), name).unwrap();
        }
        let id = model_id(dir.path()).unwrap();
        assert_eq!(model_id(dir.path()).unwrap(), id);
        std::fs::write(dir.path().join("tokenizer.json"), "other").unwrap();
        assert_ne!(model_id(dir.path()).unwrap(), id);
        std::fs::remove_file(dir.path().join("config.json")).unwrap();
        assert!(model_id(dir.path()).is_err());
    }
}
