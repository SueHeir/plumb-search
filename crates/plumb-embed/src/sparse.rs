//! A learned sparse model: reads a whole page once and weighs every word of
//! its vocabulary for how well it would find the page, including words the
//! page never uses ("electric car" for a car maker's homepage). Used by
//! `plumb terms` to pick a site's search terms; an experiment.
//!
//! The model is a BERT masked language model trained for document-side
//! sparse retrieval, such as OpenSearch's
//! `opensearch-neural-sparse-encoding-doc-v2-mini`: a word's weight is
//! `ln(1 + relu(logit))`, at its largest over the page's tokens. Searches
//! need no model, only the words.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use candle_core::{DType, Device, Tensor, D};
use candle_nn::{layer_norm, linear, LayerNorm, Linear, Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{Tokenizer, TruncationParams};

/// The model's weights, in either of the forms models are shared in.
pub const SPARSE_WEIGHT_FILES: [&str; 2] = ["model.safetensors", "pytorch_model.bin"];

/// A word the model picked and its weight.
#[derive(Debug, Clone, PartialEq)]
pub struct WeightedWord {
    pub word: String,
    pub weight: f32,
}

/// A learned sparse model, with its masked language model head.
pub struct SparseModel {
    bert: BertModel,
    transform: Linear,
    norm: LayerNorm,
    decoder: Tensor,
    bias: Tensor,
    tokenizer: Tokenizer,
    /// Token ids that never count as words: `[CLS]`, `[SEP]`, `[PAD]`...
    special: Vec<u32>,
    vocab_size: usize,
}

impl SparseModel {
    /// Loads the model in `dir`: its `config.json`, `tokenizer.json` and
    /// one of [`SPARSE_WEIGHT_FILES`], reading at most `max_tokens` tokens
    /// of a page.
    pub fn load(dir: &Path, max_tokens: usize) -> Result<Self> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        };
        let config: Config =
            serde_json::from_slice(&read("config.json")?).context("reading the model's config")?;
        let mut tokenizer = Tokenizer::from_bytes(read("tokenizer.json")?)
            .map_err(|err| anyhow!("reading the model's tokenizer: {err}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_tokens,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("setting up the model's tokenizer: {err}"))?;
        tokenizer.with_padding(None);

        let device = Device::Cpu;
        let safetensors = dir.join(SPARSE_WEIGHT_FILES[0]);
        let vb = if safetensors.exists() {
            VarBuilder::from_buffered_safetensors(
                read(SPARSE_WEIGHT_FILES[0])?,
                DType::F32,
                &device,
            )
        } else {
            VarBuilder::from_pth(dir.join(SPARSE_WEIGHT_FILES[1]), DType::F32, &device)
        }
        .context("reading the model's weights")?;

        let bert = BertModel::load(vb.pp("bert"), &config)
            .or_else(|_| BertModel::load(vb.clone(), &config))
            .context("loading the model")?;
        let head = vb.pp("cls").pp("predictions");
        let hidden = config.hidden_size;
        let transform = linear(hidden, hidden, head.pp("transform").pp("dense"))?;
        let norm = layer_norm(
            hidden,
            config.layer_norm_eps,
            head.pp("transform").pp("LayerNorm"),
        )?;
        // The decoder's weights are usually the word embeddings, shared.
        let vocab_size = config.vocab_size;
        let decoder = head
            .pp("decoder")
            .get((vocab_size, hidden), "weight")
            .or_else(|_| {
                vb.pp("bert")
                    .pp("embeddings")
                    .pp("word_embeddings")
                    .get((vocab_size, hidden), "weight")
            })
            .context("reading the model's decoder")?;
        let bias = head
            .get(vocab_size, "bias")
            .or_else(|_| head.pp("decoder").get(vocab_size, "bias"))
            .context("reading the model's decoder bias")?;

        let special = ["[CLS]", "[SEP]", "[PAD]", "[UNK]", "[MASK]"]
            .iter()
            .filter_map(|token| tokenizer.token_to_id(token))
            .collect();
        Ok(SparseModel {
            bert,
            transform,
            norm,
            decoder,
            bias,
            tokenizer,
            special,
            vocab_size,
        })
    }

    /// The `count` heaviest whole words for `text`, heaviest first; ties go
    /// to the alphabetically first. Word pieces (`##ing`), special tokens
    /// and one-letter words are left out.
    pub fn words(&self, text: &str, count: usize) -> Result<Vec<WeightedWord>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|err| anyhow!("splitting text into tokens: {err}"))?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            bail!("the tokenizer gave no tokens");
        }
        let device = Device::Cpu;
        let input = Tensor::new(ids, &device)?.unsqueeze(0)?;
        let types = input.zeros_like()?;
        let mask = input.ones_like()?;
        let hidden = self.bert.forward(&input, &types, Some(&mask))?.squeeze(0)?;
        let hidden = self
            .norm
            .forward(&self.transform.forward(&hidden)?.gelu_erf()?)?;
        let logits = hidden
            .matmul(&self.decoder.t()?)?
            .broadcast_add(&self.bias)?;
        let best: Vec<f32> = logits.max(D::Minus2)?.to_vec1()?;
        debug_assert_eq!(best.len(), self.vocab_size);

        let mut words: Vec<WeightedWord> = best
            .iter()
            .enumerate()
            .filter(|&(_, &logit)| logit > 0.0)
            .filter(|&(id, _)| !self.special.contains(&(id as u32)))
            .filter_map(|(id, &logit)| {
                let word = self.tokenizer.id_to_token(id as u32)?;
                let whole = word.chars().count() > 1
                    && word.chars().all(|c| c.is_alphanumeric())
                    && !word.starts_with("##");
                whole.then(|| WeightedWord {
                    word,
                    weight: logit.ln_1p(),
                })
            })
            .collect();
        words.sort_by(|a, b| {
            b.weight
                .total_cmp(&a.weight)
                .then_with(|| a.word.cmp(&b.word))
        });
        words.truncate(count);
        Ok(words)
    }
}

