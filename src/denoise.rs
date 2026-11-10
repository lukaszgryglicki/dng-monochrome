use crate::{noise::NoiseEstimate, raw::MonoImage};
use anyhow::{Result, ensure};
use rayon::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub level: f64,
    pub applied: bool,
    pub scales: usize,
    pub noise_source: Option<&'static str>,
    pub changed_samples: usize,
    pub warnings: Vec<String>,
}

pub struct Reduction {
    pub pixels: Option<Vec<f32>>,
    pub report: Report,
}

fn reflect(index: isize, length: usize) -> usize {
    if index < 0 {
        (-index) as usize
    } else if index as usize >= length {
        2 * (length - 1) - index as usize
    } else {
        index as usize
    }
}

fn smooth(pixels: &mut [f32], scratch: &mut [f32], width: usize, height: usize, step: usize) {
    let step = step as isize;
    scratch
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let source = &pixels[y * width..(y + 1) * width];
            for (x, value) in row.iter_mut().enumerate() {
                let x = x as isize;
                *value = (source[reflect(x - 2 * step, width)]
                    + 4.0 * source[reflect(x - step, width)]
                    + 6.0 * source[x as usize]
                    + 4.0 * source[reflect(x + step, width)]
                    + source[reflect(x + 2 * step, width)])
                    / 16.0;
            }
        });
    pixels
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let y = y as isize;
            let rows = [-2, -1, 0, 1, 2].map(|offset| {
                let start = reflect(y + offset * step, height) * width;
                &scratch[start..start + width]
            });
            for (x, value) in row.iter_mut().enumerate() {
                *value = (rows[0][x]
                    + 4.0 * rows[1][x]
                    + 6.0 * rows[2][x]
                    + 4.0 * rows[3][x]
                    + rows[4][x])
                    / 16.0;
            }
        });
}

fn band_variances(scales: usize) -> Vec<f32> {
    let mut previous = vec![1.0f64];
    (0..scales)
        .map(|scale| {
            let step = 1 << scale;
            let mut next = vec![0.0; previous.len() + 4 * step];
            for (i, &value) in previous.iter().enumerate() {
                for (tap, weight) in [1.0, 4.0, 6.0, 4.0, 1.0].into_iter().enumerate() {
                    next[i + tap * step] += value * weight / 16.0;
                }
            }
            let before: f64 = previous.iter().map(|value| value * value).sum();
            let after: f64 = next.iter().map(|value| value * value).sum();
            let cross: f64 = previous
                .iter()
                .zip(&next[2 * step..])
                .map(|(a, b)| a * b)
                .sum();
            // Squared norm of the difference between the two separable 2D kernels.
            let variance = (before * before + after * after - 2.0 * cross * cross).max(0.0);
            previous = next;
            variance as f32
        })
        .collect()
}

pub fn reduce(image: &MonoImage, noise: &NoiseEstimate, level: f64) -> Result<Reduction> {
    ensure!(
        level.is_finite() && (0.0..=2.0).contains(&level),
        "noise reduction level must be finite and in [0, 2]"
    );
    let (width, height) = (image.metadata.width, image.metadata.height);
    ensure!(
        !image.pixels.is_empty() && width.checked_mul(height) == Some(image.pixels.len()),
        "noise reduction requires matching nonempty image dimensions"
    );
    let mut result = Reduction {
        pixels: None,
        report: Report {
            level,
            applied: false,
            scales: 0,
            noise_source: None,
            changed_samples: 0,
            warnings: Vec::new(),
        },
    };
    if level == 0.0 {
        return Ok(result);
    }
    let scales = (0..4)
        .take_while(|scale| 4 << scale < width.min(height))
        .count();
    if scales == 0 {
        result
            .report
            .warnings
            .push("Image too small for spatial noise reduction; unchanged.".into());
        return Ok(result);
    }
    let black = image.metadata.black_level;
    ensure!(
        black.is_finite() && black >= 0.0 && black < f64::from(image.metadata.white_level),
        "noise reduction requires valid black/white calibration"
    );
    let Some(variance): Option<Vec<_>> = (0..=u16::MAX)
        .map(|code| {
            noise
                .sigma_at((f64::from(code) - black).max(0.0))
                .map(|sigma| sigma * sigma)
        })
        .collect()
    else {
        result
            .report
            .warnings
            .push("No usable noise estimate; noise reduction skipped.".into());
        return Ok(result);
    };
    ensure!(
        variance
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0 && *value <= f64::from(f32::MAX) / 4.0),
        "invalid noise reduction variance"
    );
    if variance.iter().all(|&value| value == 0.0) {
        result
            .report
            .warnings
            .push("No measurable noise; noise reduction skipped.".into());
        return Ok(result);
    }
    let variance: Vec<_> = variance.into_iter().map(|value| value as f32).collect();
    let mut current: Vec<_> = image
        .pixels
        .par_iter()
        .map(|&value| f32::from(value))
        .collect();
    let mut output = current.clone();
    let mut low = vec![0.0; current.len()];
    let mut energy = vec![0.0; current.len()];
    let mut scratch = vec![0.0; current.len()];
    for (scale, variance_factor) in band_variances(scales).into_iter().enumerate() {
        let step = 1 << scale;
        low.copy_from_slice(&current);
        smooth(&mut low, &mut scratch, width, height, step);
        current
            .par_iter_mut()
            .zip(&low)
            .zip(&mut energy)
            .for_each(|((detail, &base), power)| {
                *detail -= base;
                *power = *detail * *detail;
            });
        smooth(&mut energy, &mut scratch, width, height, step);
        let factor = variance_factor * (level * level) as f32;
        output
            .par_iter_mut()
            .zip(&current)
            .zip(&low)
            .zip(&energy)
            .for_each(|(((value, &detail), &base), &power)| {
                let noise = variance[base.clamp(0.0, 65535.0) as usize] * factor;
                let removed = (noise / power.max(1e-12)).clamp(0.0, 1.0);
                *value -= removed * detail;
            });
        std::mem::swap(&mut current, &mut low);
    }
    let white = image.metadata.white_level;
    result.report.changed_samples = output
        .par_iter_mut()
        .zip(&image.pixels)
        .map(|(value, &original)| {
            *value = if original == 0 || original >= white {
                f32::from(original)
            } else {
                value.clamp(0.0, 65535.0)
            };
            usize::from(*value != f32::from(original))
        })
        .sum();
    result.report.applied = true;
    result.report.scales = scales;
    result.report.noise_source = Some(
        noise
            .model
            .filter(|model| model.source != "dng_noise_profile" || noise.read_noise_resolved)
            .map_or("measured_noise_bins", |model| model.source),
    );
    result.pixels = Some(output);
    Ok(result)
}
