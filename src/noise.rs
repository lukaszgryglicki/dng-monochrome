use crate::{range::Histogram, raw::MonoImage};
use rayon::prelude::*;
use serde::Serialize;

const SIDE: usize = 16;
const AREA: usize = SIDE * SIDE;
const MAX_PATCHES: usize = 12_000;
const NORMAL_MAD: f64 = 0.674_489_750_196_081_7;
const QUANTIZATION_VARIANCE: f64 = 1.0 / 12.0;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Moderate,
    Low,
    #[default]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct NoiseModel {
    pub shot_coefficient: f64,
    pub read_variance: f64,
    pub source: &'static str,
}

impl NoiseModel {
    pub fn variance(self, signal: f64) -> f64 {
        self.shot_coefficient * signal.max(0.0) + self.read_variance
    }

    pub fn signal_for_snr(self, snr: f64) -> f64 {
        let a = snr * snr * self.shot_coefficient;
        0.5 * a + 0.5 * a.hypot(2.0 * snr * self.read_variance.sqrt())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct NoiseBin {
    pub signal_codes: f64,
    pub sigma_codes: f64,
    pub patches: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NoiseEstimate {
    pub dark_sigma_codes: Option<f64>,
    pub light_sigma_codes: Option<f64>,
    pub dark_samples: usize,
    pub light_samples: usize,
    pub patch_side: usize,
    pub patches_examined: usize,
    pub censored_patches: usize,
    pub weak_texture_patches: usize,
    pub shadow_signal_codes: Option<f64>,
    pub highlight_signal_codes: Option<f64>,
    pub correlation_ratio: Option<f64>,
    pub model: Option<NoiseModel>,
    pub read_noise_resolved: bool,
    pub read_sigma_fit_sensitivity_codes: Option<[f64; 2]>,
    pub relative_fit_error: Option<f64>,
    pub confidence: Confidence,
    pub bins: Vec<NoiseBin>,
    pub notes: Vec<String>,
}

impl NoiseEstimate {
    pub fn sigma_at(&self, signal: f64) -> Option<f64> {
        if let Some(model) = self
            .model
            .filter(|m| m.source != "dng_noise_profile" || self.read_noise_resolved)
        {
            return Some(model.variance(signal).max(QUANTIZATION_VARIANCE).sqrt());
        }
        let first = self.bins.first()?;
        let mut previous = first;
        for next in self.bins.iter().skip(1) {
            if signal <= next.signal_codes {
                let span = next.signal_codes - previous.signal_codes;
                let t = if span > 0.0 {
                    ((signal - previous.signal_codes) / span).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                return Some(
                    (previous.sigma_codes.powi(2) * (1.0 - t) + next.sigma_codes.powi(2) * t)
                        .sqrt(),
                );
            }
            previous = next;
        }
        Some(previous.sigma_codes)
    }

    pub fn signal_for_snr(&self, snr: f64) -> Option<f64> {
        if self.read_noise_resolved
            && let Some(model) = self.model
        {
            return Some(model.signal_for_snr(snr).max(1.0));
        }
        self.dark_sigma_codes
            .filter(|v| *v >= 0.5)
            .map(|sigma| (snr * sigma).max(1.0))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DynamicRange {
    pub method: &'static str,
    pub confidence: Confidence,
    pub snr1_stops: Option<f64>,
    pub snr3_stops: Option<f64>,
    pub snr1_floor_codes: Option<f64>,
    pub snr3_floor_codes: Option<f64>,
    pub highlight_signal_codes: f64,
    pub selected_scene_contrast_stops: Option<f64>,
    pub fit_sensitivity_stops: Option<[f64; 2]>,
}

impl DynamicRange {
    pub fn estimate(
        noise: &NoiseEstimate,
        black: f64,
        lower: f64,
        upper: f64,
        has_range: bool,
    ) -> Self {
        let ceiling = (upper - black).max(0.0);
        let floor1 = has_range.then(|| noise.signal_for_snr(1.0)).flatten();
        let floor3 = has_range.then(|| noise.signal_for_snr(3.0)).flatten();
        let stops = |floor: f64| (ceiling / floor).max(1.0).log2();
        let sensitivity = noise
            .read_sigma_fit_sensitivity_codes
            .filter(|_| noise.read_noise_resolved && has_range)
            .and_then(|bounds| {
                noise.model.map(|model| {
                    bounds.map(|sigma| {
                        let model = NoiseModel {
                            read_variance: sigma * sigma,
                            ..model
                        };
                        stops(model.signal_for_snr(1.0).max(1.0))
                    })
                })
            })
            .map(|bounds| [bounds[1], bounds[0]]);
        Self {
            method: if noise.read_noise_resolved {
                "poisson_gaussian_model"
            } else if floor1.is_some() {
                "measured_shadow_noise_proxy"
            } else {
                "unavailable"
            },
            confidence: if floor1.is_some() {
                noise.confidence
            } else {
                Confidence::Unavailable
            },
            snr1_stops: floor1.map(stops),
            snr3_stops: floor3.map(stops),
            snr1_floor_codes: floor1,
            snr3_floor_codes: floor3,
            highlight_signal_codes: ceiling,
            selected_scene_contrast_stops: floor3.map(|floor| stops((lower - black).max(floor))),
            fit_sensitivity_stops: sensitivity,
        }
    }
}

#[derive(Clone, Copy)]
struct Patch {
    signal: f64,
    variance: f64,
    correlation: f64,
    texture: f64,
    lag_growth: f64,
}

fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    let position = (values.len() - 1) as f64 * fraction;
    let index = position.floor() as usize;
    let (_, value, higher) = values.select_nth_unstable_by(index, f64::total_cmp);
    let value = *value;
    if position.fract() == 0.0 || higher.is_empty() {
        value
    } else {
        value + position.fract() * (higher.iter().copied().fold(f64::INFINITY, f64::min) - value)
    }
}

fn patch(image: &MonoImage, x: usize, y: usize) -> Option<Patch> {
    let width = image.metadata.width;
    let white = image.metadata.white_level;
    let center = (SIDE - 1) as f64 / 2.0;
    let mut values = [0.0; AREA];
    let (mut sum, mut sx, mut sy) = (0.0, 0.0, 0.0);
    for row in 0..SIDE {
        for col in 0..SIDE {
            let value = image.pixels[(y + row) * width + x + col];
            if value == 0 || value >= white {
                return None;
            }
            let value = f64::from(value);
            values[row * SIDE + col] = value;
            sum += value;
            sx += (col as f64 - center) * value;
            sy += (row as f64 - center) * value;
        }
    }
    let mean = sum / AREA as f64;
    let coordinate_energy = (AREA * (SIDE * SIDE - 1)) as f64 / 12.0;
    let (bx, by) = (sx / coordinate_energy, sy / coordinate_energy);
    let mut residuals = [0.0; AREA];
    for row in 0..SIDE {
        for col in 0..SIDE {
            residuals[row * SIDE + col] = (values[row * SIDE + col]
                - mean
                - bx * (col as f64 - center)
                - by * (row as f64 - center))
                .abs();
        }
    }
    let sigma =
        percentile(&mut residuals, 0.5) / NORMAL_MAD * (AREA as f64 / (AREA - 3) as f64).sqrt();
    let mut haar = [0.0; 3];
    for (index, lag) in [1, 2, 4].into_iter().enumerate() {
        let mut count = 0;
        for row in 0..SIDE - lag {
            for col in 0..SIDE - lag {
                let offset = row * SIDE + col;
                residuals[count] =
                    (values[offset] - values[offset + lag] - values[offset + lag * SIDE]
                        + values[offset + lag * SIDE + lag])
                        .abs()
                        * 0.5;
                count += 1;
            }
        }
        haar[index] = percentile(&mut residuals[..count], 0.5) / NORMAL_MAD;
    }
    let reference = haar[1].max(haar[2]).max(QUANTIZATION_VARIANCE.sqrt());
    Some(Patch {
        signal: (mean - image.metadata.black_level).max(0.0),
        variance: sigma * sigma,
        correlation: sigma / haar[0].max(QUANTIZATION_VARIANCE.sqrt()),
        texture: sigma / reference,
        lag_growth: haar[2] / haar[1].max(QUANTIZATION_VARIANCE.sqrt()),
    })
}

fn summarize(patches: &[Patch], model: Option<NoiseModel>) -> NoiseBin {
    let mut means: Vec<_> = patches.iter().map(|p| p.signal).collect();
    let signal = percentile(&mut means, 0.5);
    let mut variances: Vec<_> = patches
        .iter()
        .map(|p| {
            p.variance / model.map_or(1.0, |m| m.variance(p.signal).max(QUANTIZATION_VARIANCE))
        })
        .collect();
    // Correct the lower quartile's sampling bias for a Gaussian MAD with 253 residual degrees of freedom.
    let correction = (1.0 - NORMAL_MAD * (1.3605 / (AREA - 3) as f64).sqrt()).powi(2);
    let variance = percentile(&mut variances, 0.25) / correction
        * model.map_or(1.0, |m| m.variance(signal).max(QUANTIZATION_VARIANCE));
    NoiseBin {
        signal_codes: signal,
        sigma_codes: variance.sqrt(),
        patches: patches.len(),
    }
}

fn fit_model(bins: &[NoiseBin]) -> Option<(NoiseModel, f64)> {
    if bins.len() < 6 || bins.iter().any(|p| p.sigma_codes < 0.5) {
        return None;
    }
    let span = bins.last()?.signal_codes - bins.first()?.signal_codes;
    if span < 16.0 {
        return None;
    }
    let mut slopes = Vec::new();
    for (i, left) in bins.iter().enumerate() {
        for right in &bins[i + 1..] {
            let delta = right.signal_codes - left.signal_codes;
            if delta > 0.15 * span {
                slopes.push((right.sigma_codes.powi(2) - left.sigma_codes.powi(2)) / delta);
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    let mut a = percentile(&mut slopes, 0.5);
    let mut intercepts: Vec<_> = bins
        .iter()
        .map(|p| p.sigma_codes.powi(2) - a * p.signal_codes)
        .collect();
    let mut b = percentile(&mut intercepts, 0.5);
    let minimum_variance = bins
        .iter()
        .map(|p| p.sigma_codes.powi(2))
        .fold(f64::INFINITY, f64::min)
        .max(QUANTIZATION_VARIANCE);
    for _ in 0..4 {
        let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for bin in bins {
            let x = bin.signal_codes / span;
            let y = bin.sigma_codes.powi(2).max(QUANTIZATION_VARIANCE);
            let residual = ((y - (a * bin.signal_codes + b)) / y).abs();
            let weight = (minimum_variance / y).powi(2) * (0.15 / residual.max(0.15));
            sw += weight;
            sx += weight * x;
            sy += weight * y;
            sxx += weight * x * x;
            sxy += weight * x * y;
        }
        let determinant = sw * sxx - sx * sx;
        if determinant <= f64::EPSILON {
            return None;
        }
        a = (sw * sxy - sx * sy) / determinant / span;
        b = (sy - a * span * sx) / sw;
    }
    if a < 0.0 {
        let mut variances: Vec<_> = bins.iter().map(|p| p.sigma_codes.powi(2)).collect();
        let center = percentile(&mut variances, 0.5);
        if -a * span > 0.15 * center {
            return None;
        }
        a = 0.0;
        b = center;
    }
    if !a.is_finite() || !b.is_finite() || b < QUANTIZATION_VARIANCE {
        return None;
    }
    let model = NoiseModel {
        shot_coefficient: a,
        read_variance: b,
        source: "weak_texture_patches",
    };
    let mut errors: Vec<_> = bins
        .iter()
        .map(|p| {
            (p.sigma_codes.powi(2) - model.variance(p.signal_codes)).abs()
                / p.sigma_codes.powi(2).max(QUANTIZATION_VARIANCE)
        })
        .collect();
    let error = percentile(&mut errors, 0.5);
    (error <= 0.25).then_some((model, error))
}

pub fn estimate_noise(image: &MonoImage, hist: &Histogram) -> NoiseEstimate {
    let (width, height) = (image.metadata.width, image.metadata.height);
    let mut estimate = NoiseEstimate {
        patch_side: SIDE,
        ..NoiseEstimate::default()
    };
    if let Some([shot, read]) = image.metadata.noise_profile {
        let span = f64::from(image.metadata.white_level) - image.metadata.black_level;
        estimate.model = Some(NoiseModel {
            shot_coefficient: shot * span,
            read_variance: read * span * span,
            source: "dng_noise_profile",
        });
        estimate.read_noise_resolved = true;
        estimate.confidence = Confidence::Moderate;
    }
    if width < SIDE || height < SIDE {
        estimate
            .notes
            .push("Image too small for spatial noise estimation.".into());
        return estimate;
    }
    let step = ((image.pixels.len() as f64 / MAX_PATCHES as f64)
        .sqrt()
        .ceil() as usize)
        .max(SIDE);
    let cols = (width - SIDE) / step + 1;
    let rows = (height - SIDE) / step + 1;
    let samples: Vec<_> = (0..cols * rows)
        .into_par_iter()
        .map(|i| patch(image, (i % cols) * step, (i / cols) * step))
        .collect();
    estimate.patches_examined = samples.len();
    let mut valid: Vec<_> = samples.into_iter().flatten().collect();
    estimate.censored_patches = estimate.patches_examined - valid.len();
    valid.sort_unstable_by(|a, b| a.signal.total_cmp(&b.signal));
    let weak: Vec<_> = valid
        .iter()
        .filter(|p| p.texture <= 1.5 && p.lag_growth <= 1.35)
        .copied()
        .collect();
    estimate.weak_texture_patches = weak.len();
    let candidates = if weak.len() >= 32 { &weak } else { &valid };
    if candidates.len() < 32 {
        estimate
            .notes
            .push("Too few uncensored patches; spatial noise is unavailable.".into());
        return estimate;
    }
    let count = (candidates.len() / 48).clamp(1, 16);
    let chunk = candidates.len().div_ceil(count);
    let calibrated = estimate.model.is_some();
    estimate.bins = candidates
        .chunks(chunk)
        .map(|p| summarize(p, None))
        .collect();
    if !calibrated && weak.len() >= 32 {
        for _ in 0..3 {
            if let Some((model, error)) = fit_model(&estimate.bins) {
                estimate.model = Some(model);
                estimate.relative_fit_error = Some(error);
                estimate.bins = candidates
                    .chunks(chunk)
                    .map(|p| summarize(p, Some(model)))
                    .collect();
            } else {
                estimate.model = None;
                estimate.relative_fit_error = None;
            }
        }
        if let Some((model, error)) = fit_model(&estimate.bins) {
            estimate.model = Some(model);
            estimate.relative_fit_error = Some(error);
            let mut sigmas = Vec::new();
            for excluded in 0..estimate.bins.len() {
                let subset: Vec<_> = estimate
                    .bins
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != excluded)
                    .map(|(_, p)| p.clone())
                    .collect();
                if let Some((model, _)) = fit_model(&subset) {
                    sigmas.push(model.read_variance.sqrt());
                }
            }
            if sigmas.len() * 5 >= estimate.bins.len() * 4 {
                let bounds = [percentile(&mut sigmas, 0.1), percentile(&mut sigmas, 0.9)];
                estimate.read_sigma_fit_sensitivity_codes = Some(bounds);
                let first = estimate.bins.first().expect("nonempty bins").signal_codes;
                let last = estimate.bins.last().expect("nonempty bins").signal_codes;
                estimate.read_noise_resolved = bounds[1] <= 3.0 * bounds[0]
                    && (first <= 0.35 * (last - first)
                        || first <= 6.0 * model.read_variance.sqrt());
            }
        } else {
            estimate.model = None;
            estimate.relative_fit_error = None;
        }
    }
    let tail_count = (candidates.len() / 5).max(32).min(candidates.len());
    let dark = summarize(&candidates[..tail_count], estimate.model);
    let light = summarize(&candidates[candidates.len() - tail_count..], estimate.model);
    estimate.dark_sigma_codes = (dark.sigma_codes >= 0.5).then_some(dark.sigma_codes);
    estimate.light_sigma_codes = (light.sigma_codes >= 0.5).then_some(light.sigma_codes);
    estimate.dark_samples = tail_count;
    estimate.light_samples = tail_count;
    estimate.shadow_signal_codes = Some(dark.signal_codes);
    estimate.highlight_signal_codes = Some(light.signal_codes);
    let mut correlations: Vec<_> = candidates
        .iter()
        .filter(|p| p.variance >= 0.25)
        .map(|p| p.correlation)
        .collect();
    if !correlations.is_empty() {
        estimate.correlation_ratio = Some(percentile(&mut correlations, 0.5));
    }
    estimate.confidence = if estimate.read_noise_resolved {
        Confidence::Moderate
    } else if estimate.dark_sigma_codes.is_some() {
        Confidence::Low
    } else {
        Confidence::Unavailable
    };
    if !estimate.read_noise_resolved {
        estimate.notes.push("Read-noise extrapolation is unsupported; DR uses measured shadow noise, which includes shot noise and possibly texture.".into());
    }
    if weak.len() < 32 {
        estimate.notes.push(
            "Too few weak-texture patches; the conservative proxy includes image structure.".into(),
        );
    }
    if estimate.correlation_ratio.is_some_and(|v| v > 1.3) {
        estimate.notes.push("Spatial correlation detected: detrended patch noise is used instead of assuming independent adjacent pixels.".into());
    }
    if let Some(model) = estimate.model.filter(|_| calibrated) {
        let mut ratios: Vec<_> = estimate
            .bins
            .iter()
            .map(|p| {
                p.sigma_codes
                    / model
                        .variance(p.signal_codes)
                        .max(QUANTIZATION_VARIANCE)
                        .sqrt()
            })
            .collect();
        let ratio = percentile(&mut ratios, 0.5);
        if !(0.5..=2.0).contains(&ratio) {
            estimate.confidence = Confidence::Low;
            estimate.read_noise_resolved = false;
            estimate.notes.push("DNG NoiseProfile disagrees with spatial observations by more than 2x; DR uses the measured shadow proxy.".into());
        }
    }
    if hist.min == hist.max {
        estimate.confidence = Confidence::Unavailable;
    }
    estimate
}
