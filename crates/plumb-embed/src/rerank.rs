//! A second pass over the first results: a cross-encoder model reads the
//! search and one result's text together and says how well they fit.
//!
//! Two kinds of open models load: BERT ones with a classifier on top
//! (`cross-encoder/ms-marco-MiniLM-L6-v2`) and XLM-RoBERTa ones
//! (`BAAI/bge-reranker-base`). Each is a folder with the model's
//! `config.json`, `tokenizer.json` and `model.safetensors`.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Linear, VarBuilder};
use candle_transformers::models::{bert, xlm_roberta};
use tokenizers::{Tokenizer, TruncationParams};

/// Most tokens of a search and a result's text the model reads together.
pub const RERANK_MAX_TOKENS: usize = 256;

enum Model {
    Bert {
        model: bert::BertModel,
        pooler: Linear,
        classifier: Linear,
    },
    Roberta(xlm_roberta::XLMRobertaForSequenceClassification),
}

/// Scores how well a result's text fits a search.
pub struct Reranker {
    model: Model,
    tokenizer: Tokenizer,
    /// Whether the model reads token types (BERT does; RoBERTa has one).
    token_types: bool,
}

impl Reranker {
    /// Loads the model in `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        };
        let config = read("config.json")?;
        let kind: serde_json::Value =
            serde_json::from_slice(&config).context("reading the model's config")?;
        let model_type = kind["model_type"].as_str().unwrap_or("bert").to_string();
        let weights = read("model.safetensors")?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &Device::Cpu)
            .context("reading the model's weights")?;
        let mut tokenizer = Tokenizer::from_bytes(read("tokenizer.json")?)
            .map_err(|err| anyhow!("reading the model's tokenizer: {err}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: RERANK_MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("setting up the model's tokenizer: {err}"))?;
        tokenizer.with_padding(None);
        let (model, token_types) = match model_type.as_str() {
            "bert" => {
                let config: bert::Config =
                    serde_json::from_slice(&config).context("reading the model's config")?;
                let hidden = config.hidden_size;
                let model = bert::BertModel::load(vb.pp("bert"), &config)
                    .or_else(|_| bert::BertModel::load(vb.clone(), &config))
                    .context("loading the model")?;
                let pooler = linear(hidden, hidden, vb.pp("bert.pooler.dense"))
                    .context("loading the model's pooler")?;
                let classifier =
                    linear(hidden, 1, vb.pp("classifier")).context("loading the classifier")?;
                (
                    Model::Bert {
                        model,
                        pooler,
                        classifier,
                    },
                    true,
                )
            }
            "xlm-roberta" => {
                let config: xlm_roberta::Config =
                    serde_json::from_slice(&config).context("reading the model's config")?;
                let model = xlm_roberta::XLMRobertaForSequenceClassification::new(1, &config, vb)
                    .context("loading the model")?;
                (Model::Roberta(model), false)
            }
            other => bail!("no reranker for models of type {other:?}"),
        };
        Ok(Reranker {
            model,
            tokenizer,
            token_types,
        })
    }

    /// How well `text` fits `query`: the model's raw score, higher is
    /// better.
    pub fn score(&self, query: &str, text: &str) -> Result<f32> {
        let encoding = self
            .tokenizer
            .encode((query, text), true)
            .map_err(|err| anyhow!("splitting text into tokens: {err}"))?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            bail!("the tokenizer gave no tokens");
        }
        let device = Device::Cpu;
        let input = Tensor::new(ids, &device)?.unsqueeze(0)?;
        let types = if self.token_types {
            Tensor::new(encoding.get_type_ids(), &device)?.unsqueeze(0)?
        } else {
            input.zeros_like()?
        };
        let mask = input.ones_like()?;
        let logits = match &self.model {
            Model::Bert {
                model,
                pooler,
                classifier,
            } => {
                let output = model.forward(&input, &types, Some(&mask))?;
                let first = output.get_on_dim(1, 0)?;
                classifier.forward(&pooler.forward(&first)?.tanh()?)?
            }
            Model::Roberta(model) => model.forward(&input, &mask, &types)?,
        };
        Ok(logits.flatten_all()?.to_vec1::<f32>()?[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny BERT cross-encoder with random weights, as
    /// `cross-encoder/ms-marco-MiniLM-L6-v2` lays out its files.
    fn write_test_reranker(dir: &Path) -> Result<()> {
        let (config, tokenizer) = crate::model::tiny_config_and_tokenizer();
        std::fs::write(dir.join("config.json"), &config)?;
        std::fs::write(dir.join("tokenizer.json"), tokenizer)?;
        let config: bert::Config = serde_json::from_str(&config)?;
        let weights = candle_nn::VarMap::new();
        let vb = VarBuilder::from_varmap(&weights, DType::F32, &Device::Cpu);
        bert::BertModel::load(vb.pp("bert"), &config)?;
        linear(
            config.hidden_size,
            config.hidden_size,
            vb.pp("bert.pooler.dense"),
        )?;
        linear(config.hidden_size, 1, vb.pp("classifier"))?;
        weights.save(dir.join("model.safetensors"))?;
        Ok(())
    }

    #[test]
    fn scores_a_search_and_a_text_the_same_every_time() {
        let dir = tempfile::tempdir().unwrap();
        write_test_reranker(dir.path()).unwrap();
        let reranker = Reranker::load(dir.path()).unwrap();
        let first = reranker
            .score("electric car maker", "tesla electric cars")
            .unwrap();
        let again = reranker
            .score("electric car maker", "tesla electric cars")
            .unwrap();
        let other = reranker
            .score("electric car maker", "american airline flights")
            .unwrap();
        assert!(first.is_finite());
        assert_eq!(first, again);
        assert_ne!(first, other);
    }
}
