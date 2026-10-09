//! A small neural ranker for [`super::train`], after "Are Neural Rankers
//! still Outperformed by Gradient Boosted Decision Trees?" (Qin et al.
//! 2021): every feature goes through a signed `log1p` and is scaled to
//! the training rows' mean and spread, noise is added to the inputs while
//! it trains, and a few ReLU layers give each row its score. It trains
//! with Adam on the gradients of the chosen [`super::Objective`], one
//! batch of searches at a time.

use serde::{Deserialize, Serialize};

use super::{gradients, TrainOptions, FEATURES};

/// One dense layer: `weights[out][in]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Layer {
    weights: Vec<Vec<f64>>,
    bias: Vec<f64>,
}

/// A neural ranking: scaled features through ReLU layers to one score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Net {
    /// The mean and spread of each transformed feature in training.
    mean: Vec<f64>,
    spread: Vec<f64>,
    /// Hidden layers, then the one that gives the score.
    layers: Vec<Layer>,
}

/// Searches per Adam step.
const BATCH: usize = 16;
/// Adam's step size.
const STEP: f64 = 3e-3;
/// Weight decay, so weights the data does not need stay small.
const DECAY: f64 = 1e-4;

/// Signed `log1p`, so counts and scores far apart in size line up.
fn squash(v: f64) -> f64 {
    v.signum() * v.abs().ln_1p()
}

/// A small deterministic random source (SplitMix64).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    fn normal(&mut self) -> f64 {
        let (a, b) = (self.uniform(), self.uniform());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }
}

impl Net {
    /// The inputs the layers read for features `x`.
    fn inputs(&self, x: &[f64]) -> Vec<f64> {
        x.iter()
            .zip(self.mean.iter().zip(&self.spread))
            .map(|(&v, (m, s))| (squash(v) - m) / s)
            .collect()
    }

