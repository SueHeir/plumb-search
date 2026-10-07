//! EmbeddingGemma 2 (Google, Apache 2.0), text only, run from its GGUF file
//! the way llama.cpp runs it (`gemma-embedding2`), so the vectors match the
//! ones llama.cpp makes from the same file.
//!
//! The model: token embeddings scaled by `sqrt(n_embd)`; a per-layer input
//! projected from them; `n_layer` bidirectional layers, sliding-window
//! attention (a symmetric window) on most and full attention on the rest,
//! each with RMS-normed queries, keys and values, rotary positions, a
//! GELU-gated feed-forward, a per-layer-input branch and a learned output
//! scale; then an RMS norm, a projection to the output size and the mean
//! over the tokens.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use candle_core::quantized::{ggml_file, gguf_file, GgmlDType, QMatMul, QTensor};
use candle_core::{DType, Device, Module, Tensor, D};
use sha2::{Digest, Sha256};
use tokenizers::{Tokenizer, TruncationParams};

use crate::{quantize, ModelId};

/// The model file in a model directory.
pub const GEMMA_FILE: &str = "model.gguf";
/// The tokenizer next to it.
pub const GEMMA_TOKENIZER_FILE: &str = "tokenizer.json";
/// Most tokens of a text the model reads.
pub const GEMMA_MAX_TOKENS: usize = 512;
/// Values kept of each vector: the model is trained so that the first 256
/// of its 768 lose almost nothing.
pub const GEMMA_DIM: usize = 256;
/// What the model reads before a search, as its model card says.
pub const GEMMA_QUERY_PREFIX: &str = "task: search result | query: ";
/// What it reads before a site's text.
pub const GEMMA_TEXT_PREFIX: &str = "title: none | text: ";

/// Where a node downloads EmbeddingGemma 2 from: the file each address
/// becomes and the address (Q8_0 GGUF from ggml-org, the tokenizer from
/// Google's own repository).
pub const GEMMA_DOWNLOADS: [(&str, &str); 2] = [
    (
        GEMMA_FILE,
        "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-Q8_0.gguf",
    ),
    (
        GEMMA_TOKENIZER_FILE,
        "https://huggingface.co/google/embeddinggemma-2/resolve/914f7f89142e33e77833254d9c9b90c3cef7303b/tokenizer.json",
    ),
];

/// The [`ModelId`] of the EmbeddingGemma 2 files in `dir`: SHA-256 of the
/// SHA-256s of [`GEMMA_FILE`] and [`GEMMA_TOKENIZER_FILE`], then of
/// [`GEMMA_DIM`] and the two prefixes, which change the vectors too.
pub fn gemma_id(dir: &Path) -> Result<ModelId> {
    let mut id = Sha256::new();
    for name in [GEMMA_FILE, GEMMA_TOKENIZER_FILE] {
        let path = dir.join(name);
        let mut file =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)
            .with_context(|| format!("reading {}", path.display()))?;
        id.update(hash.finalize());
    }
    for part in [
        GEMMA_DIM.to_string().as_str(),
        GEMMA_QUERY_PREFIX,
        GEMMA_TEXT_PREFIX,
    ] {
        id.update(Sha256::digest(part.as_bytes()));
    }
    Ok(id.finalize().into())
}

/// Whether `dir` holds EmbeddingGemma 2 rather than the pinned BERT model.
pub fn is_gemma_dir(dir: &Path) -> bool {
    dir.join(GEMMA_FILE).is_file()
}

const ARCH: &str = "gemma-embedding2";

/// A weight read from the file: quantized for matrix products.
struct Linear(QMatMul);

impl Linear {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        Ok(self.0.forward(xs)?)
    }
}

/// An RMS norm with a learned weight (used as is: llama.cpp's Gemma 4
/// files carry the final weight).
struct RmsNorm {
    weight: Tensor,
    eps: f64,
}

impl RmsNorm {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        Ok(rms(xs, self.eps)?.broadcast_mul(&self.weight)?)
    }
}

/// `xs` over the root of the mean of its squares, along the last dim.
fn rms(xs: &Tensor, eps: f64) -> candle_core::Result<Tensor> {
    let mean = xs.sqr()?.mean_keepdim(D::Minus1)?;
    xs.broadcast_div(&(mean + eps)?.sqrt()?)
}

