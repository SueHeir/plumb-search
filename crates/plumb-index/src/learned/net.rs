//! A small neural ranking, after "Are Neural Rankers still Outperformed
//! by Gradient Boosted Decision Trees?" (Qin et al. 2021): every feature
//! goes through a signed `log1p` ([`squash`]) and is scaled to the
//! training rows' mean and spread, and a few ReLU layers give each row
//! its score. `plumb-tools train-rank` trains it.

use serde::{Deserialize, Serialize};

/// One dense layer: `weights[out][in]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub weights: Vec<Vec<f64>>,
    pub bias: Vec<f64>,
}

/// A neural ranking: scaled features through ReLU layers to one score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Net {
    /// The mean and spread of each transformed feature in training.
    pub mean: Vec<f64>,
    pub spread: Vec<f64>,
    /// Hidden layers, then the one that gives the score.
    pub layers: Vec<Layer>,
}

/// Signed `log1p`, so counts and scores far apart in size line up.
pub fn squash(v: f64) -> f64 {
    v.signum() * v.abs().ln_1p()
}

impl Net {
    /// The inputs the layers read for features `x`.
    pub fn inputs(&self, x: &[f64]) -> Vec<f64> {
        x.iter()
            .zip(self.mean.iter().zip(&self.spread))
            .map(|(&v, (m, s))| (squash(v) - m) / s)
            .collect()
    }

    /// Every layer's output for `input`, the last being the score.
    pub fn forward(&self, input: Vec<f64>) -> Vec<Vec<f64>> {
        let mut outs = vec![input];
        for (l, layer) in self.layers.iter().enumerate() {
            let last = l + 1 == self.layers.len();
            let prev = outs.last().expect("starts with the input");
            let out = layer
                .weights
                .iter()
                .zip(&layer.bias)
                .map(|(w, b)| {
                    let v = b + w.iter().zip(prev).map(|(w, x)| w * x).sum::<f64>();
                    if last {
                        v
                    } else {
                        v.max(0.0)
                    }
                })
                .collect();
            outs.push(out);
        }
        outs
    }

    /// The score of a row with features `x`; higher comes first.
    pub fn score(&self, x: &[f64]) -> f64 {
        self.forward(self.inputs(x)).last().expect("has layers")[0]
    }
}