/// Writes a tiny sparse model with random weights to `dir`, in the files
/// [`SparseModel::load`] reads, for tests that need no download. Its words
/// mean nothing.
#[doc(hidden)]
pub fn write_test_sparse_model(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let (config, tokenizer) = crate::model::tiny_config_and_tokenizer();
    std::fs::write(dir.join("config.json"), &config)?;
    std::fs::write(dir.join("tokenizer.json"), tokenizer)?;
    let config: Config = serde_json::from_str(&config)?;
    let weights = candle_nn::VarMap::new();
    let vb = VarBuilder::from_varmap(&weights, DType::F32, &Device::Cpu);
    BertModel::load(vb.pp("bert"), &config)?;
    let head = vb.pp("cls").pp("predictions");
    let hidden = config.hidden_size;
    linear(hidden, hidden, head.pp("transform").pp("dense"))?;
    layer_norm(hidden, 1e-12, head.pp("transform").pp("LayerNorm"))?;
    head.get_with_hints(config.vocab_size, "bias", candle_nn::Init::Const(1.0))?;
    weights.save(dir.join(SPARSE_WEIGHT_FILES[0]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_whole_words_heaviest_first_the_same_each_time() {
        let dir = tempfile::tempdir().unwrap();
        write_test_sparse_model(dir.path()).unwrap();
        let model = SparseModel::load(dir.path(), 64).unwrap();
        let text = "Tesla makes electric cars and solar energy for the american car buyer";
        let words = model.words(text, 5).unwrap();
        assert!(!words.is_empty() && words.len() <= 5);
        assert!(words.windows(2).all(|w| w[0].weight >= w[1].weight));
        for word in &words {
            assert!(word.word.len() > 1 && !word.word.starts_with('['));
        }
        assert_eq!(model.words(text, 5).unwrap(), words);
    }
}
