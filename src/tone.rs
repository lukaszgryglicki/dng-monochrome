use crate::{
    expression::{self, Expression, FunctionPolicy, FunctionReport},
    range::{Histogram, LEVELS, RangeAnalysis},
    raw::MonoImage,
};
use anyhow::Result;
use clap::ValueEnum;
use rayon::prelude::*;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Transfer {
    Srgb,
    #[default]
    Linear,
}

const TONE_BINS: usize = 1024;

#[derive(Clone, Debug, Serialize)]
pub struct ToneCurve {
    pub optimized: bool,
    pub exposure_factor: f64,
    pub input_median: f64,
    pub contrast_strength: f64,
    pub predicted_midtone_noise_display: Option<f64>,
    pub perceptual_curve: Vec<f64>,
}

impl ToneCurve {
    pub fn fit(hist: &Histogram, range: &RangeAnalysis, optimized: bool, black: f64) -> Self {
        let median = range.normalize(f64::from(hist.quantile(0.5)));
        let shadow = range.normalize(f64::from(hist.quantile(0.1)));
        let highlight = range.normalize(f64::from(hist.quantile(0.9)));
        let active = optimized && hist.min != hist.max;
        let exposure_factor = if active {
            (0..=64)
                .map(|i| 2.0f64.powf(-2.0 + f64::from(i) / 8.0))
                .min_by(|&a, &b| {
                    let score = |exposure| {
                        let middle = srgb_encode(expose(median, exposure));
                        let contrast = srgb_encode(expose(highlight, exposure))
                            - srgb_encode(expose(shadow, exposure));
                        let noise = display_noise(range, black, median, exposure).unwrap_or(0.0);
                        ((middle - 0.43) / 0.15).powi(2)
                            + 0.15 * ((0.6 - contrast).max(0.0) / 0.4).powi(2)
                            + 0.35 * ((noise - 0.10).max(0.0) / 0.10).powi(2)
                    };
                    score(a).total_cmp(&score(b))
                })
                .expect("nonempty exposure candidates")
        } else {
            1.0
        };
        let mut curve = Self {
            optimized,
            exposure_factor,
            input_median: median,
            contrast_strength: if active { 0.75 } else { 0.0 },
            predicted_midtone_noise_display: display_noise(range, black, median, exposure_factor),
            perceptual_curve: Vec::new(),
        };
        if active {
            curve.perceptual_curve =
                perceptual_curve(hist, range, black, exposure_factor, curve.contrast_strength);
        }
        curve
    }

    pub fn map(&self, x: f64) -> f64 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let linear = expose(x, self.exposure_factor);
        if self.perceptual_curve.is_empty() {
            return linear;
        }
        let position = srgb_encode(linear) * TONE_BINS as f64;
        let index = (position.floor() as usize).min(TONE_BINS - 1);
        let t = position - index as f64;
        srgb_decode(self.perceptual_curve[index] * (1.0 - t) + self.perceptual_curve[index + 1] * t)
    }
}

fn expose(x: f64, exposure: f64) -> f64 {
    exposure * x / (1.0 + (exposure - 1.0) * x)
}

fn display_noise(range: &RangeAnalysis, black: f64, x: f64, exposure: f64) -> Option<f64> {
    let span = range.upper - range.lower;
    let sigma = range.noise.sigma_at(range.lower + span * x - black)?;
    let y = expose(x, exposure);
    let display_slope = if y <= 0.003_130_8 {
        12.92
    } else {
        (1.055 / 2.4) * y.powf(1.0 / 2.4 - 1.0)
    };
    Some(sigma / span * exposure / (1.0 + (exposure - 1.0) * x).powi(2) * display_slope)
}