struct Layer {
    attn_norm: RmsNorm,
    wq: Linear,
    wk: Linear,
    wv: Linear,
    wo: Linear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    attn_post_norm: RmsNorm,
    ffn_norm: RmsNorm,
    ffn_gate: Linear,
    ffn_up: Linear,
    ffn_down: Linear,
    ffn_post_norm: RmsNorm,
    inp_gate: Linear,
    proj: Linear,
    post_norm: RmsNorm,
    out_scale: f32,
    n_head: usize,
    n_head_kv: usize,
    head_dim: usize,
    /// Dims of each head that turn with position (the first ones).
    n_rot: usize,
    rope_base: f32,
    /// Half the sliding window, when the layer has one.
    half_window: Option<usize>,
}

/// EmbeddingGemma 2's text model.
pub(crate) struct Gemma {
    tokens: QTensor,
    n_embd: usize,
    per_layer_proj: Linear,
    per_layer_norm: RmsNorm,
    n_per_layer: usize,
    layers: Vec<Layer>,
    output_norm: RmsNorm,
    output: Linear,
    out_dim: usize,
    eps: f64,
    tokenizer: Tokenizer,
}

/// A metadata value that is a number, or the `layer`th of an array.
fn per_layer(meta: &gguf_file::Content, key: &str, layer: usize) -> Result<Option<u64>> {
    let Some(value) = meta.metadata.get(&format!("{ARCH}.{key}")) else {
        return Ok(None);
    };
    let value = match value {
        gguf_file::Value::Array(values) => values
            .get(layer)
            .ok_or_else(|| anyhow!("{key} has no value for layer {layer}"))?,
        value => value,
    };
    let number = match value {
        gguf_file::Value::U8(v) => u64::from(*v),
        gguf_file::Value::U16(v) => u64::from(*v),
        gguf_file::Value::U32(v) => u64::from(*v),
        gguf_file::Value::U64(v) => *v,
        gguf_file::Value::I32(v) => u64::try_from(*v)?,
        gguf_file::Value::I64(v) => u64::try_from(*v)?,
        gguf_file::Value::Bool(v) => u64::from(*v),
        other => bail!("{key} is not a number: {other:?}"),
    };
    Ok(Some(number))
}

fn required(meta: &gguf_file::Content, key: &str, layer: usize) -> Result<usize> {
    per_layer(meta, key, layer)?
        .map(|v| v as usize)
        .ok_or_else(|| anyhow!("the model file has no {ARCH}.{key}"))
}

fn float(meta: &gguf_file::Content, key: &str) -> Result<Option<f64>> {
    Ok(match meta.metadata.get(&format!("{ARCH}.{key}")) {
        None => None,
        Some(value) => Some(value.to_f64().or_else(|_| value.to_f32().map(f64::from))?),
    })
}

impl Gemma {
    /// Loads the model and tokenizer in `dir`.
    pub(crate) fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(GEMMA_FILE);
        let mut file =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let meta = gguf_file::Content::read(&mut file)
            .with_context(|| format!("reading {}", path.display()))?;
        let arch = meta
            .metadata
            .get("general.architecture")
            .and_then(|v| v.to_string().ok())
            .map(String::as_str)
            .unwrap_or("");
        if arch != ARCH {
            bail!("{} is a {arch:?} model, not {ARCH}", path.display());
        }
        let device = Device::Cpu;
        let mut tensor = |name: &str| {
            meta.tensor(&mut file, name, &device)
                .with_context(|| format!("reading tensor {name}"))
        };
        let eps = float(&meta, "attention.layer_norm_rms_epsilon")?.unwrap_or(1e-6);
        let norm = |t: QTensor| -> Result<RmsNorm> {
            Ok(RmsNorm {
                weight: t.dequantize(&Device::Cpu)?.to_dtype(DType::F32)?,
                eps,
            })
        };
        let linear = |t: QTensor| -> Result<Linear> { Ok(Linear(QMatMul::from_qtensor(t)?)) };

