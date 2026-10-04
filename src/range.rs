use crate::noise::DynamicRange;
pub use crate::noise::{NoiseEstimate, estimate_noise};
use crate::raw::MonoImage;
use anyhow::{Result, ensure};
use rayon::prelude::*;
use serde::Serialize;

pub const LEVELS: usize = 65536;
const AUTO_CAP: f64 = 0.02;

#[derive(Debug)]
pub struct Histogram {
    pub bins: Vec<u64>,
    pub total: u64,
    pub min: u16,
    pub max: u16,
}

impl Histogram {
    pub fn new(pixels: &[u16]) -> Result<Self> {
        ensure!(!pixels.is_empty(), "cannot analyze an empty image");
        let chunk = pixels
            .len()
            .div_ceil(rayon::current_num_threads() * 4)
            .max(262144);
        let bins = pixels
            .par_chunks(chunk)
            .map(|part| {
                let mut bins = vec![0u64; LEVELS];
                for &v in part {
                    bins[usize::from(v)] += 1;
                }
                bins
            })
            .reduce(
                || vec![0; LEVELS],
                |mut a, b| {
                    for (a, b) in a.iter_mut().zip(b) {
                        *a += b;
                    }
                    a
                },
            );
        let min = bins
            .iter()
            .position(|&n| n > 0)
            .expect("nonempty histogram") as u16;
        let max = bins
            .iter()
            .rposition(|&n| n > 0)
            .expect("nonempty histogram") as u16;
        Ok(Self {
            bins,
            total: pixels.len() as u64,
            min,
            max,
        })
    }

    pub fn at_rank(&self, rank: u64) -> u16 {
        let rank = rank.min(self.total - 1);
        let mut sum = 0;
        for (value, &n) in self.bins.iter().enumerate() {
            sum += n;
            if sum > rank {
                return value as u16;
            }
        }
        unreachable!("histogram count matches its total")
    }

    pub fn quantile(&self, fraction: f64) -> u16 {
        self.at_rank((fraction.clamp(0.0, 1.0) * (self.total - 1) as f64) as u64)
    }

    fn tail_value(&self, fraction: f64, high: bool) -> u16 {
        let count = (fraction * self.total as f64).floor() as u64;
        self.at_rank(if high {
            (self.total - 1).saturating_sub(count)
        } else {
            count
        })
    }

