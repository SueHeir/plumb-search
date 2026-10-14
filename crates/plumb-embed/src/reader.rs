//! The reader: a cross-encoder model reads the search and one result's text
//! together and says how well they fit. The learned ranking reads its score
//! of each of the first results (`plumb_index::learned`); Plumb's reader is
//! a 6-layer MiniLM trained on LLM grades of results (eval/README.md).
//!
//! Two kinds of open models load: BERT ones with a classifier on top
//! (Plumb's, or `cross-encoder/ms-marco-MiniLM-L6-v2`) and XLM-RoBERTa ones
//! (`BAAI/bge-reranker-base`). Each is a folder with the model's
//! `config.json`, `tokenizer.json` and `model.safetensors`.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Linear, VarBuilder};
use candle_transformers::models::{bert, xlm_roberta};
use tokenizers::{Tokenizer, TruncationParams};

/// Most tokens of a search and a result's text the model reads together,
/// as Plumb's reader was trained.
pub const READ_MAX_TOKENS: usize = 128;

enum Model {
    Bert {
        model: bert::BertModel,
        pooler: Linear,
        classifier: Linear,
    },
    Roberta(xlm_roberta::XLMRobertaForSequenceClassification),
}

/// Scores how well a result's text fits a search.
pub struct Reader {
    model: Model,
    tokenizer: Tokenizer,
    /// Whether the model reads token types (BERT does; RoBERTa has one).
    token_types: bool,
    /// The token that pads a shorter text in a batch.
    pad: u32,
}

impl Reader {
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
                max_length: READ_MAX_TOKENS,
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
            other => bail!("no reader for models of type {other:?}"),
        };
        let pad = ["[PAD]", "<pad>"]
            .into_iter()
            .find_map(|token| tokenizer.token_to_id(token))
            .unwrap_or(0);
        Ok(Reader {
            model,
            tokenizer,
            token_types,
            pad,
        })
    }

    /// How well each of `texts` fits `query`: the model's raw scores, in
    /// order, higher is better. The texts are read in one padded batch.
    pub fn score_all(&self, query: &str, texts: &[String]) -> Result<Vec<f32>> {
        let encodings = texts
            .iter()
            .map(|text| self.tokenizer.encode((query, text.as_str()), true))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| anyhow!("splitting text into tokens: {err}"))?;
        let width = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        if width == 0 {
            return Ok(Vec::new());
        }
        let (mut ids, mut types, mut mask) = (Vec::new(), Vec::new(), Vec::new());
        for encoding in &encodings {
            let len = encoding.get_ids().len();
            let pad = width - len;
            ids.extend_from_slice(encoding.get_ids());
            ids.extend(std::iter::repeat_n(self.pad, pad));
            match self.token_types {
                true => types.extend_from_slice(encoding.get_type_ids()),
                false => types.extend(std::iter::repeat_n(0, len)),
            }
            types.extend(std::iter::repeat_n(0, pad));
            mask.extend(std::iter::repeat_n(1u32, len));
            mask.extend(std::iter::repeat_n(0, pad));
        }
        let device = Device::Cpu;
        let shape = (encodings.len(), width);
        let input = Tensor::from_vec(ids, shape, &device)?;
        let types = Tensor::from_vec(types, shape, &device)?;
        let mask = Tensor::from_vec(mask, shape, &device)?;
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
        Ok(logits.flatten_all()?.to_vec1::<f32>()?)
    }
}

/// Writes a tiny BERT reader with random weights into `dir`, laid out as
/// Plumb's reader is, for tests.
pub fn write_test_reader(dir: &Path) -> Result<()> {
    let (config, tokenizer) = crate::tiny_config_and_tokenizer();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_each_text_as_it_would_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_test_reader(dir.path()).unwrap();
        let reader = Reader::load(dir.path()).unwrap();
        let query = "electric car maker";
        let texts = ["tesla electric cars".to_string(), "airline".to_string()];
        let both = reader.score_all(query, &texts).unwrap();
        let alone: Vec<f32> = texts
            .iter()
            .map(|text| reader.score_all(query, std::slice::from_ref(text)).unwrap()[0])
            .collect();
        assert_eq!(both.len(), 2);
        assert!(both.iter().all(|score| score.is_finite()));
        assert_ne!(both[0], both[1]);
        // Padding the shorter text changes its score by no more than
        // rounding.
        for (b, a) in both.iter().zip(&alone) {
            assert!((b - a).abs() < 1e-4, "{b} against {a}");
        }
        assert!(reader.score_all(query, &[]).unwrap().is_empty());
    }
}