        let n_embd = required(&meta, "embedding_length", 0)?;
        let n_layer = required(&meta, "block_count", 0)?;
        let n_per_layer = required(&meta, "embedding_length_per_layer_input", 0)?;
        let window = required(&meta, "attention.sliding_window", 0)?;
        let rope_base = float(&meta, "rope.freq_base")?.unwrap_or(1_000_000.0) as f32;
        let rope_base_swa = float(&meta, "rope.freq_base_swa")?.unwrap_or(10_000.0) as f32;

        let tokens = tensor("token_embd.weight")?;
        let per_layer_proj = linear(tensor("per_layer_model_proj.weight")?)?;
        let per_layer_norm = norm(tensor("per_layer_proj_norm.weight")?)?;
        let output_norm = norm(tensor("output_norm.weight")?)?;
        let output_tensor = tensor("output.weight")?;
        let out_dim = output_tensor.shape().dims()[0];
        let output = linear(output_tensor)?;

        let mut layers = Vec::with_capacity(n_layer);
        for il in 0..n_layer {
            let sliding =
                per_layer(&meta, "attention.sliding_window_pattern", il)?.is_some_and(|v| v != 0);
            let n_head = required(&meta, "attention.head_count", il)?;
            let n_head_kv = required(&meta, "attention.head_count_kv", il)?;
            let (head_key, rot_key) = if sliding {
                ("attention.key_length_swa", "rope.dimension_count_swa")
            } else {
                ("attention.key_length", "rope.dimension_count")
            };
            let head_dim = required(&meta, head_key, il)?;
            let n_rot = per_layer(&meta, rot_key, il)?.map_or(head_dim, |v| v as usize);
            let mut t = |name: &str| tensor(&format!("blk.{il}.{name}.weight"));
            let out_scale = t("layer_output_scale")?
                .dequantize(&Device::Cpu)?
                .to_dtype(DType::F32)?
                .flatten_all()?
                .to_vec1::<f32>()?
                .first()
                .copied()
                .ok_or_else(|| anyhow!("layer {il} has an empty output scale"))?;
            layers.push(Layer {
                attn_norm: norm(t("attn_norm")?)?,
                wq: linear(t("attn_q")?)?,
                wk: linear(t("attn_k")?)?,
                wv: linear(t("attn_v")?)?,
                wo: linear(t("attn_output")?)?,
                q_norm: norm(t("attn_q_norm")?)?,
                k_norm: norm(t("attn_k_norm")?)?,
                attn_post_norm: norm(t("post_attention_norm")?)?,
                ffn_norm: norm(t("ffn_norm")?)?,
                ffn_gate: linear(t("ffn_gate")?)?,
                ffn_up: linear(t("ffn_up")?)?,
                ffn_down: linear(t("ffn_down")?)?,
                ffn_post_norm: norm(t("post_ffw_norm")?)?,
                inp_gate: linear(t("inp_gate")?)?,
                proj: linear(t("proj")?)?,
                post_norm: norm(t("post_norm")?)?,
                out_scale,
                n_head,
                n_head_kv,
                head_dim,
                n_rot,
                rope_base: if sliding { rope_base_swa } else { rope_base },
                half_window: sliding.then_some(window / 2),
            });
        }