    pub fn percent_where(&self, predicate: impl Fn(f64) -> bool) -> f64 {
        let count: u64 = self
            .bins
            .iter()
            .enumerate()
            .filter(|(v, _)| predicate(*v as f64))
            .map(|(_, &n)| n)
            .sum();
        100.0 * count as f64 / self.total as f64
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RangeOptions {
    pub dark: Option<f64>,
    pub light: Option<f64>,
    pub clip_strength: u8,
}

impl Default for RangeOptions {
    fn default() -> Self {
        Self {
            dark: None,
            light: None,
            clip_strength: 4,
        }
    }
}

impl RangeOptions {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=9).contains(&self.clip_strength),
            "clip strength must be an integer from 1 to 9"
        );
        for (name, value) in [("dark", self.dark), ("light", self.light)] {
            if let Some(value) = value {
                ensure!(
                    value.is_finite() && (0.0..100.0).contains(&value),
                    "{name} percentage must be finite and in [0, 100)"
                );
            }
        }
        ensure!(
            self.dark.unwrap_or(0.0) + self.light.unwrap_or(0.0) < 100.0,
            "dark + light percentages must total less than 100"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RangeAnalysis {
    pub observed_min: u16,
    pub observed_max: u16,
    pub lower: f64,
    pub upper: f64,
    pub clip_strength: u8,
    pub requested_dark_percent: Option<f64>,
    pub requested_light_percent: Option<f64>,
    pub dark_tail_knee_percent: f64,
    pub light_tail_knee_percent: f64,
    pub dark_coherent_samples: usize,
    pub light_coherent_samples: usize,
    pub clipped_dark_percent: f64,
    pub clipped_light_percent: f64,
    pub below_black_percent: f64,
    pub above_white_percent: f64,
    pub occupied_codes: usize,
    pub retained_codes: usize,
    pub white_level_bits: f64,
    pub retained_span_bits: f64,
    pub histogram_entropy_bits: f64,
    pub noise: NoiseEstimate,
    pub estimated_snr_span_stops: Option<f64>,
    pub dynamic_range: DynamicRange,
    pub warnings: Vec<String>,
}

impl RangeAnalysis {
    pub fn normalize(&self, value: f64) -> f64 {
        ((value - self.lower) / (self.upper - self.lower)).clamp(0.0, 1.0)
    }
}

pub fn analyze(
    image: &MonoImage,
    hist: &Histogram,
    options: RangeOptions,
) -> Result<RangeAnalysis> {
    options.validate()?;
    ensure!(
        image.metadata.width.checked_mul(image.metadata.height) == Some(image.pixels.len())
            && hist.total == image.pixels.len() as u64,
        "image dimensions and histogram count must match the pixels"
    );
    let black = image.metadata.black_level;
    let white = f64::from(image.metadata.white_level);
    let noise = estimate_noise(image, hist);
    let (dark_knee, dark_percent) = tail_knee(hist, false, noise.dark_sigma_codes);
    let (light_knee, light_percent) = tail_knee(hist, true, noise.light_sigma_codes);
    let (dark_knee, dark_coherent_samples) = if options.dark.is_none() && dark_percent > 0.0 {
        protect_coherent_tail(image, hist, &noise, dark_knee, false)
    } else {
        (dark_knee, 0)
    };
    let (light_knee, light_coherent_samples) = if options.light.is_none() && light_percent > 0.0 {
        protect_coherent_tail(image, hist, &noise, light_knee, true)
    } else {
        (light_knee, 0)
    };
    let dark_cap = f64::from(hist.tail_value(AUTO_CAP, false));
    let light_cap = f64::from(hist.tail_value(AUTO_CAP, true));
    let noise_floor = black + noise.signal_for_snr(2.0).unwrap_or(0.0);
    let mut lower = options.dark.map_or_else(
        || black.max(dark_knee.max(noise_floor).min(dark_cap)),
        |p| f64::from(hist.tail_value(p / 100.0, false)),
    );
    let mut upper = options.light.map_or_else(
        || white.min(light_knee.max(light_cap)),
        |p| f64::from(hist.tail_value(p / 100.0, true)),
    );
    let mut warnings = Vec::new();
    if hist.min == hist.max {
        lower = black;
        upper = white;
        warnings.push("Constant image: no range to stretch; using black/white calibration.".into());
    } else if lower >= upper && (options.dark.is_none() || options.light.is_none()) {
        if options.dark.is_none() {
            lower = black.max(f64::from(hist.min));
        }
        if options.light.is_none() {
            upper = white.min(f64::from(hist.max));
        }
        if lower >= upper && options.dark.is_none() && options.light.is_none() {
            lower = black;
            upper = white;
        }
        warnings.push(
            "Automatic endpoints collapsed; reverted automatic bounds to calibrated observed range."
                .into(),
        );
    }
    ensure!(
        lower < upper,
        "clipping leaves no usable range ({lower}..{upper}); reduce percentages or set the other tail to 0"
    );

    if options.clip_strength > 1
        && hist.min != hist.max
        && f64::from(hist.max) > lower
        && f64::from(hist.min) < upper
    {
        let blend = f64::from(options.clip_strength - 1) / 8.0;
        let stronger = |bound: f64, knee: f64, high: bool| {
            let current = hist.percent_where(|v| if high { v > bound } else { v < bound });
            let strongest = (1.0 + 0.5 * knee).clamp(1.0, 2.0).max(current);
            let target = current + blend * (strongest - current);
            if target <= current {
                return bound;
            }
            let quantile = f64::from(hist.tail_value(target / 100.0, high));
            if high {
                bound.min(quantile)
            } else {
                bound.max(quantile)
            }
        };
        let mut selected_lower = if options.dark.is_none() {
            stronger(lower, dark_percent, false)
        } else {
            lower
        };
        let mut selected_upper = if options.light.is_none() {
            stronger(upper, light_percent, true)
        } else {
            upper
        };
        if selected_lower >= selected_upper {
            let minimum_span = (upper - lower).min(1.0);
            if options.dark.is_none() && options.light.is_none() {
                let center = ((selected_lower + selected_upper) * 0.5)
                    .clamp(lower + minimum_span * 0.5, upper - minimum_span * 0.5);
                selected_lower = center - minimum_span * 0.5;
                selected_upper = center + minimum_span * 0.5;
            } else if options.dark.is_none() {
                selected_lower = upper - minimum_span;
            } else {
                selected_upper = lower + minimum_span;
            }
            warnings.push("Clip strength reached tied values; limited automatic clipping to keep a nonzero interval and preserve manual bounds.".into());
        }
        lower = selected_lower;
        upper = selected_upper;
    }

    let mut occupied_codes = 0;
    let mut retained_codes = 0;
    let mut entropy = 0.0;
    for (value, &n) in hist.bins.iter().enumerate().filter(|(_, n)| **n > 0) {
        occupied_codes += 1;
        if value as f64 >= lower && value as f64 <= upper {
            retained_codes += 1;
        }
        let p = n as f64 / hist.total as f64;
        entropy -= p * p.log2();
    }
    let estimated_snr_span_stops = noise
        .dark_sigma_codes
        .filter(|sigma| *sigma > 0.0)
        .map(|sigma| ((upper - lower) / sigma).max(1.0).log2());
    let dynamic_range =
        DynamicRange::estimate(&noise, black, lower, upper.min(white), hist.min != hist.max);
    if noise
        .dark_sigma_codes
        .is_some_and(|sigma| upper - lower < 8.0 * sigma)
    {
        warnings.push("Selected signal span is small relative to estimated noise; stretching will amplify noise.".into());
    }
    Ok(RangeAnalysis {
        observed_min: hist.min,
        observed_max: hist.max,
        lower,
        upper,
        clip_strength: options.clip_strength,
        requested_dark_percent: options.dark,
        requested_light_percent: options.light,
        dark_tail_knee_percent: if options.dark.is_none() {
            dark_percent
        } else {
            0.0
        },
        light_tail_knee_percent: if options.light.is_none() {
            light_percent
        } else {
            0.0
        },
        dark_coherent_samples,
        light_coherent_samples,
        clipped_dark_percent: hist.percent_where(|v| v < lower),
        clipped_light_percent: hist.percent_where(|v| v > upper),
        below_black_percent: hist.percent_where(|v| v < black),
        above_white_percent: hist.percent_where(|v| v > white),
        occupied_codes,
        retained_codes,
        white_level_bits: (white + 1.0).log2(),
        retained_span_bits: (upper - lower + 1.0).log2(),
        histogram_entropy_bits: entropy,
        noise,
        estimated_snr_span_stops,
        dynamic_range,
        warnings,
    })
}

fn protect_coherent_tail(
    image: &MonoImage,
    hist: &Histogram,
    noise: &NoiseEstimate,
    bound: f64,
    high: bool,
) -> (f64, usize) {
    let (width, height) = (image.metadata.width, image.metadata.height);
    if width < 3 || height < 3 {
        return (bound, 0);
    }
    let step = ((image.pixels.len() as f64 / 500_000.0).sqrt().ceil() as usize).max(1);
    let cols = (width - 3) / step + 1;
    let rows = (height - 3) / step + 1;
    let in_tail = |v: f64| if high { v > bound } else { v < bound };
    let mut supported: Vec<f64> = (0..cols * rows)
        .into_par_iter()
        .filter_map(|i| {
            let (x, y) = (1 + (i % cols) * step, 1 + (i / cols) * step);
            if !in_tail(f64::from(image.pixels[y * width + x])) {
                return None;
            }
            let mut neighbors = [0u16; 8];
            let mut n = 0;
            for row in y - 1..=y + 1 {
                for col in x - 1..=x + 1 {
                    if row != y || col != x {
                        neighbors[n] = image.pixels[row * width + col];
                        n += 1;
                    }
                }
            }
            if neighbors.iter().filter(|v| in_tail(f64::from(**v))).count() < 5 {
                return None;
            }
            neighbors.sort_unstable();
            Some((f64::from(neighbors[3]) + f64::from(neighbors[4])) * 0.5)
        })
        .collect();
    if supported.len() < 4 {
        return (bound, supported.len());
    }
    supported.sort_unstable_by(f64::total_cmp);
    let rank = ((supported.len() - 1) as f64 * if high { 0.995 } else { 0.005 }) as usize;
    let value = supported[rank];
    let margin = 2.0
        * noise
            .sigma_at((value - image.metadata.black_level).max(0.0))
            .unwrap_or(0.0);
    let protected = if high {
        (value + margin).min(f64::from(hist.max)).max(bound)
    } else {
        (value - margin).max(f64::from(hist.min)).min(bound)
    };
    (protected, supported.len())
}

fn tail_knee(hist: &Histogram, high: bool, sigma: Option<f64>) -> (f64, f64) {
    let outer = f64::from(if high { hist.max } else { hist.min });
    let inner = f64::from(hist.tail_value(AUTO_CAP, high));
    let span = (inner - outer).abs();
    if span < (2.0 * sigma.unwrap_or(0.0)).max(4.0) {
        return (outer, 0.0);
    }
    let mut best = (0.0, outer, 0.0);
    for i in 1..200 {
        let fraction = AUTO_CAP * f64::from(i) / 200.0;
        let value = f64::from(hist.tail_value(fraction, high));
        let score = (value - outer).abs() / span - f64::from(i) / 200.0;
        if score > best.0 {
            best = (score, value, fraction * 100.0);
        }
    }
    if best.0 >= 0.2 {
        (best.1, best.2)
    } else {
        (outer, 0.0)
    }
}
