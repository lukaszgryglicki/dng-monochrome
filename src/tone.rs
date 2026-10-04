use crate::{
    expression::{self, Expression, FunctionPolicy, FunctionReport},
    parameters::Parameters,
    range::{Histogram, LEVELS, RangeAnalysis},
    raw::MonoImage,
};
use anyhow::{Result, ensure};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Transfers {
    pub png: Transfer,
    pub jpeg: Transfer,
}

impl Default for Transfers {
    fn default() -> Self {
        Self {
            png: Transfer::Linear,
            jpeg: Transfer::Linear,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ToneCurve {
    pub optimized: bool,
    pub optimize_strength: f64,
    pub exposure_factor: f64,
    pub input_median: f64,
    pub contrast_strength: f64,
    pub predicted_midtone_noise_display: Option<f64>,
    pub perceptual_curve: Vec<f64>,
}

impl ToneCurve {
    pub fn fit(hist: &Histogram, range: &RangeAnalysis, optimized: bool, black: f64) -> Self {
        Self::fit_with_parameters(hist, range, optimized, black, &Parameters::default())
            .expect("default tone settings must produce a normalized monotonic curve")
    }

    pub fn fit_with_parameters(
        hist: &Histogram,
        range: &RangeAnalysis,
        optimized: bool,
        black: f64,
        parameters: &Parameters,
    ) -> Result<Self> {
        parameters.validate()?;
        let median = range.normalize(f64::from(hist.quantile(parameters.tone_midtone_quantile)));
        let shadow = range.normalize(f64::from(hist.quantile(parameters.tone_shadow_quantile)));
        let highlight =
            range.normalize(f64::from(hist.quantile(parameters.tone_highlight_quantile)));
        let active = optimized && parameters.optimize_strength > 0.0 && hist.min != hist.max;
        let mut exposure_factor = if active {
            let steps = ((parameters.tone_max_exposure_ev - parameters.tone_min_exposure_ev)
                * parameters.tone_exposure_steps as f64)
                .ceil() as usize;
            (0..=steps)
                .map(|i| {
                    2.0f64.powf(
                        (parameters.tone_min_exposure_ev
                            + i as f64 / parameters.tone_exposure_steps as f64)
                            .min(parameters.tone_max_exposure_ev),
                    )
                })
                .min_by(|&a, &b| {
                    let score = |exposure| {
                        let middle = srgb_encode(expose(median, exposure));
                        let contrast = srgb_encode(expose(highlight, exposure))
                            - srgb_encode(expose(shadow, exposure));
                        let noise = display_noise(range, black, median, exposure).unwrap_or(0.0);
                        ((middle - parameters.tone_target_midtone)
                            / parameters.tone_midtone_tolerance)
                            .powi(2)
                            + parameters.tone_contrast_weight
                                * ((parameters.tone_target_contrast - contrast).max(0.0)
                                    / parameters.tone_contrast_tolerance)
                                    .powi(2)
                            + parameters.tone_noise_weight
                                * ((noise - parameters.tone_noise_threshold).max(0.0)
                                    / parameters.tone_noise_tolerance)
                                    .powi(2)
                    };
                    score(a).total_cmp(&score(b))
                })
                .expect("nonempty exposure candidates")
        } else {
            1.0
        };
        if active && parameters.tone_exposure_bias != 0.0 {
            exposure_factor *= parameters.tone_exposure_bias.exp2();
        }
        let mut curve = Self {
            optimized,
            optimize_strength: if active {
                parameters.optimize_strength
            } else {
                0.0
            },
            exposure_factor,
            input_median: median,
            contrast_strength: if active {
                parameters.tone_contrast_strength
            } else {
                0.0
            },
            predicted_midtone_noise_display: display_noise(range, black, median, exposure_factor),
            perceptual_curve: Vec::new(),
        };
        if active {
            curve.perceptual_curve = perceptual_curve(
                hist,
                range,
                black,
                exposure_factor,
                curve.contrast_strength,
                parameters,
            )?;
        }
        Ok(curve)
    }

    pub fn map(&self, x: f64) -> f64 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let linear = expose(x, self.exposure_factor);
        let mapped = if self.perceptual_curve.is_empty() {
            linear
        } else {
            let bins = self.perceptual_curve.len() - 1;
            let position = srgb_encode(linear) * bins as f64;
            let index = (position.floor() as usize).min(bins - 1);
            let t = position - index as f64;
            srgb_decode(
                self.perceptual_curve[index] * (1.0 - t) + self.perceptual_curve[index + 1] * t,
            )
        };
        if self.optimize_strength == 1.0 || !self.optimized {
            mapped
        } else if self.optimize_strength == 0.0 {
            x
        } else {
            (1.0 - self.optimize_strength) * x + self.optimize_strength * mapped
        }
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
    parameters: &Parameters,
) -> Result<Vec<f64>> {
    if strength == 0.0 {
        return Ok(Vec::new());
    }
    let bins = parameters.tone_bins;
    let mut density = vec![0.0; bins];
    for (code, &count) in hist.bins.iter().enumerate() {
        if count > 0 && code as f64 > range.lower && (code as f64) < range.upper {
            let z = srgb_encode(expose(range.normalize(code as f64), exposure));
            let bin = ((z * bins as f64) as usize).min(bins - 1);
            density[bin] += count as f64;
        }
    }
    let radius = parameters.tone_smoothing_radius;
    let mut kernel = vec![1.0; 2 * radius + 1];
    for i in 1..kernel.len() {
        kernel[i] = kernel[i - 1] * (kernel.len() - i) as f64 / i as f64;
    }
    let normalization = 2.0f64.powi((2 * radius) as i32);
    for _ in 0..parameters.tone_smoothing_passes {
        density = (0..bins)
            .map(|i| {
                kernel
                    .iter()
                    .enumerate()
                    .map(|(k, &weight)| {
                        density[(i + k).saturating_sub(radius).min(bins - 1)] * weight
                            / normalization
                    })
                    .sum()
            })
            .collect();
    }
    let average = density.iter().sum::<f64>() / bins as f64;
    if average == 0.0 {
        return Ok(Vec::new());
    }
    let weights: Vec<_> = density
        .iter()
        .map(|v| {
            parameters.tone_density_prior
                + if parameters.tone_density_power == 0.5 {
                    (v / average).sqrt()
                } else {
                    (v / average).powf(parameters.tone_density_power)
                }
        })
        .collect();
    let caps: Vec<_> = (0..bins)
        .map(|i| {
            let z = (i as f64 + 0.5) / bins as f64;
            let y = srgb_decode(z);
            let x = y / (exposure - (exposure - 1.0) * y);
            let noise = display_noise(range, black, x, exposure).unwrap_or(0.0);
            (parameters.tone_noise_target / noise.max(1e-9))
                .clamp(parameters.tone_cap_min, parameters.tone_cap_max)
        })
        .collect();
    // A positive prior and slope bounds prevent equalization of a narrow/noisy peak into posterized tones.
    let (mut low, mut high) = (0.0, parameters.tone_solver_upper);
    for _ in 0..parameters.tone_solver_iterations {
        let scale = (low + high) * 0.5;
        let total: f64 = weights
            .iter()
            .zip(&caps)
            .map(|(w, cap)| (scale * w).clamp(parameters.tone_slope_min, *cap))
            .sum();
        if total > bins as f64 {
            high = scale;
        } else {
            low = scale;
        }
    }
    let scale = (low + high) * 0.5;
    let mut result = Vec::with_capacity(bins + 1);
    let mut cumulative = 0.0;
    result.push(0.0);
    for (i, (&weight, &cap)) in weights.iter().zip(&caps).enumerate() {
        cumulative += (scale * weight).clamp(parameters.tone_slope_min, cap) / bins as f64;
        result.push((1.0 - strength) * (i + 1) as f64 / bins as f64 + strength * cumulative);
    }
    result[bins] = 1.0;
    ensure!(
        (cumulative - 1.0).abs() <= 1e-8
            && result
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            && result.windows(2).all(|pair| pair[0] <= pair[1]),
        "tone normalization did not converge to a monotonic unit curve; increase --param-tone-solver-iterations or reduce --param-tone-solver-upper"
    );
    Ok(result)
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
    transfers: Transfers,
) -> Result<Rendered> {
    render_with_parameters(
        image,
        hist,
        range,
        optimized,
        expression,
        policy,
        transfers,
        &Parameters::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_with_parameters(
    image: &MonoImage,
    hist: &Histogram,
    range: &RangeAnalysis,
    optimized: bool,
    expression: Option<&Expression>,
    policy: FunctionPolicy,
    transfers: Transfers,
    parameters: &Parameters,
) -> Result<Rendered> {
    let tone = ToneCurve::fit_with_parameters(
        hist,
        range,
        optimized,
        image.metadata.black_level,
        parameters,
    )?;
    let mut values: Vec<f64> = (0..LEVELS)
        .map(|code| tone.map(range.normalize(code as f64)))
        .collect();
    let function = expression::apply(&mut values, hist, expression, policy)?;
    let png_table: Vec<u16> = values
        .par_iter()
        .map(|&x| {
            let value = match transfers.png {
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
            let display = match (transfers.png, transfers.jpeg) {
                (Transfer::Srgb, Transfer::Linear) => srgb_decode(value),
                (Transfer::Linear, Transfer::Srgb) => srgb_encode(value),
                (Transfer::Srgb, Transfer::Srgb) | (Transfer::Linear, Transfer::Linear) => value,
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