        let tokenizer_path = dir.join(GEMMA_TOKENIZER_FILE);
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|err| anyhow!("reading {}: {err}", tokenizer_path.display()))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: GEMMA_MAX_TOKENS,
                ..TruncationParams::default()
            }))
            .map_err(|err| anyhow!("setting up the tokenizer: {err}"))?;
        tokenizer.with_padding(None);

        Ok(Gemma {
            tokens,
            n_embd,
            per_layer_proj,
            per_layer_norm,
            n_per_layer,
            layers,
            output_norm,
            output,
            out_dim,
            eps,
            tokenizer,
        })
    }

    /// Values per vector, before any are cut.
    pub(crate) fn dim(&self) -> usize {
        self.out_dim
    }

    /// The tokens of `text`, as the model reads them.
    pub(crate) fn tokenize(&self, text: &str) -> Result<Vec<u32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|err| anyhow!("splitting text into tokens: {err}"))?;
        Ok(encoding.get_ids().to_vec())
    }

    /// The rows of the token embeddings for `ids`, dequantized one by one
    /// (the whole table is hundreds of megabytes as floats).
    fn embed_tokens(&self, ids: &[u32]) -> Result<Tensor> {
        let dtype = self.tokens.dtype();
        let dims = self.tokens.shape().dims();
        let (vocab, width) = (dims[0], dims[1]);
        let row_bytes = width / dtype.block_size() * dtype.type_size();
        let data = self.tokens.data()?;
        let mut rows = Vec::with_capacity(ids.len());
        for &id in ids {
            let id = id as usize;
            if id >= vocab {
                bail!("token {id} is outside the model's {vocab} tokens");
            }
            let bytes = &data[id * row_bytes..(id + 1) * row_bytes];
            let row = if dtype == GgmlDType::F32 {
                let values: Vec<f32> = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                Tensor::from_vec(values, width, &Device::Cpu)?
            } else {
                ggml_file::qtensor_from_ggml(dtype, bytes, vec![width], &Device::Cpu)?
                    .dequantize(&Device::Cpu)?
            };
            rows.push(row.to_dtype(DType::F32)?);
        }
        Ok(Tensor::stack(&rows, 0)?)
    }

    /// The vector of `text` after `prefix`: the model's first
    /// [`GEMMA_DIM`] values, scaled to length 1 and [`quantize`]d.
    pub(crate) fn embed(&self, prefix: &str, text: &str) -> Result<Vec<i8>> {
        let ids = self.tokenize(&format!("{prefix}{text}"))?;
        let values = self.forward(&ids)?;
        let kept = &values[..GEMMA_DIM.min(values.len())];
        // Summed in order on one thread, so the length is the same anywhere.
        let length = kept.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !length.is_normal() {
            bail!("the model gave a vector of length {length}");
        }
        let unit: Vec<f32> = kept.iter().map(|x| x / length).collect();
        Ok(quantize(&unit))
    }

    /// The model's vector of `ids` (mean over the tokens), not yet
    /// normalized.
    pub(crate) fn forward(&self, ids: &[u32]) -> Result<Vec<f32>> {
        let n = ids.len();
        if n == 0 {
            bail!("no tokens");
        }
        let device = Device::Cpu;
        // [n, n_embd]
        let mut x = (self.embed_tokens(ids)? * (self.n_embd as f64).sqrt())?;
        // [n, n_layer, n_per_layer]
        let per_layer = (self.per_layer_proj.forward(&x)? / (self.n_embd as f64).sqrt())?
            .reshape((n, self.layers.len(), self.n_per_layer))?;
        let per_layer = self.per_layer_norm.forward(&per_layer)?;
        let positions: Vec<f32> = (0..n).map(|p| p as f32).collect();
        let positions = Tensor::from_vec(positions, (n, 1), &device)?;

        for (il, layer) in self.layers.iter().enumerate() {
            let h = layer.attn_norm.forward(&x)?;
            let heads = |t: Tensor, count: usize| -> Result<Tensor> {
                // [count, n, head_dim]
                Ok(t.reshape((n, count, layer.head_dim))?.transpose(0, 1)?)
            };
            let q = heads(layer.wq.forward(&h)?, layer.n_head)?;
            let k = heads(layer.wk.forward(&h)?, layer.n_head_kv)?;
            let v = heads(layer.wv.forward(&h)?, layer.n_head_kv)?;
            let q = layer.q_norm.forward(&q)?;
            let k = layer.k_norm.forward(&k)?;
            let v = rms(&v, self.eps)?;
            let q = rope(&q, &positions, layer.n_rot, layer.rope_base)?;
            let k = rope(&k, &positions, layer.n_rot, layer.rope_base)?;
            let group = layer.n_head / layer.n_head_kv;
            let (k, v) = if group > 1 {
                (repeat_heads(&k, group)?, repeat_heads(&v, group)?)
            } else {
                (k, v)
            };
            // Gemma 4's normed queries need no 1/sqrt(d) scale.
            let mut scores = q.contiguous()?.matmul(&k.t()?.contiguous()?)?;
            if let Some(half) = layer.half_window.filter(|&half| n > half + 1) {
                let mask: Vec<f32> = (0..n * n)
                    .map(|i| {
                        let (a, b) = (i / n, i % n);
                        if a.abs_diff(b) > half {
                            f32::NEG_INFINITY
                        } else {
                            0.0
                        }
                    })
                    .collect();
                scores = scores.broadcast_add(&Tensor::from_vec(mask, (n, n), &device)?)?;
            }
            let weights = candle_nn::ops::softmax_last_dim(&scores)?;
            let attn = weights
                .matmul(&v.contiguous()?)?
                .transpose(0, 1)?
                .reshape((n, layer.n_head * layer.head_dim))?;
            let attn = layer.wo.forward(&attn)?;
            let attn_out = (layer.attn_post_norm.forward(&attn)? + &x)?;

            let h = layer.ffn_norm.forward(&attn_out)?;
            let gated = (layer.ffn_gate.forward(&h)?.gelu()? * layer.ffn_up.forward(&h)?)?;
            let ffn = layer.ffn_down.forward(&gated)?;
            let cur = (layer.ffn_post_norm.forward(&ffn)? + &attn_out)?;

            let this_layer = per_layer.narrow(1, il, 1)?.squeeze(1)?;
            let pe = (layer.inp_gate.forward(&cur)?.gelu()? * this_layer)?;
            let pe = layer.post_norm.forward(&layer.proj.forward(&pe)?)?;
            x = ((cur + pe)? * f64::from(layer.out_scale))?;
        }

        let out = self.output.forward(&self.output_norm.forward(&x)?)?;
        Ok(out.mean(0)?.to_vec1::<f32>()?)
    }
}