    /// Every layer's output for `input`, the last being the score.
    fn forward(&self, input: Vec<f64>) -> Vec<Vec<f64>> {
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

    /// Trains a net on rows `x` with `labels`, grouped by search in
    /// `groups`, by `options`.
    pub(super) fn train(
        x: &[[f64; FEATURES.len()]],
        labels: &[f64],
        groups: &[(usize, usize)],
        options: TrainOptions,
    ) -> Net {
        let width = FEATURES.len();
        let n = x.len().max(1) as f64;
        let mut mean = vec![0.0; width];
        for row in x {
            for (m, &v) in mean.iter_mut().zip(row) {
                *m += squash(v) / n;
            }
        }
        let mut spread = vec![0.0; width];
        for row in x {
            for ((s, &v), m) in spread.iter_mut().zip(row).zip(&mean) {
                *s += (squash(v) - m).powi(2) / n;
            }
        }
        // A feature that never changes reads as 0.
        let spread = spread
            .into_iter()
            .map(|s: f64| if s > 1e-12 { s.sqrt() } else { 1.0 })
            .collect();
        let mut random = Random(options.seed);
        let mut sizes = vec![width];
        sizes.extend(std::iter::repeat_n(options.width, options.layers));
        sizes.push(1);
        let layers = sizes
            .windows(2)
            .map(|w| {
                // He initialization.
                let scale = (2.0 / w[0] as f64).sqrt();
                Layer {
                    weights: (0..w[1])
                        .map(|_| (0..w[0]).map(|_| random.normal() * scale).collect())
                        .collect(),
                    bias: vec![0.0; w[1]],
                }
            })
            .collect();
        let mut net = Net {
            mean,
            spread,
            layers,
        };
        let inputs: Vec<Vec<f64>> = x.iter().map(|row| net.inputs(row)).collect();
        let mut adam = Adam::new(&net);
        let mut order: Vec<usize> = (0..groups.len()).collect();
        for _ in 0..options.epochs {
            // Shuffle the searches (Fisher-Yates).
            for i in (1..order.len()).rev() {
                let j = (random.next() % (i as u64 + 1)) as usize;
                order.swap(i, j);
            }
            for batch in order.chunks(BATCH) {
                let mut grads = net.zeros();
                for &g in batch {
                    let (start, end) = groups[g];
                    let noisy: Vec<Vec<f64>> = inputs[start..end]
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|v| v + options.noise * random.normal())
                                .collect()
                        })
                        .collect();
                    let outs: Vec<Vec<Vec<f64>>> =
                        noisy.into_iter().map(|row| net.forward(row)).collect();
                    let scores: Vec<f64> = outs.iter().map(|o| o.last().unwrap()[0]).collect();
                    let (d_scores, _) = gradients(
                        options.objective,
                        &scores,
                        &labels[start..end],
                        &[(0, end - start)],
                    );
                    for (o, d) in outs.iter().zip(d_scores) {
                        net.backward(o, d / batch.len() as f64, &mut grads);
                    }
                }
                adam.step(&mut net, &grads);
            }
        }
        net
    }

    /// Layers shaped like this net's, all zeros.
    fn zeros(&self) -> Vec<Layer> {
        self.layers
            .iter()
            .map(|l| Layer {
                weights: l.weights.iter().map(|w| vec![0.0; w.len()]).collect(),
                bias: vec![0.0; l.bias.len()],
            })
            .collect()
    }

    /// Adds to `grads` how the loss changes with every weight, for a row
    /// whose layer outputs are `outs` and whose score changes the loss by
    /// `d_score`.
    fn backward(&self, outs: &[Vec<f64>], d_score: f64, grads: &mut [Layer]) {
        let mut d_out = vec![d_score];
        for l in (0..self.layers.len()).rev() {
            let input = &outs[l];
            let layer = &self.layers[l];
            let mut d_in = vec![0.0; input.len()];
            for (o, &d) in d_out.iter().enumerate() {
                if d == 0.0 {
                    continue;
                }
                grads[l].bias[o] += d;
                for (i, &v) in input.iter().enumerate() {
                    grads[l].weights[o][i] += d * v;
                    d_in[i] += d * layer.weights[o][i];
                }
            }
            // Back through the ReLU of the layer below (not the inputs).
            if l > 0 {
                for (d, &v) in d_in.iter_mut().zip(input) {
                    if v <= 0.0 {
                        *d = 0.0;
                    }
                }
            }
            d_out = d_in;
        }
    }
}

/// Adam's running first and second moments of every weight.
struct Adam {
    m: Vec<Layer>,
    v: Vec<Layer>,
    t: i32,
}

impl Adam {
    fn new(net: &Net) -> Self {
        Adam {
            m: net.zeros(),
            v: net.zeros(),
            t: 0,
        }
    }

    fn step(&mut self, net: &mut Net, grads: &[Layer]) {
        const B1: f64 = 0.9;
        const B2: f64 = 0.999;
        self.t += 1;
        let (c1, c2) = (1.0 - B1.powi(self.t), 1.0 - B2.powi(self.t));
        let update = |w: &mut f64, g: f64, m: &mut f64, v: &mut f64, decay: f64| {
            *m = B1 * *m + (1.0 - B1) * g;
            *v = B2 * *v + (1.0 - B2) * g * g;
            *w -= STEP * ((*m / c1) / ((*v / c2).sqrt() + 1e-8) + decay * *w);
        };
        for (l, layer) in net.layers.iter_mut().enumerate() {
            for (o, row) in layer.weights.iter_mut().enumerate() {
                for (i, w) in row.iter_mut().enumerate() {
                    update(
                        w,
                        grads[l].weights[o][i],
                        &mut self.m[l].weights[o][i],
                        &mut self.v[l].weights[o][i],
                        DECAY,
                    );
                }
            }
            for (o, b) in layer.bias.iter_mut().enumerate() {
                update(
                    b,
                    grads[l].bias[o],
                    &mut self.m[l].bias[o],
                    &mut self.v[l].bias[o],
                    0.0,
                );
            }
        }
    }
}