fn perceptual_curve(
    hist: &Histogram,
    range: &RangeAnalysis,
    black: f64,
    exposure: f64,
    strength: f64,
) -> Vec<f64> {
    let mut density = vec![0.0; TONE_BINS];
    for (code, &count) in hist.bins.iter().enumerate() {
        if count > 0 && code as f64 > range.lower && (code as f64) < range.upper {
            let z = srgb_encode(expose(range.normalize(code as f64), exposure));
            let bin = ((z * TONE_BINS as f64) as usize).min(TONE_BINS - 1);
            density[bin] += count as f64;
        }
    }
    for _ in 0..3 {
        density = (0..TONE_BINS)
            .map(|i| {
                [1.0, 4.0, 6.0, 4.0, 1.0]
                    .into_iter()
                    .enumerate()
                    .map(|(k, weight)| {
                        density[(i + k).saturating_sub(2).min(TONE_BINS - 1)] * weight / 16.0
                    })
                    .sum()
            })
            .collect();
    }
    let average = density.iter().sum::<f64>() / TONE_BINS as f64;
    if average == 0.0 {
        return Vec::new();
    }
    let weights: Vec<_> = density
        .iter()
        .map(|v| 0.15 + (v / average).sqrt())
        .collect();
    let caps: Vec<_> = (0..TONE_BINS)
        .map(|i| {
            let z = (i as f64 + 0.5) / TONE_BINS as f64;
            let y = srgb_decode(z);
            let x = y / (exposure - (exposure - 1.0) * y);
            let noise = display_noise(range, black, x, exposure).unwrap_or(0.0);
            (0.04 / noise.max(1e-9)).clamp(1.15, 3.0)
        })
        .collect();
    // A positive prior and slope bounds prevent equalization of a narrow/noisy peak into posterized tones.
    let (mut low, mut high) = (0.0, 32.0);
    for _ in 0..48 {
        let scale = (low + high) * 0.5;
        let total: f64 = weights
            .iter()
            .zip(&caps)
            .map(|(w, cap)| (scale * w).clamp(0.35, *cap))
            .sum();
        if total > TONE_BINS as f64 {
            high = scale;
        } else {
            low = scale;
        }
    }
    let scale = (low + high) * 0.5;
    let mut result = Vec::with_capacity(TONE_BINS + 1);
    let mut cumulative = 0.0;
    result.push(0.0);
    for (i, (&weight, &cap)) in weights.iter().zip(&caps).enumerate() {
        cumulative += (scale * weight).clamp(0.35, cap) / TONE_BINS as f64;
        result.push((1.0 - strength) * (i + 1) as f64 / TONE_BINS as f64 + strength * cumulative);
    }
    result[TONE_BINS] = 1.0;
    result
}

pub fn srgb_encode(x: f64) -> f64 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

pub fn srgb_decode(x: f64) -> f64 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

pub struct Rendered {
    pub png: Vec<u16>,
    pub jpeg: Vec<u8>,
    pub tone: ToneCurve,
    pub function: FunctionReport,
    pub stats: RenderStats,
}

#[derive(Debug, Serialize)]
pub struct RenderStats {
    pub png_min: u16,
    pub png_max: u16,
    pub png_occupied_codes: usize,
    pub jpeg_min: u8,
    pub jpeg_max: u8,
}

pub fn render(
    image: &MonoImage,
    hist: &Histogram,
    range: &RangeAnalysis,
    optimized: bool,
    expression: Option<&Expression>,
    policy: FunctionPolicy,
    transfer: Transfer,
) -> Result<Rendered> {
    let tone = ToneCurve::fit(hist, range, optimized, image.metadata.black_level);
    let mut values: Vec<f64> = (0..LEVELS)
        .map(|code| tone.map(range.normalize(code as f64)))
        .collect();
    let function = expression::apply(&mut values, hist, expression, policy)?;
    let png_table: Vec<u16> = values
        .par_iter()
        .map(|&x| {
            let value = match transfer {
                Transfer::Srgb => srgb_encode(x),
                Transfer::Linear => x,
            };
            (value.clamp(0.0, 1.0) * 65535.0).round() as u16
        })
        .collect();
    let jpeg_table: Vec<u8> = png_table
        .par_iter()
        .map(|&v| {
            let value = f64::from(v) / 65535.0;
            let display = match transfer {
                Transfer::Srgb => value,
                Transfer::Linear => srgb_encode(value),
            };
            (display.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect();
    let mut occupied = vec![false; LEVELS];
    let mut stats = RenderStats {
        png_min: u16::MAX,
        png_max: 0,
        png_occupied_codes: 0,
        jpeg_min: u8::MAX,
        jpeg_max: 0,
    };
    for (code, _) in hist.bins.iter().enumerate().filter(|(_, n)| **n > 0) {
        let png = png_table[code];
        let jpeg = jpeg_table[code];
        occupied[usize::from(png)] = true;
        stats.png_min = stats.png_min.min(png);
        stats.png_max = stats.png_max.max(png);
        stats.jpeg_min = stats.jpeg_min.min(jpeg);
        stats.jpeg_max = stats.jpeg_max.max(jpeg);
    }
    stats.png_occupied_codes = occupied.into_iter().filter(|v| *v).count();
    let (png, jpeg) = rayon::join(
        || {
            image
                .pixels
                .par_iter()
                .map(|&v| png_table[usize::from(v)])
                .collect()
        },
        || {
            image
                .pixels
                .par_iter()
                .map(|&v| jpeg_table[usize::from(v)])
                .collect()
        },
    );
    Ok(Rendered {
        png,
        jpeg,
        tone,
        function,
        stats,
    })
}