/// Rotary positions on the first `n_rot` dims of each head of `xs`
/// ([heads, n, head_dim]), halves paired the way llama.cpp's NEOX rope
/// pairs them.
fn rope(xs: &Tensor, positions: &Tensor, n_rot: usize, base: f32) -> Result<Tensor> {
    let head_dim = xs.dim(D::Minus1)?;
    let n_rot = n_rot.min(head_dim);
    let inv: Vec<f32> = (0..n_rot / 2)
        .map(|i| 1.0 / base.powf(2.0 * i as f32 / n_rot as f32))
        .collect();
    let inv = Tensor::from_vec(inv, (1, n_rot / 2), xs.device())?;
    let freqs = positions.matmul(&inv)?;
    let (cos, sin) = (freqs.cos()?, freqs.sin()?);
    let xs = xs.contiguous()?;
    let rotated = xs.narrow(D::Minus1, 0, n_rot)?.contiguous()?;
    let rotated = candle_nn::rotary_emb::rope(&rotated.unsqueeze(0)?, &cos, &sin)?.squeeze(0)?;
    if n_rot == head_dim {
        Ok(rotated)
    } else {
        let rest = xs.narrow(D::Minus1, n_rot, head_dim - n_rot)?;
        Ok(Tensor::cat(&[&rotated, &rest], D::Minus1)?)
    }
}

/// Each of the heads of `xs` ([heads, n, d]) `times` times in a row.
fn repeat_heads(xs: &Tensor, times: usize) -> Result<Tensor> {
    let (heads, n, d) = xs.dims3()?;
    Ok(xs
        .unsqueeze(1)?
        .expand((heads, times, n, d))?
        .reshape((heads * times, n, d))?)
}

/// Writes a tiny EmbeddingGemma 2 with made-up weights and a word-level
/// tokenizer to `dir`, for tests that need no download. Its vectors mean
/// nothing.
#[doc(hidden)]
pub fn write_test_gemma(dir: &Path) -> Result<()> {
    use gguf_file::Value;

    std::fs::create_dir_all(dir)?;
    let (n_embd, n_layer, n_ff, n_per_layer, out, head_dim) = (64, 2, 64, 32, 96, 32);
    let words = [
        "electric", "car", "maker", "bank", "credit", "union", "news", "tesla", "task:", "search",
        "result", "|", "query:", "title:", "none", "text:",
    ];
    let vocab = words.len() + 3;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut values = |count: usize, around: f32| -> Vec<f32> {
        (0..count)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                around + ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.4
            })
            .collect()
    };
    let device = Device::Cpu;
    let mut tensors: Vec<(String, QTensor)> = Vec::new();
    let mut add = |name: &str, dims: &[usize], quantized: bool, around: f32| -> Result<()> {
        let data = values(dims.iter().product(), around);
        let t = Tensor::from_vec(data, dims, &device)?;
        let dtype = if quantized {
            GgmlDType::Q8_0
        } else {
            GgmlDType::F32
        };
        tensors.push((name.to_string(), QTensor::quantize(&t, dtype)?));
        Ok(())
    };
    add("token_embd.weight", &[vocab, n_embd], true, 0.0)?;
    add(
        "per_layer_model_proj.weight",
        &[n_per_layer * n_layer, n_embd],
        true,
        0.0,
    )?;
    add("per_layer_proj_norm.weight", &[n_per_layer], false, 1.0)?;
    add("output_norm.weight", &[n_embd], false, 1.0)?;
    add("output.weight", &[out, n_embd], true, 0.0)?;
    for il in 0..n_layer {
        let b = |name: &str| format!("blk.{il}.{name}.weight");
        add(&b("attn_norm"), &[n_embd], false, 1.0)?;
        add(&b("attn_q"), &[2 * head_dim, n_embd], true, 0.0)?;
        add(&b("attn_k"), &[head_dim, n_embd], true, 0.0)?;
        add(&b("attn_v"), &[head_dim, n_embd], true, 0.0)?;
        add(&b("attn_output"), &[n_embd, 2 * head_dim], true, 0.0)?;
        add(&b("attn_q_norm"), &[head_dim], false, 1.0)?;
        add(&b("attn_k_norm"), &[head_dim], false, 1.0)?;
        add(&b("post_attention_norm"), &[n_embd], false, 1.0)?;
        add(&b("ffn_norm"), &[n_embd], false, 1.0)?;
        add(&b("ffn_gate"), &[n_ff, n_embd], true, 0.0)?;
        add(&b("ffn_up"), &[n_ff, n_embd], true, 0.0)?;
        add(&b("ffn_down"), &[n_embd, n_ff], true, 0.0)?;
        add(&b("post_ffw_norm"), &[n_embd], false, 1.0)?;
        add(&b("inp_gate"), &[n_per_layer, n_embd], true, 0.0)?;
        add(&b("proj"), &[n_embd, n_per_layer], true, 0.0)?;
        add(&b("post_norm"), &[n_embd], false, 1.0)?;
        add(&b("layer_output_scale"), &[1], false, 0.5)?;
    }
    let key = |k: &str| format!("{ARCH}.{k}");
    let metadata: Vec<(String, Value)> = vec![
        ("general.architecture".into(), Value::String(ARCH.into())),
        (key("embedding_length"), Value::U32(n_embd as u32)),
        (key("block_count"), Value::U32(n_layer as u32)),
        (
            key("embedding_length_per_layer_input"),
            Value::U32(n_per_layer as u32),
        ),
        (key("embedding_length_out"), Value::U32(out as u32)),
        (key("attention.head_count"), Value::U32(2)),
        (key("attention.head_count_kv"), Value::U32(1)),
        (key("attention.key_length"), Value::U32(head_dim as u32)),
        (key("attention.key_length_swa"), Value::U32(head_dim as u32)),
        (key("rope.dimension_count"), Value::U32(head_dim as u32)),
        (
            key("rope.dimension_count_swa"),
            Value::U32(head_dim as u32 / 2),
        ),
        (key("rope.freq_base"), Value::F32(1_000_000.0)),
        (key("rope.freq_base_swa"), Value::F32(10_000.0)),
        (key("attention.sliding_window"), Value::U32(4)),
        (
            key("attention.sliding_window_pattern"),
            Value::Array(vec![Value::Bool(true), Value::Bool(false)]),
        ),
        (key("attention.layer_norm_rms_epsilon"), Value::F32(1e-6)),
    ];
    let metadata: Vec<(&str, &Value)> = metadata.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let tensors: Vec<(&str, &QTensor)> = tensors.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut file = std::fs::File::create(dir.join(GEMMA_FILE))?;
    gguf_file::write(&mut file, &metadata, &tensors)?;

    let mut vocab_map = serde_json::Map::new();
    for (i, word) in ["<pad>", "<unk>", "<bos>"].iter().chain(&words).enumerate() {
        vocab_map.insert(word.to_string(), i.into());
    }
    let tokenizer = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": {"type": "Lowercase"},
        "pre_tokenizer": {"type": "WhitespaceSplit"},
        "post_processor": {"type": "TemplateProcessing",
            "single": [{"SpecialToken": {"id": "<bos>", "type_id": 0}},
                       {"Sequence": {"id": "A", "type_id": 0}}],
            "pair": [{"Sequence": {"id": "A", "type_id": 0}}],
            "special_tokens": {"<bos>": {"id": "<bos>", "ids": [2], "tokens": ["<bos>"]}}},
        "decoder": null,
        "model": {"type": "WordLevel", "unk_token": "<unk>", "vocab": vocab_map},
    });
    std::fs::write(dir.join(GEMMA_TOKENIZER_FILE), tokenizer.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cosine, Embedder};

    #[test]
    fn the_test_gemma_embeds_the_same_text_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        write_test_gemma(dir.path()).unwrap();
        assert!(is_gemma_dir(dir.path()));
        let embedder = Embedder::load(dir.path()).unwrap();
        assert_eq!(embedder.dim(), 96);
        assert_eq!(embedder.id(), gemma_id(dir.path()).unwrap());
        let a = embedder.embed("Tesla electric car maker").unwrap();
        assert_eq!(a.len(), 96);
        assert_eq!(embedder.embed("Tesla electric car maker").unwrap(), a);
        assert_eq!(
            Embedder::load(dir.path())
                .unwrap()
                .embed("Tesla electric car maker")
                .unwrap(),
            a
        );
        let b = embedder.embed("credit union bank news").unwrap();
        assert_ne!(a, b);
        // Searches read another prefix than sites' texts.
        assert_ne!(embedder.embed_query("Tesla electric car maker").unwrap(), a);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        // Longer than the sliding window, and longer than the token limit.
        let long = "electric car maker bank ".repeat(GEMMA_MAX_TOKENS);
        assert_eq!(embedder.embed(&long).unwrap().len(), 96);
        assert_eq!(embedder.embed("").unwrap().len(), 96);
    }

    #[test]
    fn gemma_ids_change_with_the_files() {
        let dir = tempfile::tempdir().unwrap();
        write_test_gemma(dir.path()).unwrap();
        let id = gemma_id(dir.path()).unwrap();
        assert_eq!(gemma_id(dir.path()).unwrap(), id);
        std::fs::write(dir.path().join(GEMMA_TOKENIZER_FILE), "{}").unwrap();
        assert_ne!(gemma_id(dir.path()).unwrap(), id);
    }

    /// Checks the port against llama.cpp: `PLUMB_EG2_DIR` holds the real
    /// model's [`GEMMA_FILE`] and [`GEMMA_TOKENIZER_FILE`] and a
    /// `reference.jsonl` of llama-server's tokens and vectors (`input`,
    /// `tokens`, `embedding`). Run with `cargo test --release -p
    /// plumb-embed llama_cpp -- --ignored`.
    #[test]
    #[ignore]
    fn the_real_gemma_matches_llama_cpp() {
        let dir = std::path::PathBuf::from(std::env::var("PLUMB_EG2_DIR").unwrap());
        let gemma = Gemma::load(&dir).unwrap();
        let reference = std::fs::read_to_string(dir.join("reference.jsonl")).unwrap();
        for line in reference.lines() {
            let line: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = line["input"].as_str().unwrap();
            let tokens: Vec<u32> = line["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_u64().unwrap() as u32)
                .collect();
            if tokens.len() <= GEMMA_MAX_TOKENS {
                assert_eq!(
                    gemma.tokenize(input).unwrap(),
                    tokens,
                    "tokens of {input:?}"
                );
            }
            let wanted: Vec<f32> = line["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap() as f32)
                .collect();
            let got = gemma.forward(&tokens).unwrap();
            assert_eq!(got.len(), wanted.len());
            let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
            let similar = |n: usize| {
                let (a, b) = (&got[..n], &wanted[..n]);
                dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
            };
            let (full, cut) = (similar(got.len()), similar(GEMMA_DIM));
            println!(
                "{} tokens: cosine {full:.5}, first {GEMMA_DIM}: {cut:.5}",
                tokens.len()
            );
            assert!(full > 0.999 && cut > 0.999, "{input:?}: {full} {cut}");
        }
    }
}
