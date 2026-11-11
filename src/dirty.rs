use crate::raw::{MonoImage, SensorMetadata};
use anyhow::{Context, Result, bail, ensure};
use rawler::decoders::Orientation;
use rayon::prelude::*;
use serde::Serialize;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

const PATCH_SIDE: usize = 33;
const MAX_LOG_GAIN: f64 = std::f64::consts::LN_2 * 2.0;
const MAX_FIT_ERROR: f64 = 0.025;
// Stacked evidence of --fix-dirty-extra: preview-resolution residual codes per analyzed RAW.
const STACK_UNITS: f32 = 2048.0;
const STACK_BACKGROUND: f64 = 16.0;
const STACK_TILE: usize = 16;
const STACK_VIEWS: usize = 24;
const STACK_POOL: usize = 64;
const STACK_MIN_VIEWS: usize = 3;
const STACK_MARGIN: usize = 12;
// How many frames on either side of a target, in capture order, its stack may draw from;
// twice the pool, because near-duplicate views are skipped.
pub const STACK_REACH: usize = 2 * STACK_POOL;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Geometry {
    make: String,
    model: String,
    raw: [usize; 2],
    crop: [usize; 4],
}

impl Geometry {
    fn new(metadata: &SensorMetadata) -> Self {
        Self {
            make: metadata.make.clone(),
            model: metadata.model.clone(),
            raw: [metadata.raw_width, metadata.raw_height],
            crop: metadata.crop,
        }
    }

    fn accepts(&self, metadata: &SensorMetadata) -> bool {
        self.make == metadata.make
            && self.model == metadata.model
            && self.raw == [metadata.raw_width, metadata.raw_height]
            && self.crop[..2] == metadata.crop[..2]
            && (2..4).all(|axis| {
                self.crop[axis] >= metadata.crop[axis] && self.crop[axis] - metadata.crop[axis] <= 1
            })
    }
}

struct Preview {
    width: usize,
    height: usize,
    step: usize,
    pixels: Vec<f32>,
}

impl Preview {
    fn new(image: &MonoImage) -> Result<Self> {
        let metadata = &image.metadata;
        ensure!(
            metadata.width > 0
                && metadata.height > 0
                && metadata.width.checked_mul(metadata.height) == Some(image.pixels.len())
                && metadata.black_level.is_finite()
                && metadata.black_level >= 0.0
                && metadata.black_level < f64::from(metadata.white_level),
            "sensor-dirt analysis requires valid raw dimensions and black/white calibration"
        );
        let (transpose, flip_x, flip_y) = Orientation::from_u16(metadata.orientation).to_flips();
        let (sw, sh) = if transpose {
            (metadata.height, metadata.width)
        } else {
            (metadata.width, metadata.height)
        };
        ensure!(
            metadata.crop[2..] == [sw, sh],
            "sensor-dirt analysis requires the unsplit raw raster"
        );
        let step = sw.max(sh).div_ceil(2048).div_ceil(2) * 2;
        let (width, height) = (sw.div_ceil(step), sh.div_ceil(step));
        let span = f64::from(metadata.white_level) - metadata.black_level;
        let mut pixels = vec![0.0; width * height];
        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, pixel) in row.iter_mut().enumerate() {
                    let mut sum = 0u64;
                    let mut count = 0u32;
                    for sy in y * step..((y + 1) * step).min(sh) {
                        for sx in x * step..((x + 1) * step).min(sw) {
                            let sx = if flip_x { sw - 1 - sx } else { sx };
                            let sy = if flip_y { sh - 1 - sy } else { sy };
                            let (px, py) = if transpose { (sy, sx) } else { (sx, sy) };
                            sum += u64::from(image.pixels[py * metadata.width + px]);
                            count += 1;
                        }
                    }
                    *pixel = ((sum as f64 / f64::from(count) - metadata.black_level) / span)
                        .clamp(0.0, 1.0) as f32;
                }
            });
        Ok(Self {
            width,
            height,
            step,
            pixels,
        })
    }

    fn sample(&self, x: f64, y: f64) -> f64 {
        bilinear(&self.pixels, self.width, self.height, x, y)
    }

    fn smooth_sample(&self, x: f64, y: f64, spacing: f64) -> f64 {
        let mut sum = 0.0;
        for (dy, wy) in [(-1.0, 0.2), (0.0, 0.6), (1.0, 0.2)] {
            for (dx, wx) in [(-1.0, 0.2), (0.0, 0.6), (1.0, 0.2)] {
                sum += wx * wy * self.sample(x + dx * spacing, y + dy * spacing);
            }
        }
        sum
    }

    fn signature(&self) -> Vec<f64> {
        let mut values = Vec::with_capacity(16 * 12);
        for gy in 0..12 {
            for gx in 0..16 {
                let mut sum = 0.0;
                let mut count = 0;
                for y in gy * self.height / 12..((gy + 1) * self.height / 12) {
                    for x in gx * self.width / 16..((gx + 1) * self.width / 16) {
                        sum += f64::from(self.pixels[y * self.width + x]).max(0.001).ln();
                        count += 1;
                    }
                }
                values.push(sum / count.max(1) as f64);
            }
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        values.iter_mut().for_each(|value| *value -= mean);
        let norm = values.iter().map(|value| value * value).sum::<f64>().sqrt();
        values
            .iter_mut()
            .for_each(|value| *value /= norm.max(1e-12));
        values
    }

    // High-pass log signal and a per-pixel "smooth, unclipped" flag, packed for stacking.
    fn residual(&self) -> Vec<i16> {
        let (width, height) = (self.width, self.height);
        let log: Vec<f32> = self
            .pixels
            .iter()
            .map(|&value| value.max(0.0007).ln())
            .collect();
        let background = gaussian(&log, width, height, STACK_BACKGROUND, true);
        let detail: Vec<f32> = log
            .iter()
            .zip(&background)
            .map(|(value, base)| value - base)
            .collect();
        // Re-level without dark outliers, so a deep shadow leaves no bright halo around it.
        let kept: Vec<f32> = detail
            .iter()
            .map(|&value| f32::from(u8::from(value >= -0.03)))
            .collect();
        let weighted: Vec<f32> = detail.iter().zip(&kept).map(|(a, b)| a * b).collect();
        let level = gaussian(&weighted, width, height, STACK_BACKGROUND, false);
        let weight = gaussian(&kept, width, height, STACK_BACKGROUND, false);
        let detail: Vec<f32> = (0..detail.len())
            .map(|i| {
                if weight[i] > 0.05 {
                    detail[i] - level[i] / weight[i]
                } else {
                    detail[i]
                }
            })
            .collect();
        let coarse = gaussian(&detail, width, height, 1.5, true);
        let (tiles_x, tiles_y) = (width.div_ceil(STACK_TILE), height.div_ceil(STACK_TILE));
        let mut texture = vec![0.0f32; tiles_x * tiles_y];
        let mut level = vec![0usize; tiles_x * tiles_y];
        texture
            .par_iter_mut()
            .zip(level.par_iter_mut())
            .enumerate()
            .for_each(|(tile, (texture, level))| {
                let (tx, ty) = (tile % tiles_x, tile / tiles_x);
                let mut fine = Vec::with_capacity(STACK_TILE * STACK_TILE);
                let mut sum = 0.0f64;
                for y in ty * STACK_TILE..((ty + 1) * STACK_TILE).min(height) {
                    for x in tx * STACK_TILE..((tx + 1) * STACK_TILE).min(width) {
                        let i = y * width + x;
                        fine.push((detail[i] - coarse[i]).abs());
                        sum += f64::from(log[i]);
                    }
                }
                let middle = fine.len() / 2;
                *texture = *fine.select_nth_unstable_by(middle, f32::total_cmp).1 * 1.4826;
                *level = (sum / fine.len() as f64 / std::f64::consts::LN_2 + 11.0)
                    .floor()
                    .clamp(0.0, 11.0) as usize;
            });
        // Noise floor of each brightness octave: its 20th-percentile tile texture.
        let quantile = |values: &mut Vec<f32>| {
            let index = values.len() / 5;
            *values.select_nth_unstable_by(index, f32::total_cmp).1
        };
        let fallback = quantile(&mut texture.clone());
        let measured: Vec<Option<f32>> = (0..12)
            .map(|octave| {
                let mut values: Vec<f32> = texture
                    .iter()
                    .zip(&level)
                    .filter(|(_, level)| **level == octave)
                    .map(|(value, _)| *value)
                    .collect();
                (values.len() >= 20).then(|| quantile(&mut values))
            })
            .collect();
        let floors: Vec<f32> = (0..12usize)
            .map(|octave| {
                (0..12usize)
                    .filter_map(|other| {
                        measured[other].map(|value| (other.abs_diff(octave), value))
                    })
                    .min_by_key(|(distance, _)| *distance)
                    .map_or(fallback, |(_, value)| value)
            })
            .collect();
        let limit: Vec<f32> = level
            .iter()
            .map(|&octave| (1.8 * floors[octave]).max(0.012))
            .collect();
        let mut codes = vec![0i16; width * height];
        codes
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                let ty = (y as f64 + 0.5) / STACK_TILE as f64 - 0.5;
                for (x, code) in row.iter_mut().enumerate() {
                    let i = y * width + x;
                    let tx = (x as f64 + 0.5) / STACK_TILE as f64 - 0.5;
                    let smooth = (0.0007..0.97).contains(&self.pixels[i])
                        && bilinear(&texture, tiles_x, tiles_y, tx, ty)
                            <= bilinear(&limit, tiles_x, tiles_y, tx, ty);
                    let value = (detail[i] * STACK_UNITS).round().clamp(-16383.0, 16383.0) as i16;
                    *code = (value << 1) | i16::from(smooth);
                }
            });
        codes
    }

    fn irregular_proposals(
        &self,
        scores: &[f32],
        peaks: &[(f32, usize, usize)],
    ) -> Vec<(f64, f64, f64, Fit)> {
        let mut visited = vec![false; scores.len()];
        let mut found = Vec::new();
        let grid = FitGrid::get();
        for &(peak, x, y) in peaks {
            let seed = y * self.width + x;
            if visited[seed] {
                continue;
            }
            let threshold = (peak * 0.25).max(0.002);
            let mut pending = vec![seed];
            visited[seed] = true;
            let (mut left, mut right, mut top, mut bottom) = (x, x, y, y);
            let mut count = 0;
            while let Some(i) = pending.pop() {
                let (x, y) = (i % self.width, i / self.width);
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
                count += 1;
                for sy in y.saturating_sub(1)..=(y + 1).min(self.height - 1) {
                    for sx in x.saturating_sub(1)..=(x + 1).min(self.width - 1) {
                        let next = sy * self.width + sx;
                        if !visited[next] && scores[next] > threshold {
                            visited[next] = true;
                            pending.push(next);
                        }
                    }
                }
            }
            let width = (right - left + 1) as f64;
            let height = (bottom - top + 1) as f64;
            let extent = width.max(height);
            if count < 4 || !(8.0..=90.0).contains(&extent) {
                continue;
            }
            let center = [(left + right) as f64 * 0.5, (top + bottom) as f64 * 0.5];
            let axes = [(width / extent).max(0.35), (height / extent).max(0.35)];
            'rectangle: for (dx, dy) in [
                (0.0, 0.0),
                (-0.5, 0.0),
                (0.5, 0.0),
                (0.0, -0.5),
                (0.0, 0.5),
                (-0.5, -0.5),
                (-0.5, 0.5),
                (0.5, -0.5),
                (0.5, 0.5),
            ] {
                let (x, y) = (center[0] + dx, center[1] + dy);
                for padding in (8..=24).map(|value| f64::from(value) * 0.5) {
                    let radius = (extent + 2.0 * padding) / 8.0;
                    let axes = [
                        (width + 2.0 * padding) / (8.0 * radius),
                        (height + 2.0 * padding) / (8.0 * radius),
                    ];
                    if let Some(fit) =
                        FitGrid::shape(true).fit_irregular_axes(self, x, y, radius, axes)
                    {
                        found.push((x, y, radius, fit));
                        break 'rectangle;
                    }
                }
            }
            let mut radius = extent * 0.25;
            let limit = (extent * 0.5).min(18.0);
            if radius > limit {
                continue;
            }
            loop {
                let mut best: Option<(f64, f64, f64, Fit)> = None;
                for axes in [axes, [1.0, 0.5], [0.5, 1.0], [1.0, 1.0]] {
                    for dy in [0.0, -0.75, 0.75] {
                        for dx in [0.0, -0.75, 0.75] {
                            let x = center[0] + dx * radius * axes[0];
                            let y = center[1] + dy * radius * axes[1];
                            if let Some(fit) = grid.fit_irregular_axes(self, x, y, radius, axes)
                                && best
                                    .as_ref()
                                    .is_none_or(|old| fit.quality() > old.3.quality())
                            {
                                best = Some((x, y, radius, fit));
                            }
                        }
                    }
                }
                if let Some(best) = best {
                    found.push(best);
                    break;
                }
                if radius >= limit {
                    break;
                }
                radius = (radius * 1.125).min(limit);
            }
        }
        found
    }

    fn candidates(&self, extra: bool) -> Vec<Candidate> {
        let stride = self.width + 1;
        let mut integral = vec![0.0; stride * (self.height + 1)];
        for y in 0..self.height {
            let mut sum = 0.0;
            for x in 0..self.width {
                sum += f64::from(self.pixels[y * self.width + x]);
                integral[(y + 1) * stride + x + 1] = integral[y * stride + x + 1] + sum;
            }
        }
        let mean = |x: usize, y: usize, radius: usize| {
            let (left, top, right, bottom) =
                (x - radius, y - radius, x + radius + 1, y + radius + 1);
            (integral[bottom * stride + right] + integral[top * stride + left]
                - integral[top * stride + right]
                - integral[bottom * stride + left])
                / ((right - left) * (bottom - top)) as f64
        };
        let grid = FitGrid::get();
        let mut found = Vec::new();
        for sigma in [0.7f64, 1.2, 2.0, 3.2, 5.0, 8.0, 12.0, 18.0] {
            let margin = (sigma * 4.0).ceil() as usize + 2;
            if self.width <= margin * 2 + 2 || self.height <= margin * 2 + 2 {
                continue;
            }
            let small = sigma.round().max(1.0) as usize;
            let large = (sigma * 3.0).round() as usize;
            let mut scores = vec![0.0f32; self.pixels.len()];
            scores
                .par_chunks_mut(self.width)
                .enumerate()
                .for_each(|(y, row)| {
                    if y < margin || y >= self.height - margin {
                        return;
                    }
                    for (x, value) in row
                        .iter_mut()
                        .enumerate()
                        .take(self.width - margin)
                        .skip(margin)
                    {
                        let background = mean(x, y, large);
                        if background > 0.001 && background < 0.97 {
                            *value = ((background - mean(x, y, small)) / background) as f32;
                        }
                    }
                });
            let mut peaks = Vec::new();
            for y in margin..self.height - margin {
                for x in margin..self.width - margin {
                    let value = scores[y * self.width + x];
                    if value <= 0.008 {
                        continue;
                    }
                    let is_maximum = |radius| {
                        (y - radius..=y + radius).all(|sy| {
                            (x - radius..=x + radius).all(|sx| {
                                scores[sy * self.width + sx] < value
                                    || (scores[sy * self.width + sx] == value && (sy, sx) >= (y, x))
                            })
                        })
                    };
                    let radius = (sigma as usize).max(1);
                    if is_maximum(1) && (radius == 1 || is_maximum(radius)) {
                        peaks.push((value, x, y));
                    }
                }
            }
            peaks.sort_unstable_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then_with(|| (a.2, a.1).cmp(&(b.2, b.1)))
            });
            let mut tile_counts = [0; 16 * 12];
            let mut rank = 0;
            peaks.retain(|&(_, x, y)| {
                let tile = (y * 12 / self.height) * 16 + x * 16 / self.width;
                let keep = rank < 800 || tile_counts[tile] < 16;
                tile_counts[tile] += 1;
                rank += 1;
                keep
            });
            if extra && (sigma == 1.2 || sigma == 5.0) {
                found.extend(self.irregular_proposals(&scores, &peaks));
            }
            let fitted: Vec<_> = peaks
                .par_iter()
                .flat_map_iter(|&(_, x, y)| {
                    let (x, y) = (x as f64, y as f64);
                    let compact = grid.fit(self, x, y, sigma).map(|fit| (x, y, sigma, fit));
                    let mut radius = sigma;
                    let irregular = loop {
                        if !extra {
                            break None;
                        }
                        if let Some(fit) = grid.fit_irregular(self, x, y, radius) {
                            break Some((x, y, radius, fit));
                        }
                        if radius >= 18.0 {
                            break None;
                        }
                        radius = (radius * 2.0).min(18.0);
                    };
                    [compact, irregular].into_iter().flatten()
                })
                .collect();
            found.extend(fitted);
        }
        found.sort_by(|a, b| {
            a.3.irregular
                .cmp(&b.3.irregular)
                .then_with(|| b.3.quality().total_cmp(&a.3.quality()))
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.0.total_cmp(&b.0))
                .then_with(|| a.2.total_cmp(&b.2))
        });
        let mut selected: Vec<Candidate> = Vec::new();
        for (mut x, mut y, mut sigma, mut fit) in found {
            if !fit.irregular
                && selected
                    .iter()
                    .any(|other| (x - other.x).hypot(y - other.y) <= 2.0 * sigma.max(other.sigma))
            {
                continue;
            }
            for fraction in [0.1, 0.05, 0.025] {
                for (px, py, radius) in [
                    (x - fraction * 5.0, y, sigma),
                    (x + fraction * 5.0, y, sigma),
                    (x, y - fraction * 5.0, sigma),
                    (x, y + fraction * 5.0, sigma),
                    (x, y, sigma * (1.0 - fraction)),
                    (x, y, sigma * (1.0 + fraction)),
                ] {
                    let refined = if fit.irregular {
                        FitGrid::shape(fit.rectangular)
                            .fit_irregular_axes(self, px, py, radius, fit.axes)
                    } else {
                        grid.fit(self, px, py, radius)
                    };
                    if let Some(refined) = refined
                        && refined.quality() > fit.quality()
                    {
                        (x, y, sigma, fit) = (px, py, radius, refined);
                    }
                }
            }
            let patch = fit.patch(self, x, y, sigma);
            if fit.irregular {
                let reach = if fit.rectangular {
                    4.0 * 2.0f64.sqrt()
                } else {
                    3.5
                };
                let nearby: Vec<_> = selected
                    .iter()
                    .filter(|other| {
                        (x - other.x).hypot(y - other.y) < reach * sigma + 3.0 * other.sigma
                    })
                    .collect();
                let threshold = patch.values.iter().copied().fold(0.0, f32::max) * 0.1;
                let (mut total, mut outside, mut sx, mut sy) = (0.0, 0.0, 0.0, 0.0);
                for (i, &gain) in patch.values.iter().enumerate() {
                    if gain <= threshold {
                        continue;
                    }
                    let px = (patch.origin[0] + (i % patch.side) as f64 * patch.spacing + 0.5)
                        / self.step as f64
                        - 0.5;
                    let py = (patch.origin[1] + (i / patch.side) as f64 * patch.spacing + 0.5)
                        / self.step as f64
                        - 0.5;
                    let gain = f64::from(gain);
                    total += gain;
                    sx += px * gain;
                    sy += py * gain;
                    if nearby
                        .iter()
                        .all(|other| (px - other.x).hypot(py - other.y) > 3.0 * other.sigma)
                    {
                        outside += gain;
                    }
                }
                if total <= 0.0 || outside < total * 0.2 {
                    continue;
                }
                (x, y) = (sx / total, sy / total);
            }
            selected.push(Candidate {
                x,
                y,
                sigma,
                fit_error: fit.rms,
                patch,
            });
            if selected.len() == 400 {
                break;
            }
        }
        selected
    }
}

struct FitPoint {
    x: f64,
    y: f64,
    basis: [f64; 7],
    weights: [f64; 7],
    ring_weights: [f64; 6],
    ring: bool,
}

struct FitGrid {
    points: Vec<FitPoint>,
    rectangular: bool,
}

impl FitGrid {
    fn get() -> &'static Self {
        Self::shape(false)
    }

    fn shape(rectangular: bool) -> &'static Self {
        static GRID: OnceLock<FitGrid> = OnceLock::new();
        static RECTANGULAR: OnceLock<FitGrid> = OnceLock::new();
        let grid = if rectangular { &RECTANGULAR } else { &GRID };
        grid.get_or_init(|| {
            let mut points = Vec::new();
            let mut full = [[0.0; 7]; 7];
            let mut ring = [[0.0; 6]; 6];
            for iy in -8..=8 {
                for ix in -8..=8 {
                    let (x, y) = (f64::from(ix) * 0.5, f64::from(iy) * 0.5);
                    let r2 = x * x + y * y;
                    if !rectangular && r2 > 16.0 {
                        continue;
                    }
                    let basis = [1.0, x, y, x * x, x * y, y * y, -(-0.5 * r2).exp()];
                    let in_ring = if rectangular {
                        x.abs().max(y.abs()) >= 3.5
                    } else {
                        r2 >= 6.25
                    };
                    for i in 0..7 {
                        for j in 0..7 {
                            full[i][j] += basis[i] * basis[j];
                            if in_ring && i < 6 && j < 6 {
                                ring[i][j] += basis[i] * basis[j];
                            }
                        }
                    }
                    points.push(FitPoint {
                        x,
                        y,
                        basis,
                        weights: [0.0; 7],
                        ring_weights: [0.0; 6],
                        ring: in_ring,
                    });
                }
            }
            let full = inverse(full);
            let ring = inverse(ring);
            for point in &mut points {
                point.weights =
                    full.map(|row| row.iter().zip(point.basis).map(|(a, b)| a * b).sum());
                if point.ring {
                    point.ring_weights =
                        ring.map(|row| row.iter().zip(point.basis).map(|(a, b)| a * b).sum());
                }
            }
            Self {
                points,
                rectangular,
            }
        })
    }

    fn fit(&self, image: &Preview, x: f64, y: f64, sigma: f64) -> Option<Fit> {
        if x < 4.0 * sigma
            || y < 4.0 * sigma
            || x + 4.0 * sigma >= image.width as f64
            || y + 4.0 * sigma >= image.height as f64
        {
            return None;
        }
        let mut coefficients = [0.0; 7];
        let mut values = Vec::with_capacity(self.points.len());
        for point in &self.points {
            let signal = image.sample(x + point.x * sigma, y + point.y * sigma);
            if !(0.0007..0.97).contains(&signal) {
                return None;
            }
            let value = signal.ln();
            values.push(value);
            for (sum, weight) in coefficients.iter_mut().zip(point.weights) {
                *sum += weight * value;
            }
        }
        let amplitude = coefficients[6];
        if !(0.012..MAX_LOG_GAIN).contains(&amplitude) {
            return None;
        }
        let mut background = [0.0; 6];
        for (point, value) in self.points.iter().zip(&values) {
            for (sum, weight) in background.iter_mut().zip(point.ring_weights) {
                *sum += weight * (value - amplitude * point.basis[6]);
            }
        }
        let mut error = 0.0;
        let mut ring_error = 0.0;
        let mut ring_count = 0;
        for (point, value) in self.points.iter().zip(values) {
            let prediction: f64 = coefficients
                .iter()
                .zip(point.basis)
                .map(|(a, b)| a * b)
                .sum();
            error += (value - prediction).powi(2);
            if point.ring {
                let prediction: f64 = background.iter().zip(point.basis).map(|(a, b)| a * b).sum();
                ring_error += (value - prediction - amplitude * point.basis[6]).powi(2);
                ring_count += 1;
            }
        }
        let rms = (error / self.points.len() as f64).sqrt();
        let ring_rms = (ring_error / f64::from(ring_count)).sqrt();
        (rms < MAX_FIT_ERROR.min((amplitude * 0.25).max(0.002)) && ring_rms < MAX_FIT_ERROR)
            .then_some(Fit {
                amplitude,
                rms,
                ring_rms,
                background,
                irregular: false,
                axes: [1.0, 1.0],
                rectangular: false,
            })
    }

    fn fit_irregular(&self, image: &Preview, x: f64, y: f64, sigma: f64) -> Option<Fit> {
        self.fit_irregular_axes(image, x, y, sigma, [1.0, 1.0])
    }

    fn fit_irregular_axes(
        &self,
        image: &Preview,
        x: f64,
        y: f64,
        sigma: f64,
        axes: [f64; 2],
    ) -> Option<Fit> {
        let rx = 4.0 * sigma * axes[0];
        let ry = 4.0 * sigma * axes[1];
        if x < rx || y < ry || x + rx >= image.width as f64 || y + ry >= image.height as f64 {
            return None;
        }
        let mut background = [0.0; 6];
        let mut values = Vec::with_capacity(self.points.len());
        for point in &self.points {
            let signal = image.sample(x + point.x * sigma * axes[0], y + point.y * sigma * axes[1]);
            if !(0.0007..0.97).contains(&signal) {
                return None;
            }
            let value = signal.ln();
            values.push(value);
            for (sum, weight) in background.iter_mut().zip(point.ring_weights) {
                *sum += weight * value;
            }
        }
        let shadows: Vec<_> = self
            .points
            .iter()
            .zip(values)
            .map(|(point, value)| {
                background
                    .iter()
                    .zip(point.basis)
                    .map(|(a, b)| a * b)
                    .sum::<f64>()
                    - value
            })
            .collect();
        let amplitude = shadows.iter().copied().fold(0.0, f64::max);
        if !(0.02..MAX_LOG_GAIN).contains(&amplitude) {
            return None;
        }
        let mut ring_error = 0.0;
        let mut ring_count = 0;
        let mut bright_error = 0.0;
        for (point, &shadow) in self.points.iter().zip(&shadows) {
            if point.ring {
                ring_error += shadow * shadow;
                ring_count += 1;
            }
            bright_error += shadow.min(0.0).powi(2);
        }
        let ring_rms = (ring_error / f64::from(ring_count)).sqrt();
        let bright_rms = (bright_error / self.points.len() as f64).sqrt();
        let threshold = (4.0 * ring_rms).max(0.004);
        if ring_rms >= MAX_FIT_ERROR.min(amplitude * 0.1)
            || bright_rms > 1.5 * ring_rms.max(0.0005)
            || self
                .points
                .iter()
                .zip(&shadows)
                .any(|(point, &shadow)| point.ring && shadow > threshold)
            || shadows
                .iter()
                .filter(|&&shadow| shadow > threshold.max(amplitude * 0.15))
                .count()
                < 4
        {
            return None;
        }
        Some(Fit {
            amplitude,
            rms: ring_rms.max(bright_rms),
            ring_rms,
            background,
            irregular: true,
            axes,
            rectangular: self.rectangular,
        })
    }
}

fn inverse<const N: usize>(mut matrix: [[f64; N]; N]) -> [[f64; N]; N] {
    let mut result = [[0.0; N]; N];
    for (i, row) in result.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for column in 0..N {
        let pivot = (column..N)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))
            .unwrap();
        matrix.swap(column, pivot);
        result.swap(column, pivot);
        let divisor = matrix[column][column];
        assert!(
            divisor.abs() > 1e-12,
            "fixed dust fitting basis must be nonsingular"
        );
        for j in 0..N {
            matrix[column][j] /= divisor;
            result[column][j] /= divisor;
        }
        for row in 0..N {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            for j in 0..N {
                matrix[row][j] -= factor * matrix[column][j];
                result[row][j] -= factor * result[column][j];
            }
        }
    }
    result
}

struct Fit {
    amplitude: f64,
    rms: f64,
    ring_rms: f64,
    background: [f64; 6],
    irregular: bool,
    axes: [f64; 2],
    rectangular: bool,
}

impl Fit {
    fn quality(&self) -> f64 {
        self.amplitude / self.rms.max(0.001)
    }

    fn patch(&self, image: &Preview, x: f64, y: f64, sigma: f64) -> Patch {
        let reach = if self.rectangular { 4.0 } else { 3.5 };
        let side = if self.irregular {
            ((4.0 * reach * sigma).ceil() as usize + 1).max(PATCH_SIDE)
        } else {
            PATCH_SIDE
        };
        let spacing = 2.0 * reach * sigma / (side - 1) as f64;
        let smoothing = (sigma * 0.2).min(if self.irregular { 0.5 } else { 1.0 });
        let origin = [x - reach * sigma, y - reach * sigma];
        let mut values = Vec::with_capacity(side * side);
        for gy in 0..side {
            for gx in 0..side {
                let (px, py) = (
                    origin[0] + gx as f64 * spacing,
                    origin[1] + gy as f64 * spacing,
                );
                let (u, v) = (
                    (px - x) / (sigma * self.axes[0]),
                    (py - y) / (sigma * self.axes[1]),
                );
                let basis = [1.0, u, v, u * u, u * v, v * v];
                let background: f64 = self.background.iter().zip(basis).map(|(a, b)| a * b).sum();
                let shadow = background - image.smooth_sample(px, py, smoothing).max(0.0007).ln();
                let radius = if self.rectangular {
                    u.abs().max(v.abs())
                } else {
                    u.hypot(v)
                };
                let taper = (reach - radius).clamp(0.0, 1.0);
                let taper = taper * taper * (3.0 - 2.0 * taper);
                values.push(
                    ((shadow - 0.5 * self.ring_rms)
                        .clamp(0.0, (self.amplitude * 2.0).min(MAX_LOG_GAIN))
                        * taper) as f32,
                );
            }
        }
        if self.irregular {
            let threshold = (3.0 * self.ring_rms).max(self.amplitude * 0.05) as f32;
            let middle = side / 2;
            let seed = values
                .iter()
                .enumerate()
                .filter(|&(_, &value)| value > threshold)
                .min_by_key(|&(i, _)| {
                    (i % side).abs_diff(middle).pow(2) + (i / side).abs_diff(middle).pow(2)
                })
                .map(|(i, _)| i);
            let mut covered = vec![false; side * side];
            let mut pending: Vec<_> = seed.into_iter().collect();
            while let Some(i) = pending.pop() {
                if covered[i] || values[i] <= threshold {
                    continue;
                }
                covered[i] = true;
                let (x, y) = (i % side, i / side);
                for sy in y.saturating_sub(1)..=(y + 1).min(side - 1) {
                    for sx in x.saturating_sub(1)..=(x + 1).min(side - 1) {
                        pending.push(sy * side + sx);
                    }
                }
            }
            for (i, value) in values.iter_mut().enumerate() {
                let (x, y) = (i % side, i / side);
                let mut weight = 0.0f32;
                for sy in y.saturating_sub(2)..=(y + 2).min(side - 1) {
                    for sx in x.saturating_sub(2)..=(x + 2).min(side - 1) {
                        if covered[sy * side + sx] {
                            let distance = x.abs_diff(sx).max(y.abs_diff(sy));
                            weight = weight.max(if distance < 2 { 1.0 } else { 0.5 });
                        }
                    }
                }
                *value *= weight;
            }
        }
        Patch {
            origin: origin.map(|value| (value + 0.5) * image.step as f64 - 0.5),
            spacing: spacing * image.step as f64,
            side,
            irregular: self.irregular,
            values,
        }
    }
}

#[derive(Clone)]
struct Patch {
    origin: [f64; 2],
    spacing: f64,
    side: usize,
    irregular: bool,
    values: Vec<f32>,
}

struct Candidate {
    x: f64,
    y: f64,
    sigma: f64,
    fit_error: f64,
    patch: Patch,
}

pub struct Evidence {
    geometry: Geometry,
    width: usize,
    height: usize,
    step: usize,
    signature: Vec<f64>,
    candidates: Vec<Candidate>,
    stack: Option<Arc<Vec<i16>>>,
}

pub fn analyze(image: &MonoImage) -> Result<Evidence> {
    analyze_with_extra(image, false)
}

pub fn analyze_with_extra(image: &MonoImage, extra: bool) -> Result<Evidence> {
    analyze_with_stack(image, extra, extra)
}

// `stack` keeps what extra detection stacks around nearby targets, about 4 MB per frame.
// A reference further than STACK_REACH frames from every target only needs its votes.
pub fn analyze_with_stack(image: &MonoImage, extra: bool, stack: bool) -> Result<Evidence> {
    let preview = Preview::new(image)?;
    Ok(Evidence {
        geometry: Geometry::new(&image.metadata),
        width: preview.width,
        height: preview.height,
        step: preview.step,
        signature: preview.signature(),
        candidates: preview.candidates(extra),
        stack: stack.then(|| Arc::new(preview.residual())),
    })
}

fn same_view(a: &[f64], b: &[f64]) -> bool {
    a == b || a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>() > 0.985
}

struct ConfirmedSpot {
    x: f64,
    y: f64,
    sigma: f64,
    irregular: bool,
    supporting_images: usize,
    distinct_views: usize,
    support: Vec<Patch>,
    // Median summed log gain of the supporting shadows, in preview cells.
    depth: f64,
}

struct Group {
    geometry: Geometry,
    images: usize,
    distinct_views: usize,
    spots: Vec<ConfirmedSpot>,
}

struct StackFrame {
    index: usize,
    geometry: Geometry,
    width: usize,
    height: usize,
    step: usize,
    signature: Vec<f64>,
    data: Arc<Vec<i16>>,
}

pub struct Calibration {
    reference_images: usize,
    groups: Vec<Group>,
    stacks: Vec<StackFrame>,
}

fn match_radius(a: f64, b: f64, irregular: bool) -> f64 {
    // Extra radii enclose entire shapes rather than their compact cores.
    let fraction = if irregular { 0.25 } else { 0.375 };
    (fraction * (a + b)).max(0.75)
}

// Whether an extra shape can be the stopped-down shadow of dirt confirmed as a compact
// one. Wide open that shadow blurs into a disc around its own center of mass, so the shape
// must balance on the confirmed spot, lie within its reach and take no more light.
fn sharpened(spot: &ConfirmedSpot, candidate: &Candidate, step: f64) -> bool {
    if spot.irregular || !candidate.patch.irregular {
        return false;
    }
    let patch = &candidate.patch;
    let floor = 0.1 * patch.values.iter().copied().fold(0.0, f32::max);
    let reach = (3.0 * spot.sigma).max(spot.sigma + 8.0);
    let (mut total, mut weight, mut sx, mut sy) = (0.0, 0.0, 0.0, 0.0);
    for (i, &gain) in patch.values.iter().enumerate() {
        total += f64::from(gain.max(0.0));
        if gain <= floor {
            continue;
        }
        let x = (patch.origin[0] + (i % patch.side) as f64 * patch.spacing + 0.5) / step - 0.5;
        let y = (patch.origin[1] + (i / patch.side) as f64 * patch.spacing + 0.5) / step - 0.5;
        if (x - spot.x).hypot(y - spot.y) > reach {
            return false;
        }
        weight += f64::from(gain);
        sx += f64::from(gain) * x;
        sy += f64::from(gain) * y;
    }
    weight > 0.0
        && (sx / weight - spot.x).hypot(sy / weight - spot.y) <= (0.25 * spot.sigma).max(1.5)
        && total * (patch.spacing / step).powi(2) <= 3.0 * spot.depth.max(0.5)
}

impl Calibration {
    pub fn new(frames: &[Evidence]) -> Result<Self> {
        ensure!(
            !frames.is_empty(),
            "sensor-dirt calibration needs reference images"
        );
        let mut groups: Vec<Group> = Vec::new();
        for frame in frames {
            if !groups.iter().any(|group| group.geometry == frame.geometry) {
                groups.push(Group {
                    geometry: frame.geometry.clone(),
                    images: 0,
                    distinct_views: 0,
                    spots: Vec::new(),
                });
            }
        }
        for group in &mut groups {
            let indices: Vec<_> = (0..frames.len())
                .filter(|&i| frames[i].geometry == group.geometry)
                .collect();
            group.images = indices.len();
            let mut representatives: Vec<usize> = Vec::new();
            let mut views = vec![0; frames.len()];
            for &i in &indices {
                let same_view = representatives.iter().position(|&other| {
                    let a = &frames[i].signature;
                    let b = &frames[other].signature;
                    a == b || a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>() > 0.985
                });
                views[i] = same_view.unwrap_or_else(|| {
                    representatives.push(i);
                    representatives.len() - 1
                });
            }
            group.distinct_views = representatives.len();
            if group.distinct_views < 3 {
                continue;
            }
            let mut observations: Vec<_> = indices
                .iter()
                .flat_map(|&i| {
                    frames[i]
                        .candidates
                        .iter()
                        .map(move |candidate| (i, candidate))
                })
                .collect();
            observations.sort_by(|a, b| {
                a.1.fit_error
                    .total_cmp(&b.1.fit_error)
                    .then_with(|| a.0.cmp(&b.0))
                    .then_with(|| a.1.y.total_cmp(&b.1.y))
                    .then_with(|| a.1.x.total_cmp(&b.1.x))
            });
            let trials = [false, true].map(|irregular| {
                observations
                    .iter()
                    .filter(|(_, candidate)| candidate.patch.irregular == irregular)
                    .count()
                    .max(1)
            });
            let mut clusters: Vec<Vec<(usize, &Candidate)>> = Vec::new();
            for observation in observations {
                let candidate = observation.1;
                let nearest = clusters
                    .iter()
                    .enumerate()
                    .filter_map(|(i, cluster)| {
                        let center = cluster[0].1;
                        let distance = (center.x - candidate.x).hypot(center.y - candidate.y);
                        (center.patch.irregular == candidate.patch.irregular
                            && distance
                                <= match_radius(
                                    center.sigma,
                                    candidate.sigma,
                                    center.patch.irregular,
                                ))
                        .then_some((i, distance))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((index, _)) = nearest {
                    if !clusters[index].iter().any(|(i, _)| *i == observation.0) {
                        clusters[index].push(observation);
                    }
                } else {
                    clusters.push(vec![observation]);
                }
            }
            for cluster in clusters {
                let mut distinct: Vec<_> = cluster.iter().map(|(i, _)| views[*i]).collect();
                distinct.sort_unstable();
                distinct.dedup();
                if distinct.len() < 3 {
                    continue;
                }
                let center = cluster[0].1;
                let frame = &frames[indices[0]];
                let half_width = frame.width as f64 / 8.0;
                let half_height = frame.height as f64 / 8.0;
                let area = ((center.x + half_width).min(frame.width as f64)
                    - (center.x - half_width).max(0.0))
                    * ((center.y + half_height).min(frame.height as f64)
                        - (center.y - half_height).max(0.0));
                let coincidence_area: f64 = indices
                    .iter()
                    .flat_map(|&i| &frames[i].candidates)
                    .filter(|candidate| {
                        candidate.patch.irregular == center.patch.irregular
                            && (candidate.x - center.x).abs() <= half_width
                            && (candidate.y - center.y).abs() <= half_height
                    })
                    .map(|candidate| {
                        std::f64::consts::PI
                            * match_radius(center.sigma, candidate.sigma, center.patch.irregular)
                                .powi(2)
                    })
                    .sum();
                let probability =
                    (coincidence_area / (group.images as f64 * area)).clamp(f64::MIN_POSITIVE, 1.0);
                let k = distinct.len() - 1;
                let n = group.distinct_views - 1;
                let log_chance = (trials[usize::from(center.patch.irregular)] as f64).ln()
                    + (0..k)
                        .map(|i| ((n - i) as f64 / (i + 1) as f64).ln())
                        .sum::<f64>()
                    + k as f64 * probability.ln();
                if log_chance > 0.01f64.ln() {
                    continue;
                }
                let support = if center.patch.irregular {
                    distinct
                        .iter()
                        .map(|&view| {
                            let mut patch = cluster
                                .iter()
                                .find(|(i, _)| views[*i] == view)
                                .unwrap()
                                .1
                                .patch
                                .clone();
                            let peak = patch.values.iter().copied().fold(0.0, f32::max);
                            // One grid neighbor is at most half a preview pixel per axis.
                            let values = patch.values.clone();
                            for (i, value) in patch.values.iter_mut().enumerate() {
                                let (x, y) = (i % patch.side, i / patch.side);
                                for sy in y.saturating_sub(1)..=(y + 1).min(patch.side - 1) {
                                    for sx in x.saturating_sub(1)..=(x + 1).min(patch.side - 1) {
                                        *value = value.max(values[sy * patch.side + sx]);
                                    }
                                }
                                *value /= peak;
                            }
                            patch
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let mut depths: Vec<f64> = cluster
                    .iter()
                    .map(|(_, candidate)| {
                        let patch = &candidate.patch;
                        patch
                            .values
                            .iter()
                            .map(|&value| f64::from(value.max(0.0)))
                            .sum::<f64>()
                            * (patch.spacing / frame.step as f64).powi(2)
                    })
                    .collect();
                depths.sort_unstable_by(f64::total_cmp);
                group.spots.push(ConfirmedSpot {
                    x: cluster.iter().map(|(_, c)| c.x).sum::<f64>() / cluster.len() as f64,
                    y: cluster.iter().map(|(_, c)| c.y).sum::<f64>() / cluster.len() as f64,
                    sigma: center.sigma,
                    irregular: center.patch.irregular,
                    supporting_images: cluster.len(),
                    distinct_views: distinct.len(),
                    support,
                    depth: depths[depths.len() / 2],
                });
            }
        }
        Ok(Self {
            reference_images: frames.len(),
            groups,
            stacks: frames
                .iter()
                .enumerate()
                .filter_map(|(index, frame)| {
                    Some(StackFrame {
                        index,
                        geometry: frame.geometry.clone(),
                        width: frame.width,
                        height: frame.height,
                        step: frame.step,
                        signature: frame.signature.clone(),
                        data: frame.stack.clone()?,
                    })
                })
                .collect(),
        })
    }

    pub fn plan(&self, target: &Evidence) -> Plan {
        self.plan_near(target, None)
    }

    // References are expected in capture order. `position` is the index of the one closest
    // to the target; without it a target that is itself a reference uses its own.
    pub fn plan_near(&self, target: &Evidence, position: Option<usize>) -> Plan {
        let group = self
            .groups
            .iter()
            .find(|group| group.geometry == target.geometry);
        // Stacked shadows need the same three independent views as every other one.
        let fields = if group.is_some_and(|group| group.distinct_views >= 3) {
            self.stack_fields(target, position)
        } else {
            Vec::new()
        };
        let sensor = |x: f64, y: f64| {
            (
                target.geometry.crop[0] as f64 + (x + 0.5) * target.step as f64 - 0.5,
                target.geometry.crop[1] as f64 + (y + 0.5) * target.step as f64 - 0.5,
            )
        };
        let mut patches = Vec::new();
        let mut spots = Vec::new();
        let mut matched: Vec<&ConfirmedSpot> = Vec::new();
        if let Some(group) = group {
            for candidate in &target.candidates {
                let (x, y) = sensor(candidate.x, candidate.y);
                if fields.iter().any(|field| field.covers(x, y)) {
                    continue;
                }
                if let Some(confirmed) = group
                    .spots
                    .iter()
                    .find(|spot| {
                        spot.irregular == candidate.patch.irregular
                            && (spot.x - candidate.x).hypot(spot.y - candidate.y)
                                <= match_radius(spot.sigma, candidate.sigma, spot.irregular)
                    })
                    .or_else(|| {
                        if candidate.patch.irregular {
                            None
                        } else {
                            group.spots.iter().find(|spot| {
                                spot.irregular
                                    && (spot.x - candidate.x).hypot(spot.y - candidate.y)
                                        <= match_radius(spot.sigma, candidate.sigma, true)
                            })
                        }
                    })
                    .or_else(|| {
                        // Stopped down, dirt confirmed as a soft disc casts a small sharp
                        // shadow of its own shape.
                        group
                            .spots
                            .iter()
                            .find(|spot| sharpened(spot, candidate, target.step as f64))
                    })
                {
                    let mut patch = candidate.patch.clone();
                    // Such a shape has no reference shapes to agree with.
                    let unshared = patch.irregular && !confirmed.irregular;
                    if !patch.irregular
                        && confirmed.irregular
                        && patch.spacing > target.step as f64 * 0.5
                    {
                        let span = (patch.side - 1) as f64 * patch.spacing;
                        let side = (span / (target.step as f64 * 0.5)).ceil() as usize + 1;
                        let scale = (patch.side - 1) as f64 / (side - 1) as f64;
                        let values = (0..side * side)
                            .map(|i| {
                                bilinear(
                                    &patch.values,
                                    patch.side,
                                    patch.side,
                                    (i % side) as f64 * scale,
                                    (i / side) as f64 * scale,
                                ) as f32
                            })
                            .collect();
                        patch.values = values;
                        patch.spacing = span / (side - 1) as f64;
                        patch.side = side;
                    }
                    patch.irregular = confirmed.irregular || unshared;
                    if patch.irregular && !unshared {
                        let peak = f64::from(patch.values.iter().copied().fold(0.0, f32::max));
                        for (i, gain) in patch.values.iter_mut().enumerate() {
                            if *gain <= 0.0 {
                                continue;
                            }
                            let x = patch.origin[0] + (i % patch.side) as f64 * patch.spacing;
                            let y = patch.origin[1] + (i / patch.side) as f64 * patch.spacing;
                            let mut strongest = [0.0f64; 3];
                            for reference in &confirmed.support {
                                let rx = (x - reference.origin[0]) / reference.spacing;
                                let ry = (y - reference.origin[1]) / reference.spacing;
                                let bounds = 0.0..=(reference.side - 1) as f64;
                                if !bounds.contains(&rx) || !bounds.contains(&ry) {
                                    continue;
                                }
                                let mut value = bilinear(
                                    &reference.values,
                                    reference.side,
                                    reference.side,
                                    rx,
                                    ry,
                                );
                                for largest in &mut strongest {
                                    if value > *largest {
                                        std::mem::swap(&mut value, largest);
                                    }
                                }
                            }
                            let agreement = strongest[2] * peak / f64::from(*gain);
                            *gain *= ((agreement - 0.25) * 4.0).clamp(0.0, 1.0) as f32;
                        }
                        if patch.values.iter().all(|&value| value <= 0.0) {
                            continue;
                        }
                    }
                    patch.origin[0] += target.geometry.crop[0] as f64;
                    patch.origin[1] += target.geometry.crop[1] as f64;
                    spots.push(Spot {
                        sensor_x: target.geometry.crop[0] as f64
                            + (candidate.x + 0.5) * target.step as f64
                            - 0.5,
                        sensor_y: target.geometry.crop[1] as f64
                            + (candidate.y + 0.5) * target.step as f64
                            - 0.5,
                        radius_sigma: candidate.sigma * target.step as f64,
                        supporting_images: confirmed.supporting_images,
                        distinct_views: confirmed.distinct_views,
                        fit_error: candidate.fit_error,
                        maximum_gain: f64::from(patch.values.iter().copied().fold(0.0, f32::max))
                            .exp(),
                    });
                    patches.push(patch);
                    matched.push(confirmed);
                }
            }
        }
        // Each patch was pushed with its spot. Compact contributions accumulate; confirmed
        // extra shapes become fields, which take the maximum and are refined at full
        // resolution.
        let step = target.step as f64;
        let mut fields = fields;
        for field in &mut fields {
            field.spot = Some(spots.len());
            let (sensor_x, sensor_y) = sensor(field.center[0], field.center[1]);
            spots.push(Spot {
                sensor_x,
                sensor_y,
                radius_sigma: (field.area as f64 / std::f64::consts::PI).sqrt() * step,
                supporting_images: field.views,
                distinct_views: field.views,
                fit_error: field.error,
                maximum_gain: f64::from(field.gain.iter().copied().fold(0.0, f32::max)).exp(),
            });
        }
        let stacked_regions = fields.len();
        // The same dirt is small and sharp stopped down and broad and soft wide open, so a
        // target may show confirmed dirt in a shape none of its candidates matched: there
        // the target is measured where the references place the dirt.
        if let (Some(group), Some(own)) = (group, &target.stack) {
            let planned = |x: f64, y: f64| -> f64 {
                let mut gain = 0.0;
                for patch in &patches {
                    let px = (x - patch.origin[0]) / patch.spacing;
                    let py = (y - patch.origin[1]) / patch.spacing;
                    let bounds = 0.0..=(patch.side - 1) as f64;
                    if bounds.contains(&px) && bounds.contains(&py) {
                        gain += bilinear(&patch.values, patch.side, patch.side, px, py);
                    }
                }
                for field in &fields {
                    if let Some(measured) = field.sample(&field.gain, x, y) {
                        gain = gain.max(measured);
                    }
                }
                gain
            };
            let measured: Vec<Field> = group
                .spots
                .par_iter()
                .filter(|spot| !matched.iter().any(|other| std::ptr::eq(*other, *spot)))
                .filter_map(|spot| Field::at_spot(spot, target, own, &planned).ok())
                .collect();
            for mut field in measured {
                let (sensor_x, sensor_y) = sensor(field.center[0], field.center[1]);
                // Several confirmed shapes of one piece of dirt measure the same shadow.
                if fields[stacked_regions..]
                    .iter()
                    .any(|other| other.covers(sensor_x, sensor_y))
                {
                    continue;
                }
                field.spot = Some(spots.len());
                spots.push(Spot {
                    sensor_x,
                    sensor_y,
                    radius_sigma: (field.area as f64 / std::f64::consts::PI).sqrt() * step,
                    supporting_images: field.views,
                    distinct_views: field.views,
                    fit_error: field.error,
                    maximum_gain: f64::from(field.gain.iter().copied().fold(0.0, f32::max)).exp(),
                });
                fields.push(field);
            }
        }
        let mut compact = Vec::new();
        for (spot, patch) in patches.into_iter().enumerate() {
            if patch.irregular {
                fields.push(Field::from_patch(patch, step, Some(spot)));
            } else {
                compact.push(patch);
            }
        }
        let patches = compact;
        if target.stack.is_some() {
            // Extra mode also refines compact shadows, which stay additive patches.
            fields.extend(
                patches
                    .iter()
                    .map(|patch| Field::from_patch(patch.clone(), step, None)),
            );
        }
        let warning = if group.is_none() {
            Some("no matching reference sensor geometry/crop; pixels are unchanged")
        } else if group.is_some_and(|group| group.distinct_views < 3) {
            Some(
                "at least three distinct reference views with matching geometry are needed; pixels are unchanged",
            )
        } else if patches.is_empty() && fields.is_empty() {
            Some("no confidently recurring, locally measurable dirt shadows; pixels are unchanged")
        } else {
            None
        };
        Plan {
            geometry: target.geometry.clone(),
            patches,
            fields,
            report: Report {
                method: "multi_image_local_flat_field",
                strength: 1.0,
                reference_images: self.reference_images,
                compatible_reference_images: group.map_or(0, |group| group.images),
                distinct_reference_views: group.map_or(0, |group| group.distinct_views),
                candidate_spots: target.candidates.len(),
                confirmed_sensor_spots: group.map_or(0, |group| group.spots.len()),
                stacked_regions,
                spots,
                changed_samples: 0,
                clipped_samples: 0,
                maximum_gain: 1.0,
                warning,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Spot {
    pub sensor_x: f64,
    pub sensor_y: f64,
    pub radius_sigma: f64,
    pub supporting_images: usize,
    pub distinct_views: usize,
    pub fit_error: f64,
    pub maximum_gain: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub method: &'static str,
    pub strength: f64,
    pub reference_images: usize,
    pub compatible_reference_images: usize,
    pub distinct_reference_views: usize,
    pub candidate_spots: usize,
    pub confirmed_sensor_spots: usize,
    pub stacked_regions: usize,
    pub spots: Vec<Spot>,
    pub changed_samples: usize,
    pub clipped_samples: usize,
    pub maximum_gain: f64,
    pub warning: Option<&'static str>,
}

pub struct Plan {
    geometry: Geometry,
    patches: Vec<Patch>,
    fields: Vec<Field>,
    pub report: Report,
}

impl Plan {
    pub fn apply(&self, image: &mut MonoImage) -> Result<Report> {
        self.apply_with_strength(image, 1.0)
    }

    pub fn apply_with_strength(&self, image: &mut MonoImage, strength: f64) -> Result<Report> {
        let refinement = self.refine(&[&*image])?;
        self.apply_refined(image, strength, &refinement)
    }

    // Measure, once for all planes of a RAW, what the preview-resolution stack cannot
    // express: sharp shadow edges and fringes inside confirmed supports on smooth scenes.
    pub fn refine(&self, images: &[&MonoImage]) -> Result<Refinement> {
        let Some(first) = images.first() else {
            return Ok(Refinement {
                crops: Vec::new(),
                scenery: Vec::new(),
            });
        };
        let metadata = &first.metadata;
        ensure!(
            images.iter().all(|image| {
                let other = &image.metadata;
                self.geometry.accepts(other)
                    && (other.width, other.height, other.crop, other.orientation)
                        == (
                            metadata.width,
                            metadata.height,
                            metadata.crop,
                            metadata.orientation,
                        )
                    && other.width.checked_mul(other.height) == Some(image.pixels.len())
                    && other.width > 0
                    && other.height > 0
            }),
            "sensor-dirt plan does not match the target raw geometry/crop"
        );
        let layout = self.layout(metadata, &[]);
        let mapping = &layout.mapping;
        // Black-subtracted signal of all planes at one pixel; None when any sample is
        // unusable. A lone image may be an undeveloped color mosaic, so it is read through
        // a kernel that weighs the four sites of any 2x2 pattern equally.
        let signal = |x: usize, y: usize| -> Option<f64> {
            let sample = |image: &MonoImage, x: usize, y: usize| {
                let value = f64::from(image.pixels[y * metadata.width + x]);
                (value > image.metadata.black_level
                    && value < f64::from(image.metadata.white_level))
                .then_some(value - image.metadata.black_level)
            };
            if let [image] = images {
                if x == 0 || y == 0 || x + 1 >= metadata.width || y + 1 >= metadata.height {
                    return None;
                }
                let mut sum = 0.0;
                for (dy, vertical) in [1.0, 2.0, 1.0].into_iter().enumerate() {
                    for (dx, horizontal) in [1.0, 2.0, 1.0].into_iter().enumerate() {
                        sum += vertical * horizontal * sample(image, x + dx - 1, y + dy - 1)?;
                    }
                }
                Some(sum)
            } else {
                images.iter().map(|image| sample(image, x, y)).sum()
            }
        };
        let outcomes: Vec<Result<Crop, usize>> = self
            .fields
            .par_iter()
            .enumerate()
            .filter(|(_, field)| field.refine)
            .filter_map(|(index, field)| {
                let [w, h] = field.size;
                // Grid cells and analysis-preview pixels in output pixels; planes of one RAW
                // share this geometry.
                let cell = field.spacing / mapping.scale[0];
                let coarse = field.step / mapping.scale[0];
                let covered: Vec<usize> = (0..w * h).filter(|&i| field.support[i] > 0.02).collect();
                let strongest = field.gain.iter().copied().fold(0.0, f32::max);
                if covered.is_empty() || strongest <= 0.0 {
                    return None;
                }
                let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
                for &i in &covered {
                    x0 = x0.min(i % w);
                    x1 = x1.max(i % w);
                    y0 = y0.min(i / w);
                    y1 = y1.max(i / w);
                }
                // Half-thickness of the support: how far a background must be interpolated.
                // Across a broad shadow that is no measurement; its fitted gain stands.
                let body: Vec<bool> = field.support.iter().map(|value| *value > 0.5).collect();
                let reach = thickness(&body, w, h, usize::MAX) as f64 * cell;
                if reach > 16.0 * coarse {
                    return None;
                }
                let reach = reach.max(1.5 * coarse);
                let pad = (3.0 * reach).ceil() + 2.0;
                let bounds = mapping.bounds(
                    [
                        field.origin[0] + x0 as f64 * field.spacing,
                        field.origin[1] + y0 as f64 * field.spacing,
                    ],
                    [
                        (x1 - x0) as f64 * field.spacing,
                        (y1 - y0) as f64 * field.spacing,
                    ],
                );
                let left = (bounds.0 - pad).floor().max(0.0) as usize;
                let top = (bounds.1 - pad).floor().max(0.0) as usize;
                let right = ((bounds.2 + pad).ceil().max(0.0) as usize).min(metadata.width - 1);
                let bottom = ((bounds.3 + pad).ceil().max(0.0) as usize).min(metadata.height - 1);
                if right < left + 8 || bottom < top + 8 {
                    return None;
                }
                let (cw, ch) = (right - left + 1, bottom - top + 1);
                let mut level = vec![0.0f32; cw * ch];
                let mut valid = vec![0.0f32; cw * ch];
                let mut support = vec![0.0f32; cw * ch];
                let mut template = vec![0.0f32; cw * ch];
                let mut planned = vec![0.0f64; cw];
                for i in 0..cw * ch {
                    let (x, y) = (left + i % cw, top + i / cw);
                    if i % cw == 0 {
                        // The whole plan, so neighbors and overlaps are already corrected.
                        planned.fill(0.0);
                        layout.row(y, left, &mut planned);
                    }
                    let (raw_x, raw_y) = mapping.to_raw(x, y);
                    if let Some(value) = field.sample(&field.support, raw_x, raw_y) {
                        support[i] = value as f32;
                        template[i] = field.sample(&field.gain, raw_x, raw_y).unwrap_or(0.0) as f32;
                    }
                    if let Some(signal) = signal(x, y) {
                        valid[i] = 1.0;
                        level[i] = (signal.ln() + planned[i % cw]) as f32;
                    }
                }
                // Residual against a background interpolated from pixels where `outside` is
                // set, so neither the shadow nor the judged pixels can hide in it.
                let leveled = |outside: &dyn Fn(usize) -> bool| -> (Vec<f32>, Vec<f32>) {
                    let weight: Vec<f32> = (0..cw * ch)
                        .map(|i| if outside(i) { valid[i] } else { 0.0 })
                        .collect();
                    let weighted: Vec<f32> =
                        level.iter().zip(&weight).map(|(a, b)| a * b).collect();
                    let numerator = gaussian(&weighted, cw, ch, reach, false);
                    let denominator = gaussian(&weight, cw, ch, reach, false);
                    let mut known = vec![0.0f32; cw * ch];
                    let mut residual = vec![0.0f32; cw * ch];
                    for i in 0..cw * ch {
                        if valid[i] > 0.0 && denominator[i] > 1e-3 {
                            known[i] = 1.0;
                            residual[i] = level[i] - numerator[i] / denominator[i];
                        }
                    }
                    (residual, known)
                };
                let (mut residual, known) = leveled(&|i| support[i] <= 0.02);
                let smoothed = |values: &[f32], known: &[f32], sigma: f64| -> Vec<f32> {
                    let sum = gaussian(values, cw, ch, sigma, false);
                    let weight = gaussian(known, cw, ch, sigma, false);
                    sum.iter()
                        .zip(&weight)
                        .map(|(sum, weight)| if *weight > 1e-3 { sum / weight } else { 0.0 })
                        .collect()
                };
                // A shadow is soft. Sharp darkness far beyond what the planned gain explains
                // is a scene object under it (a bird, a rock) that must keep its tone: an
                // extra shape is then dropped, and a compact one is left as fitted.
                let unexplained = -(3.0 * strongest).max(0.15);
                let sharp = smoothed(&residual, &known, 1.25);
                let foreign = (0..cw * ch)
                    .filter(|&i| known[i] > 0.0 && support[i] > 0.5 && sharp[i] < unexplained)
                    .count();
                if foreign >= 4 {
                    return field.spot.map(|_| Err(index));
                }
                let limit = 0.5 * strongest + 0.03;
                let mut delta = vec![0.0f32; cw * ch];
                // First the error common to each contour of the shadow: averaging along the
                // contour keeps scene detail and noise out of the correction.
                let shape = gaussian(&template, cw, ch, coarse.max(1.0), false);
                let floor = 0.02 * shape.iter().copied().fold(0.0, f32::max);
                let mut members: Vec<usize> = (0..cw * ch)
                    .filter(|&i| known[i] > 0.0 && shape[i] > floor)
                    .collect();
                members.sort_unstable_by(|a, b| shape[*a].total_cmp(&shape[*b]).then(a.cmp(b)));
                // The interpolated background can sit off across a cloud edge or gradient.
                // The contour just outside the shadow carries that offset and no shadow.
                let mut outside: Vec<f32> = (0..cw * ch)
                    .filter(|&i| known[i] > 0.0 && shape[i] > 0.25 * floor && shape[i] <= floor)
                    .map(|i| residual[i])
                    .collect();
                let anchor = if outside.len() >= 16 {
                    stack_median(&mut outside).0
                } else {
                    0.0
                };
                // Coarse contours first; narrow ones then sharpen edges where grain allows.
                for (population, most) in [(48, 32), (12, 128)] {
                    let bins = (members.len() / population).min(most);
                    if bins < 4 {
                        continue;
                    }
                    let mut profile = vec![(floor, 0.0f32)];
                    for bin in 0..bins {
                        let group =
                            &members[bin * members.len() / bins..(bin + 1) * members.len() / bins];
                        let center =
                            group.iter().map(|&i| shape[i]).sum::<f32>() / group.len() as f32;
                        let mut values: Vec<f32> =
                            group.iter().map(|&i| residual[i] - anchor).collect();
                        let (median, spread) = stack_median(&mut values);
                        // Shrink by twice the standard error of a median.
                        let error = 2.0 * 1.2533 * spread / (group.len() as f32).sqrt();
                        profile.push((center, median.signum() * (median.abs() - error).max(0.0)));
                    }
                    for &i in &members {
                        let next = profile.partition_point(|point| point.0 < shape[i]);
                        let value = if next == profile.len() {
                            profile[next - 1].1
                        } else {
                            let (a, b) = (profile[next - 1], profile[next]);
                            let t = (shape[i] - a.0) / (b.0 - a.0).max(1e-12);
                            a.1 + t * (b.1 - a.1)
                        };
                        let change = value.clamp(-limit, limit);
                        delta[i] -= change;
                        residual[i] -= change;
                    }
                }
                // Then local structure, only where it clearly exceeds the equally
                // interpolated band just outside the support.
                let halo = gaussian(&support, cw, ch, 1.5 * coarse, false);
                let banded = |i: usize| support[i] <= 0.02 && halo[i] > 0.05;
                let (reference, comparable) = leveled(&|i| support[i] <= 0.02 && !banded(i));
                let band: Vec<usize> = (0..cw * ch)
                    .filter(|&i| comparable[i] > 0.0 && banded(i))
                    .collect();
                let mut stages = vec![1.25];
                if 0.67 * coarse > 1.5 * 1.25 {
                    stages.insert(0, 0.67 * coarse);
                }
                for sigma in stages {
                    // Band-pass at the stage's own scale: larger scene structure, which the
                    // interpolated background cannot follow, must not decide or be changed.
                    let detail = |values: &[f32], known: &[f32]| -> Vec<f32> {
                        smoothed(values, known, sigma)
                            .iter()
                            .zip(smoothed(values, known, 3.0 * sigma))
                            .map(|(fine, broad)| fine - broad)
                            .collect()
                    };
                    let smooth = detail(&residual, &known);
                    let around = detail(&reference, &comparable);
                    let (mut inner, mut inner_count) = (0.0, 0);
                    for i in (0..cw * ch).filter(|&i| known[i] > 0.0 && support[i] > 0.5) {
                        inner += f64::from(smooth[i]).powi(2);
                        inner_count += 1;
                    }
                    let mut outer: Vec<f32> = band.iter().map(|&i| around[i] * around[i]).collect();
                    if inner_count < 4 || outer.len() < 16 {
                        continue;
                    }
                    // Median energy (rescaled to a Gaussian mean) ignores stray scene detail.
                    let middle = outer.len() / 2;
                    let scale =
                        f64::from(*outer.select_nth_unstable_by(middle, f32::total_cmp).1) / 0.4549;
                    // Scene structure crossing the band (a wire, a branch) also crosses the
                    // shadow and must survive: local detail is then left alone.
                    let crossing = outer
                        .iter()
                        .filter(|energy| f64::from(**energy) > (16.0 * scale).max(4e-4))
                        .count();
                    if inner < 2.0 * scale * inner_count as f64 || crossing * 200 > outer.len() {
                        continue;
                    }
                    for i in 0..cw * ch {
                        let change = (support[i] * smooth[i]).clamp(-limit, limit);
                        delta[i] -= change;
                        if known[i] > 0.0 {
                            residual[i] -= change;
                        }
                    }
                }
                delta.iter().any(|value| *value != 0.0).then_some(Ok(Crop {
                    left,
                    top,
                    width: cw,
                    height: ch,
                    delta,
                }))
            })
            .collect();
        let mut refinement = Refinement {
            crops: Vec::new(),
            scenery: Vec::new(),
        };
        for outcome in outcomes {
            match outcome {
                Ok(crop) => refinement.crops.push(crop),
                Err(index) => refinement.scenery.push(index),
            }
        }
        Ok(refinement)
    }

    pub fn apply_refined(
        &self,
        image: &mut MonoImage,
        strength: f64,
        refinement: &Refinement,
    ) -> Result<Report> {
        ensure!(
            strength.is_finite() && (0.0..=2.0).contains(&strength),
            "sensor-dirt correction strength must be finite and in [0, 2]"
        );
        let metadata = &image.metadata;
        ensure!(
            self.geometry.accepts(metadata)
                && metadata.width > 0
                && metadata.height > 0
                && metadata.width.checked_mul(metadata.height) == Some(image.pixels.len()),
            "sensor-dirt plan does not match the target raw geometry/crop"
        );
        let mut report = self.report.clone();
        report.strength = strength;
        // Shadows disproved on the developed samples leave the report with their gain.
        let dropped: Vec<usize> = refinement
            .scenery
            .iter()
            .filter_map(|&index| self.fields.get(index)?.spot)
            .collect();
        if !dropped.is_empty() {
            let mut next = 0;
            report.spots.retain(|_| {
                next += 1;
                !dropped.contains(&(next - 1))
            });
            report.stacked_regions -= refinement
                .scenery
                .iter()
                .filter(|&&index| index < self.report.stacked_regions)
                .count();
        }
        if strength != 1.0 {
            for spot in &mut report.spots {
                spot.maximum_gain = spot.maximum_gain.powf(strength).min(MAX_LOG_GAIN.exp());
            }
        }
        if strength == 0.0 {
            report.warning = Some("zero sensor-dirt strength; pixels are unchanged");
            return Ok(report);
        }
        if self.patches.is_empty() && self.fields.is_empty() {
            return Ok(report);
        }
        let layout = self.layout(metadata, &refinement.scenery);
        let black = metadata.black_level;
        let white = f64::from(metadata.white_level);
        let stats = image
            .pixels
            .par_chunks_mut(metadata.width)
            .enumerate()
            .map(|(y, row)| {
                let refined: Vec<&Crop> = refinement
                    .crops
                    .iter()
                    .filter(|crop| (crop.top..crop.top + crop.height).contains(&y))
                    .collect();
                if !layout.touches(y) && refined.is_empty() {
                    return (0usize, 0usize, 1.0f64);
                }
                let mut gains = vec![0.0f64; row.len()];
                layout.row(y, 0, &mut gains);
                if !refined.is_empty() {
                    // Overlapping refinements measured one residual: keep the larger, not the sum.
                    let mut deltas = vec![0.0f32; row.len()];
                    for crop in refined {
                        let line = &crop.delta[(y - crop.top) * crop.width..][..crop.width];
                        for (delta, measured) in deltas[crop.left..].iter_mut().zip(line) {
                            if measured.abs() > delta.abs() {
                                *delta = *measured;
                            }
                        }
                    }
                    for (gain, delta) in gains.iter_mut().zip(deltas) {
                        *gain += f64::from(delta);
                    }
                }
                let (mut changed, mut clipped, mut maximum) = (0usize, 0usize, 1.0f64);
                for (pixel, log_gain) in row.iter_mut().zip(gains) {
                    let value = f64::from(*pixel);
                    if log_gain == 0.0 || value <= black || value >= white {
                        continue;
                    }
                    let gain = (log_gain * strength).clamp(-0.25, MAX_LOG_GAIN).exp();
                    let corrected = black + (value - black) * gain;
                    let output = corrected.min(white).round() as u16;
                    changed += usize::from(output != *pixel);
                    clipped += usize::from(corrected > white);
                    maximum = maximum.max(gain);
                    *pixel = output;
                }
                (changed, clipped, maximum)
            })
            .reduce(|| (0, 0, 1.0), |a, b| (a.0 + b.0, a.1 + b.1, a.2.max(b.2)));
        report.changed_samples = stats.0;
        report.clipped_samples = stats.1;
        report.maximum_gain = stats.2;
        Ok(report)
    }
}

fn odd_reflection(index: isize, length: usize) -> (usize, Option<usize>) {
    let last = length as isize - 1;
    if index < 0 {
        ((-index).min(last) as usize, Some(0))
    } else if index > last {
        ((2 * last - index).max(0) as usize, Some(last as usize))
    } else {
        (index as usize, None)
    }
}

// Separable Gaussian. `odd` extends borders point-symmetrically so linear trends survive;
// otherwise borders replicate, which keeps weights nonnegative for normalized convolution.
fn gaussian(values: &[f32], width: usize, height: usize, sigma: f64, odd: bool) -> Vec<f32> {
    let radius = (3.0 * sigma).ceil().max(1.0) as usize;
    let mut kernel: Vec<f32> = (0..=2 * radius)
        .map(|i| {
            let distance = i as f64 - radius as f64;
            (-0.5 * distance * distance / (sigma * sigma)).exp() as f32
        })
        .collect();
    let total: f32 = kernel.iter().sum();
    kernel.iter_mut().for_each(|weight| *weight /= total);
    let sample = |line: &dyn Fn(usize) -> f32, index: isize, length: usize| {
        let (position, edge) = odd_reflection(index, length);
        match edge {
            None => line(position),
            Some(edge) if odd => 2.0 * line(edge) - line(position),
            Some(edge) => line(edge),
        }
    };
    let mut rows = vec![0.0f32; values.len()];
    rows.par_chunks_mut(width)
        .zip(values.par_chunks(width))
        .for_each(|(out, line)| {
            for (x, value) in out.iter_mut().enumerate() {
                let mut sum = 0.0f32;
                if x >= radius && x + radius < width {
                    for (weight, source) in kernel.iter().zip(&line[x - radius..=x + radius]) {
                        sum += weight * source;
                    }
                } else {
                    for (k, weight) in kernel.iter().enumerate() {
                        let index = x as isize + k as isize - radius as isize;
                        sum += weight * sample(&|i| line[i], index, width);
                    }
                }
                *value = sum;
            }
        });
    let mut result = vec![0.0f32; values.len()];
    result
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, out)| {
            for (k, weight) in kernel.iter().enumerate() {
                let index = y as isize + k as isize - radius as isize;
                let (position, edge) = odd_reflection(index, height);
                let line = &rows[position * width..(position + 1) * width];
                match edge {
                    None => {
                        for (value, source) in out.iter_mut().zip(line) {
                            *value += weight * source;
                        }
                    }
                    Some(edge) => {
                        let edge = &rows[edge * width..(edge + 1) * width];
                        for ((value, source), edge) in out.iter_mut().zip(line).zip(edge) {
                            *value += weight * if odd { 2.0 * edge - source } else { *edge };
                        }
                    }
                }
            }
        });
    result
}

// Half-thickness of a mask in cells: how many 4-neighbor erosions empty it, up to `limit`.
fn thickness(mask: &[bool], width: usize, height: usize, limit: usize) -> usize {
    let mut body = mask.to_vec();
    let mut depth = 0;
    while depth < limit && body.iter().any(|inside| *inside) {
        depth += 1;
        body = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                body[i]
                    && x > 0
                    && y > 0
                    && x + 1 < width
                    && y + 1 < height
                    && body[i - 1]
                    && body[i + 1]
                    && body[i - width]
                    && body[i + width]
            })
            .collect();
    }
    depth
}

fn stack_value(code: i16) -> f32 {
    f32::from(code >> 1) / STACK_UNITS
}

fn stack_smooth(code: i16) -> bool {
    code & 1 != 0
}

// Median and scaled MAD; even counts take the middle value nearer zero, so half of the
// frames can never establish a shadow on their own.
fn stack_median(values: &mut [f32]) -> (f32, f32) {
    values.sort_unstable_by(f32::total_cmp);
    let count = values.len();
    let (low, high) = (values[(count - 1) / 2], values[count / 2]);
    let median = if low.abs() <= high.abs() { low } else { high };
    for value in values.iter_mut() {
        *value = (*value - median).abs();
    }
    values.sort_unstable_by(f32::total_cmp);
    (median, values[(count - 1) / 2] * 1.4826)
}

struct StackMap {
    median: Vec<f32>,
    spread: Vec<f32>,
    count: Vec<u8>,
}

impl StackMap {
    fn new(frames: &[&[i16]], width: usize) -> Self {
        let length = frames[0].len();
        let mut median = vec![0.0f32; length];
        let mut spread = vec![0.0f32; length];
        let mut count = vec![0u8; length];
        median
            .par_chunks_mut(width)
            .zip(spread.par_chunks_mut(width))
            .zip(count.par_chunks_mut(width))
            .enumerate()
            .for_each(|(y, ((median, spread), count))| {
                let mut values = [0.0f32; STACK_POOL];
                for x in 0..median.len() {
                    let mut used = 0;
                    for frame in frames {
                        let code = frame[y * width + x];
                        if stack_smooth(code) {
                            values[used] = stack_value(code);
                            used += 1;
                        }
                    }
                    if used > 0 {
                        (median[x], spread[x]) = stack_median(&mut values[..used]);
                    }
                    count[x] = used as u8;
                }
            });
        Self {
            median,
            spread,
            count,
        }
    }

    fn score(&self, i: usize) -> f32 {
        self.median[i] / (self.spread[i].max(0.002) / f32::from(self.count[i].max(1)).sqrt())
    }

    // Isolated dark features supported by enough smooth views on an otherwise flat stack.
    fn regions(&self, width: usize, height: usize) -> Vec<Region> {
        let enough = |i: usize| usize::from(self.count[i]) >= STACK_MIN_VIEWS;
        let grows = |i: usize| enough(i) && self.median[i] < -0.004 && self.score(i) < -3.0;
        let seeds = |i: usize| enough(i) && self.median[i] < -0.008 && self.score(i) < -6.0;
        let mut visited = vec![false; width * height];
        let mut regions = Vec::new();
        for start in 0..width * height {
            if visited[start] || !seeds(start) {
                continue;
            }
            visited[start] = true;
            let mut pixels = vec![start];
            let mut next = 0;
            while next < pixels.len() {
                let (x, y) = (pixels[next] % width, pixels[next] / width);
                next += 1;
                for sy in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                    for sx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                        let i = sy * width + sx;
                        if !visited[i] && grows(i) {
                            visited[i] = true;
                            pixels.push(i);
                        }
                    }
                }
            }
            let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
            for &i in &pixels {
                left = left.min(i % width);
                right = right.max(i % width);
                top = top.min(i / width);
                bottom = bottom.max(i / width);
            }
            // The residual's own high-pass flattens broad shadows; the parametric fits
            // keep those.
            if !(3..=1000).contains(&pixels.len())
                || left < 2
                || top < 2
                || right + 3 > width
                || bottom + 3 > height
            {
                continue;
            }
            let x0 = left.saturating_sub(STACK_MARGIN);
            let y0 = top.saturating_sub(STACK_MARGIN);
            let w = (right + STACK_MARGIN + 1).min(width) - x0;
            let h = (bottom + STACK_MARGIN + 1).min(height) - y0;
            let mut core = vec![false; w * h];
            for &i in &pixels {
                core[(i / width - y0) * w + i % width - x0] = true;
            }
            let dilated = |reach: usize| -> Vec<bool> {
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        (y.saturating_sub(reach)..=(y + reach).min(h - 1)).any(|sy| {
                            (x.saturating_sub(reach)..=(x + reach).min(w - 1)).any(|sx| {
                                core[sy * w + sx]
                                    && x.abs_diff(sx).pow(2) + y.abs_diff(sy).pow(2)
                                        <= reach * reach + 1
                            })
                        })
                    })
                    .collect()
            };
            let near = dilated(2);
            if thickness(&core, w, h, 7) > 6 {
                continue;
            }
            let median: Vec<f32> = (0..w * h)
                .map(|i| self.median[(y0 + i / w) * width + x0 + i % w])
                .collect();
            let peak = pixels.iter().map(|&i| self.median[i]).fold(0.0, f32::min);
            let mut ring: Vec<f32> = (0..w * h)
                .filter(|&i| !near[i] && enough((y0 + i / w) * width + x0 + i % w))
                .map(|i| median[i])
                .collect();
            if ring.len() < 8 {
                continue;
            }
            let noise = f64::from(stack_median(&mut ring).1);
            // A shadow counts only on a flat stack: textured bands breed look-alikes.
            if noise > 0.004f64.max((0.2 * f64::from(peak.abs())).min(0.012)) {
                continue;
            }
            // Scene comparisons must not see the shadow's own penumbra, fringes or neighbors.
            let persistent = (3.0 * noise).max(0.006) as f32;
            let quiet: Vec<bool> = dilated(5)
                .iter()
                .zip(&median)
                .map(|(close, value)| !close && value.abs() <= persistent)
                .collect();
            if quiet.iter().filter(|quiet| **quiet).count() < 16 {
                continue;
            }
            regions.push(Region {
                window: [x0, y0, w, h],
                core,
                near,
                quiet,
                median,
                area: pixels.len(),
                bounds: [right - left + 1, bottom - top + 1],
                peak,
                noise,
            });
        }
        regions
    }
}

// Why a target cannot measure a stacked shadow itself.
enum Unmeasured {
    // Surroundings too rough: only an aperture-matched stack can still speak.
    Rough,
    // Smooth but uneven surroundings (cloud bands, gradients) would be flattened along
    // with the shadow: the stack's own shape, scaled to the target, is applied instead.
    Uneven,
    // The target does not show the shadow, or dark scene structure (a wire, a branch)
    // touches it and would be measured as dirt.
    Scenery,
}

struct Region {
    window: [usize; 4],
    core: Vec<bool>,
    near: Vec<bool>,
    quiet: Vec<bool>,
    median: Vec<f32>,
    area: usize,
    bounds: [usize; 2],
    peak: f32,
    noise: f64,
}

impl Region {
    fn crop(&self, frame: &[i16], width: usize) -> Vec<i16> {
        let [x0, y0, w, h] = self.window;
        (0..w * h)
            .map(|i| frame[(y0 + i / w) * width + x0 + i % w])
            .collect()
    }

    fn template(&self, median: &[f32]) -> Vec<f32> {
        median
            .iter()
            .zip(&self.near)
            .map(|(value, near)| if *near { value.min(0.0) } else { 0.0 })
            .collect()
    }

    // Whether the surroundings of the shadow are plain in one frame. Bands, edges and
    // branches leave look-alike shadows in a high-passed residual, so such a frame can
    // neither vote for dirt nor have dirt measured on it.
    fn plain(&self, codes: &[i16]) -> bool {
        let mut values: Vec<f32> = (0..codes.len())
            .filter(|&i| self.quiet[i] && stack_smooth(codes[i]))
            .map(|i| stack_value(codes[i]) - self.median[i])
            .collect();
        if values.len() < 16 {
            return false;
        }
        // Sorted absolute deviations remain after the median.
        let limit = (4.0 * stack_median(&mut values).1).max(0.03);
        values
            .iter()
            .filter(|deviation| **deviation > limit)
            .count()
            * 10
            <= values.len()
    }

    // The frame's shadow over the window: how far each cell lies below the plane through
    // the given ring cells, fitted twice so stray detail does not tilt it, and the spread
    // of those ring cells around the plane.
    fn shadow(&self, data: &[f32], ring: &[usize]) -> Option<(Vec<f32>, f64)> {
        let [_, _, w, h] = self.window;
        let basis = |i: usize| {
            [
                1.0,
                (i % w) as f64 - w as f64 * 0.5,
                (i / w) as f64 - h as f64 * 0.5,
            ]
        };
        let level = |plane: [f64; 3], i: usize| -> f64 {
            basis(i).iter().zip(plane).map(|(a, b)| a * b).sum()
        };
        let mut plane = [0.0; 3];
        let mut noise = f64::INFINITY;
        for _ in 0..2 {
            let mut normal = [[0.0; 3]; 3];
            let mut right = [0.0; 3];
            for &i in ring {
                let value = f64::from(data[i]);
                if (value - level(plane, i)).abs() > 3.0 * noise {
                    continue;
                }
                let basis = basis(i);
                for a in 0..3 {
                    right[a] += basis[a] * value;
                    for b in 0..3 {
                        normal[a][b] += basis[a] * basis[b];
                    }
                }
            }
            plane = solve(normal, right)?;
            let mut errors: Vec<f32> = ring
                .iter()
                .map(|&i| (f64::from(data[i]) - level(plane, i)) as f32)
                .collect();
            noise = f64::from(stack_median(&mut errors).1).max(1e-4);
        }
        let shadow = (0..w * h)
            .map(|i| (level(plane, i) - f64::from(data[i])) as f32)
            .collect();
        Some((shadow, noise))
    }

    // The target's own shadow against the plane of its quiet ring, and where it lies. The
    // stack, often of other apertures, only locates the shadow; `expected` is the summed
    // depth it predicts for this target. Where the texture flags call the surroundings
    // smooth only their flagged cells are trusted; elsewhere the whole ring is measured,
    // because heavy grain trips those flags on plain sky.
    fn measure(
        &self,
        data: &[f32],
        codes: &[i16],
        expected: f64,
        smooth: bool,
    ) -> Result<(Vec<f32>, Vec<bool>), Unmeasured> {
        let [_, _, w, h] = self.window;
        let ring: Vec<usize> = (0..w * h)
            .filter(|&i| self.quiet[i] && (!smooth || stack_smooth(codes[i])))
            .collect();
        let (mut shadow, mut noise) = self.shadow(data, &ring).ok_or(Unmeasured::Scenery)?;
        // Real texture cannot be told from shadow cell by cell.
        if noise > 0.05 {
            return Err(Unmeasured::Rough);
        }
        // Unevenness of the surroundings at the scale of a shadow (cloud bands, gradients):
        // the spread of the ring once its grain is averaged out, against the shadow's depth
        // averaged the same way.
        let mut inside = vec![0.0f32; w * h];
        let mut around = vec![0.0f32; w * h];
        for &i in &ring {
            inside[i] = 1.0;
            around[i] = -shadow[i];
        }
        let sums = gaussian(&around, w, h, 1.5, false);
        let weights = gaussian(&inside, w, h, 1.5, false);
        let mut averaged: Vec<f32> = ring
            .iter()
            .filter(|&&i| weights[i] > 0.5)
            .map(|&i| sums[i] / weights[i])
            .collect();
        if averaged.len() < 16 {
            return Err(Unmeasured::Scenery);
        }
        let uneven = f64::from(stack_median(&mut averaged).1);
        let depth = gaussian(&shadow, w, h, 1.5, false)
            .iter()
            .zip(&self.near)
            .filter(|(_, near)| **near)
            .map(|(value, _)| f64::from(*value))
            .fold(0.0, f64::max);
        if depth < 4.0 * uneven {
            return Err(Unmeasured::Scenery);
        }
        let deepest = (0..w * h)
            .filter(|&i| self.near[i])
            .map(|i| shadow[i])
            .fold(0.0, f32::max);
        if f64::from(deepest) < 4.0 * noise {
            // A faint shadow in grain is averaged over neighbors, or the grain itself
            // would be flattened into a bright patch.
            shadow = gaussian(&shadow, w, h, 1.0, false);
            noise *= 0.3;
        }
        let gain: Vec<f32> = shadow
            .iter()
            .map(|&shadow| (f64::from(shadow) - 0.5 * noise).clamp(0.0, MAX_LOG_GAIN) as f32)
            .collect();
        let spread = |mask: &[bool], above: f32| -> Vec<bool> {
            (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    mask[i]
                        || gain[i] >= above
                            && x > 0
                            && y > 0
                            && x + 1 < w
                            && y + 1 < h
                            && (mask[i - 1] || mask[i + 1] || mask[i - w] || mask[i + w])
                })
                .collect()
        };
        // The target's shadow may be wider than the stack's: follow it a few cells out.
        let dark = (2.0 * noise).max(0.004) as f32;
        let mut near = self.near.clone();
        for _ in 0..4 {
            near = spread(&near, dark);
        }
        // Strongly dark structure that keeps going past two cells of penumbra is scenery.
        let stray = (4.0 * noise).max(0.02) as f32;
        let mut reach = near.clone();
        for _ in 0..6 {
            reach = spread(&reach, stray);
        }
        let mut close = near.clone();
        for _ in 0..2 {
            close = spread(&close, 0.0);
        }
        let touching = (0..w * h).filter(|&i| reach[i] && !close[i]).count();
        // A dust shadow keeps its summed depth across apertures; much more than the stack
        // predicts is a dark scene object sitting on a faint shadow.
        let measured: f64 = (0..w * h)
            .filter(|&i| near[i])
            .map(|i| f64::from(gain[i]))
            .sum();
        if touching >= 3 || measured > 3.0 * expected {
            Err(Unmeasured::Scenery)
        } else if uneven > 0.008 || depth < 6.0 * uneven {
            Err(Unmeasured::Uneven)
        } else {
            Ok((gain, near))
        }
    }

    // Scene content around the feature: 4x4-cell medians of the stack-free residual with a
    // quadratic surface removed. Small dirt and smooth gradients vanish; structure remains.
    fn surroundings(&self, codes: &[i16]) -> Vec<f64> {
        let [_, _, w, h] = self.window;
        let mut cells = Vec::new();
        for cy in 0..h / 4 {
            for cx in 0..w / 4 {
                let mut values: Vec<f32> = (0..16)
                    .map(|i| (cy * 4 + i / 4) * w + cx * 4 + i % 4)
                    .filter(|&i| self.quiet[i])
                    .map(|i| stack_value(codes[i]) - self.median[i])
                    .collect();
                if values.len() >= 8 {
                    let middle = values.len() / 2;
                    let value = *values.select_nth_unstable_by(middle, f32::total_cmp).1;
                    cells.push((cx as f64, cy as f64, f64::from(value)));
                }
            }
        }
        let count = cells.len().max(1) as f64;
        let mx = cells.iter().map(|cell| cell.0).sum::<f64>() / count;
        let my = cells.iter().map(|cell| cell.1).sum::<f64>() / count;
        let basis = |x: f64, y: f64| {
            let (x, y) = (x - mx, y - my);
            [1.0, x, y, x * x, x * y, y * y]
        };
        let mut normal = [[0.0; 6]; 6];
        let mut right = [0.0; 6];
        for &(x, y, value) in &cells {
            let terms = basis(x, y);
            for i in 0..6 {
                right[i] += terms[i] * value;
                for j in 0..6 {
                    normal[i][j] += terms[i] * terms[j];
                }
            }
        }
        let surface = solve(normal, right).unwrap_or_else(|| {
            let mean = cells.iter().map(|cell| cell.2).sum::<f64>() / count;
            [mean, 0.0, 0.0, 0.0, 0.0, 0.0]
        });
        cells
            .iter()
            .map(|&(x, y, value)| {
                value
                    - basis(x, y)
                        .iter()
                        .zip(surface)
                        .map(|(term, weight)| term * weight)
                        .sum::<f64>()
            })
            .collect()
    }
}

// Gaussian elimination with partial pivoting; None for a degenerate system.
fn solve<const N: usize>(mut matrix: [[f64; N]; N], mut right: [f64; N]) -> Option<[f64; N]> {
    let scale = (0..N).map(|i| matrix[i][i].abs()).fold(0.0, f64::max);
    for column in 0..N {
        let pivot = (column..N)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))?;
        if matrix[pivot][column].abs() <= 1e-9 * scale.max(1e-300) {
            return None;
        }
        matrix.swap(column, pivot);
        right.swap(column, pivot);
        for row in column + 1..N {
            let factor = matrix[row][column] / matrix[column][column];
            let pivot = matrix[column];
            for (value, pivot) in matrix[row].iter_mut().zip(pivot).skip(column) {
                *value -= factor * pivot;
            }
            right[row] -= factor * right[column];
        }
    }
    let mut result = [0.0; N];
    for row in (0..N).rev() {
        let sum: f64 = (row + 1..N).map(|j| matrix[row][j] * result[j]).sum();
        result[row] = (right[row] - sum) / matrix[row][row];
    }
    Some(result)
}

// Least-squares scale of `values` against `template` with a free offset, and its t-score.
fn matched_amplitude(values: &[f32], template: &[f32]) -> (f64, f64) {
    let count = values.len() as f64;
    let mean = |data: &[f32]| data.iter().map(|&value| f64::from(value)).sum::<f64>() / count;
    let (mt, mv) = (mean(template), mean(values));
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (&t, &v) in template.iter().zip(values) {
        sxx += (f64::from(t) - mt).powi(2);
        sxy += (f64::from(t) - mt) * (f64::from(v) - mv);
    }
    if sxx <= 1e-12 {
        return (0.0, 0.0);
    }
    let amplitude = sxy / sxx;
    let residual: f64 = template
        .iter()
        .zip(values)
        .map(|(&t, &v)| (f64::from(v) - mv - amplitude * (f64::from(t) - mt)).powi(2))
        .sum();
    let error = (residual / (count - 2.0).max(1.0) / sxx).sqrt();
    (amplitude, amplitude / error.max(1e-9))
}

// Median shadow depth of one frame relative to the stack: a metadata-free aperture proxy.
fn stack_contrast(frame: &[i16], regions: &[Region], width: usize) -> Option<f64> {
    let mut values: Vec<f64> = regions
        .iter()
        .filter(|region| region.peak <= -0.015)
        .filter_map(|region| {
            let codes = region.crop(frame, width);
            let ring = region.quiet.iter().filter(|quiet| **quiet).count();
            let flat = codes
                .iter()
                .zip(&region.quiet)
                .filter(|(code, quiet)| **quiet && stack_smooth(**code))
                .count();
            let data: Vec<f32> = codes.iter().map(|&code| stack_value(code)).collect();
            (flat * 10 >= ring * 9)
                .then(|| matched_amplitude(&data, &region.template(&region.median)).0)
        })
        .collect();
    if values.len() < 5 {
        return None;
    }
    let middle = values.len() / 2;
    Some(*values.select_nth_unstable_by(middle, f64::total_cmp).1)
}

// A correction on a regular grid in raw sensor coordinates.
#[derive(Clone)]
struct Field {
    size: [usize; 2],
    origin: [f64; 2],
    spacing: f64,
    // Raw pixels per analysis-preview pixel: the detail the grid values cannot resolve.
    step: f64,
    gain: Vec<f32>,
    support: Vec<f32>,
    // The report spot of the gain this field applies; None when an additive patch of the
    // plan already applies it and the field only refines.
    spot: Option<usize>,
    refine: bool,
    views: usize,
    area: usize,
    center: [f64; 2],
    error: f64,
}

impl Field {
    fn sample(&self, values: &[f32], raw_x: f64, raw_y: f64) -> Option<f64> {
        let fx = (raw_x - self.origin[0]) / self.spacing;
        let fy = (raw_y - self.origin[1]) / self.spacing;
        (fx >= 0.0
            && fy >= 0.0
            && fx <= (self.size[0] - 1) as f64
            && fy <= (self.size[1] - 1) as f64)
            .then(|| bilinear(values, self.size[0], self.size[1], fx, fy))
    }

    fn covers(&self, raw_x: f64, raw_y: f64) -> bool {
        self.sample(&self.support, raw_x, raw_y)
            .is_some_and(|support| support > 0.5)
    }

    // A confirmed patch: same gain, refinable inside its own measured support. Compact
    // patches stay in the plan as additive patches, so their field has no spot of its own.
    fn from_patch(patch: Patch, step: f64, spot: Option<usize>) -> Self {
        let floor = if spot.is_some() {
            0.0
        } else {
            0.08 * patch.values.iter().copied().fold(0.0, f32::max)
        };
        let covered: Vec<f32> = patch
            .values
            .iter()
            .map(|&value| f32::from(u8::from(value > floor)))
            .collect();
        Self {
            size: [patch.side; 2],
            origin: patch.origin,
            spacing: patch.spacing,
            step,
            support: gaussian(&covered, patch.side, patch.side, 0.7, false),
            gain: patch.values,
            spot,
            refine: true,
            views: 0,
            area: 0,
            center: [0.0; 2],
            error: 0.0,
        }
    }

    // Validate one stacked region on the target and express its correction; the error names
    // the safeguard that rejected it.
    fn new(
        region: &Region,
        frames: &[&[i16]],
        target: &Evidence,
        own: &[i16],
        matched: bool,
    ) -> Result<Self, &'static str> {
        let width = target.width;
        let [x0, y0, w, h] = region.window;
        let target_codes = region.crop(own, width);
        let scene = region.surroundings(&target_codes);
        if scene.len() < 16 {
            return Err("surroundings too small to compare scenes");
        }
        let energy = scene.iter().map(|value| value * value).sum::<f64>();
        let core = region.core.iter().filter(|core| **core).count();
        let mut kept = Vec::new();
        for frame in frames {
            let codes = region.crop(frame, width);
            let visible = codes
                .iter()
                .zip(&region.core)
                .filter(|(code, core)| **core && stack_smooth(**code))
                .count();
            if visible * 2 < core || !region.plain(&codes) {
                continue;
            }
            // A reference showing the target's own scene here cannot vote for dirt.
            let other = region.surroundings(&codes);
            let product = scene.iter().zip(&other).map(|(a, b)| a * b).sum::<f64>();
            let scale = (energy * other.iter().map(|value| value * value).sum::<f64>()).sqrt();
            if product > 0.3 * scale.max(1e-12) {
                continue;
            }
            kept.push(codes);
        }
        if kept.len() < STACK_MIN_VIEWS {
            return Err("too few independent views of the shadow");
        }
        let mut median = vec![0.0f32; w * h];
        let mut views = Vec::with_capacity(core);
        let mut strong = false;
        let mut values = [0.0f32; STACK_POOL];
        for i in 0..w * h {
            let mut used = 0;
            for codes in &kept {
                if stack_smooth(codes[i]) {
                    values[used] = stack_value(codes[i]);
                    used += 1;
                }
            }
            if used == 0 {
                continue;
            }
            let (value, spread) = stack_median(&mut values[..used]);
            median[i] = value;
            if region.core[i] {
                views.push(used);
                strong |= used >= STACK_MIN_VIEWS
                    && value < -0.008
                    && value / (spread.max(0.002) / (used as f32).sqrt()) < -6.0;
            }
        }
        if !strong || views.is_empty() {
            return Err("shadow not significant among independent views");
        }
        let middle = views.len() / 2;
        let views = *views.select_nth_unstable(middle).1;
        if views < STACK_MIN_VIEWS {
            return Err("too few views across the shadow");
        }
        let template = region.template(&median);
        let data: Vec<f32> = target_codes.iter().map(|&code| stack_value(code)).collect();
        let (amplitude, score) = matched_amplitude(&data, &template);
        let ring = region.quiet.iter().filter(|quiet| **quiet).count();
        let flat = target_codes
            .iter()
            .zip(&region.quiet)
            .filter(|(code, quiet)| **quiet && stack_smooth(**code))
            .count();
        let peak = template.iter().copied().fold(0.0, f32::min);
        // A faint speck in few views is within reach of chance: small details of one scene
        // shot several times overlap that often.
        if peak > -0.02 && region.area < 30 && views < 6 {
            return Err("too few views of a faint speck");
        }
        let compact = region.area <= 400
            && region.bounds[0].max(region.bounds[1])
                <= 3 * region.bounds[0].min(region.bounds[1]) + 4;
        let smooth = flat * 5 >= ring * 4;
        let expected: f64 = template.iter().map(|shadow| -f64::from(*shadow)).sum();
        let measured = if !region.plain(&target_codes) {
            Err(Unmeasured::Scenery)
        } else if (0.4..=3.0).contains(&amplitude) && score >= 5.0 {
            region.measure(&data, &target_codes, amplitude * expected, smooth)
        } else {
            Err(Unmeasured::Scenery)
        };
        // The target's summed depth over the shadow against the stack's: a least-squares
        // scale alone follows only the sharpest part.
        let weight = || {
            let ring: Vec<usize> = (0..w * h).filter(|&i| region.quiet[i]).collect();
            region.shadow(&data, &ring).map_or(0.0, |(shadow, _)| {
                let summed: f64 = (0..w * h)
                    .filter(|&i| region.near[i])
                    .map(|i| f64::from(shadow[i]))
                    .sum();
                summed / expected.max(1e-9)
            })
        };
        let refine = !matches!(measured, Err(Unmeasured::Rough));
        let (mut gain, near) = match measured {
            Ok(measured) => measured,
            Err(Unmeasured::Uneven) => (
                template
                    .iter()
                    .map(|shadow| -shadow * amplitude as f32)
                    .collect(),
                region.near.clone(),
            ),
            Err(Unmeasured::Rough)
                if matched
                    && score >= 6.0
                    && (0.6..=1.6).contains(&amplitude)
                    && compact
                    && peak <= -0.02
                    && views >= 5
                    && (0.6..=1.6).contains(&weight()) =>
            {
                // Textured surroundings: trust the aperture-matched stack as it is.
                (
                    template.iter().map(|shadow| -shadow).collect(),
                    region.near.clone(),
                )
            }
            Err(_) => return Err("target does not confirm the shadow"),
        };
        let near: Vec<f32> = near.iter().map(|near| f32::from(u8::from(*near))).collect();
        let support = gaussian(&near, w, h, 0.7, false);
        for (value, support) in gain.iter_mut().zip(&support) {
            *value = (*value * support).max(0.0);
        }
        if gain.iter().all(|value| *value <= 0.0) {
            return Err("no measurable shadow on the target");
        }
        let (mut sx, mut sy) = (0.0, 0.0);
        for i in (0..w * h).filter(|&i| region.core[i]) {
            sx += (x0 + i % w) as f64 / core as f64;
            sy += (y0 + i / w) as f64 / core as f64;
        }
        let step = target.step as f64;
        Ok(Self {
            size: [w, h],
            origin: [
                target.geometry.crop[0] as f64 + (x0 as f64 + 0.5) * step - 0.5,
                target.geometry.crop[1] as f64 + (y0 as f64 + 0.5) * step - 0.5,
            ],
            spacing: step,
            step,
            gain,
            support,
            // Set once the plan reports this shadow.
            spot: None,
            refine,
            views,
            area: region.area,
            center: [sx, sy],
            error: region.noise,
        })
    }

    // The target's own shadow at a confirmed spot: the references say only where the dirt
    // is and how much light it takes. `planned` is the log gain already planned at a raw
    // position, so that only what is left of a shadow is measured.
    fn at_spot(
        spot: &ConfirmedSpot,
        target: &Evidence,
        own: &[i16],
        planned: &(impl Fn(f64, f64) -> f64 + Sync),
    ) -> Result<Self, &'static str> {
        let radius = spot.sigma.clamp(1.5, 6.0);
        let margin = radius.ceil() as usize + STACK_MARGIN;
        let reach = (margin + 2) as f64;
        let (cx, cy) = (spot.x.round(), spot.y.round());
        if cx < reach
            || cy < reach
            || cx + reach >= target.width as f64
            || cy + reach >= target.height as f64
        {
            return Err("too close to the border to measure");
        }
        let (x0, y0) = (cx as usize - margin, cy as usize - margin);
        let w = 2 * margin + 1;
        let within = |reach: f64| -> Vec<bool> {
            (0..w * w)
                .map(|i| {
                    ((x0 + i % w) as f64 - spot.x).hypot((y0 + i / w) as f64 - spot.y) <= reach
                })
                .collect()
        };
        let core = within(radius);
        let region = Region {
            window: [x0, y0, w, w],
            near: within(radius + 2.0),
            quiet: within(radius + 5.0).iter().map(|close| !close).collect(),
            median: vec![0.0; w * w],
            area: core.iter().filter(|core| **core).count(),
            bounds: [2 * radius.ceil() as usize + 1; 2],
            peak: 0.0,
            noise: 0.0,
            core,
        };
        let codes = region.crop(own, target.width);
        if !region.plain(&codes) {
            return Err("surroundings of the spot are not plain");
        }
        let step = target.step as f64;
        let raw = |i: usize| {
            (
                target.geometry.crop[0] as f64 + ((x0 + i % w) as f64 + 0.5) * step - 0.5,
                target.geometry.crop[1] as f64 + ((y0 + i / w) as f64 + 0.5) * step - 0.5,
            )
        };
        let data: Vec<f32> = codes
            .iter()
            .enumerate()
            .map(|(i, &code)| {
                let (x, y) = raw(i);
                stack_value(code) + planned(x, y) as f32
            })
            .collect();
        let ring = region.quiet.iter().filter(|quiet| **quiet).count();
        let flat = codes
            .iter()
            .zip(&region.quiet)
            .filter(|(code, quiet)| **quiet && stack_smooth(**code))
            .count();
        let smooth = flat * 5 >= ring * 4;
        let cells: Vec<usize> = (0..w * w)
            .filter(|&i| region.quiet[i] && (!smooth || stack_smooth(codes[i])))
            .collect();
        let (shadow, noise) = region
            .shadow(&data, &cells)
            .ok_or("surroundings of the spot cannot be leveled")?;
        let depth: f64 = (0..w * w)
            .filter(|&i| region.core[i])
            .map(|i| f64::from(shadow[i]))
            .sum();
        if depth < 6.0 * noise * (region.area as f64).sqrt() {
            return Err("no significant shadow at the spot");
        }
        let (mut gain, near) = region
            .measure(&data, &codes, spot.depth.max(0.5), smooth)
            .map_err(|reason| match reason {
                Unmeasured::Rough => "surroundings of the spot are too rough",
                Unmeasured::Uneven => "surroundings of the spot are uneven",
                Unmeasured::Scenery => "no isolated shadow at the spot",
            })?;
        let near: Vec<f32> = near.iter().map(|near| f32::from(u8::from(*near))).collect();
        let support = gaussian(&near, w, w, 0.7, false);
        for (value, support) in gain.iter_mut().zip(&support) {
            *value = (*value * support).max(0.0);
        }
        let total: f64 = gain.iter().map(|value| f64::from(*value)).sum();
        if total <= 0.0 {
            return Err("no measurable shadow on the target");
        }
        let (mut sx, mut sy) = (0.0, 0.0);
        for (i, value) in gain.iter().enumerate() {
            sx += (x0 + i % w) as f64 * f64::from(*value) / total;
            sy += (y0 + i / w) as f64 * f64::from(*value) / total;
        }
        Ok(Self {
            size: [w, w],
            origin: [raw(0).0, raw(0).1],
            spacing: step,
            step,
            area: gain.iter().filter(|value| **value > 0.0).count(),
            gain,
            support,
            spot: None,
            refine: true,
            views: spot.distinct_views,
            center: [sx, sy],
            error: noise,
        })
    }
}

impl Calibration {
    // Stack the nearest distinct reference views with the target's dust rendering and keep
    // the isolated shadows that the target itself confirms.
    fn stack_fields(&self, target: &Evidence, position: Option<usize>) -> Vec<Field> {
        let Some(own) = &target.stack else {
            return Vec::new();
        };
        let width = target.width;
        let position = position
            .or_else(|| {
                self.stacks
                    .iter()
                    .find(|frame| Arc::ptr_eq(&frame.data, own))
                    .map(|frame| frame.index)
            })
            .unwrap_or(0);
        let mut order: Vec<&StackFrame> = self
            .stacks
            .iter()
            .filter(|frame| {
                frame.geometry == target.geometry
                    && frame.width == width
                    && frame.height == target.height
                    && frame.step == target.step
            })
            .collect();
        order.sort_by_key(|frame| (frame.index.abs_diff(position), frame.index));
        let mut pool: Vec<&StackFrame> = Vec::new();
        for frame in order {
            if same_view(&frame.signature, &target.signature)
                || pool
                    .iter()
                    .any(|other| same_view(&other.signature, &frame.signature))
            {
                continue;
            }
            pool.push(frame);
            if pool.len() == STACK_POOL {
                break;
            }
        }
        if pool.len() < STACK_MIN_VIEWS {
            return Vec::new();
        }
        let nearest: Vec<&[i16]> = pool
            .iter()
            .take(STACK_VIEWS)
            .map(|frame| frame.data.as_slice())
            .collect();
        // A long reference list surrounds a short session with frames of other apertures
        // that dilute its shadows, so the closest half is stacked on its own as well.
        let closest = &nearest[..nearest.len().min(STACK_VIEWS / 2)];
        let stacks: Vec<(&[&[i16]], Vec<Region>)> = [nearest.as_slice(), closest]
            .into_iter()
            .take(if closest.len() < nearest.len() { 2 } else { 1 })
            .map(|frames| {
                let regions = StackMap::new(frames, width).regions(width, target.height);
                (frames, regions)
            })
            .collect();
        // The stack that shows more shadows also judges their depth best.
        let guide = stacks
            .iter()
            .map(|(_, regions)| regions)
            .max_by_key(|regions| regions.len())
            .expect("at least one stack");
        let mut fields: Vec<Field> = Vec::new();
        if let Some(reference) = stack_contrast(own, guide, width).filter(|value| *value > 0.2) {
            let contrasts: Vec<Option<f64>> = pool
                .par_iter()
                .map(|frame| stack_contrast(&frame.data, guide, width))
                .collect();
            let similar: Vec<&[i16]> = pool
                .iter()
                .zip(contrasts)
                .filter(|(_, contrast)| {
                    contrast.is_some_and(|value| (0.7..=1.43).contains(&(value / reference)))
                })
                .take(STACK_VIEWS)
                .map(|(frame, _)| frame.data.as_slice())
                .collect();
            if similar.len() >= STACK_MIN_VIEWS {
                fields = StackMap::new(&similar, width)
                    .regions(width, target.height)
                    .par_iter()
                    .filter_map(|region| Field::new(region, &similar, target, own, true).ok())
                    .collect();
            }
        }
        // Shadows seen by too few aperture-matched views may still be measurable directly.
        let step = target.step as f64;
        for (frames, regions) in &stacks {
            let rest: Vec<Field> = regions
                .par_iter()
                .filter(|region| {
                    let [x0, y0, w, _] = region.window;
                    !(0..region.core.len()).any(|i| {
                        let raw_x = target.geometry.crop[0] as f64
                            + ((x0 + i % w) as f64 + 0.5) * step
                            - 0.5;
                        let raw_y = target.geometry.crop[1] as f64
                            + ((y0 + i / w) as f64 + 0.5) * step
                            - 0.5;
                        region.core[i] && fields.iter().any(|field| field.covers(raw_x, raw_y))
                    })
                })
                .filter_map(|region| Field::new(region, frames, target, own, false).ok())
                .collect();
            fields.extend(rest);
        }
        fields
    }
}

struct Mapping {
    transpose: bool,
    flip_x: bool,
    flip_y: bool,
    sw: usize,
    sh: usize,
    scale: [f64; 2],
    crop: [f64; 2],
}

impl Mapping {
    fn new(metadata: &SensorMetadata) -> Self {
        let (transpose, flip_x, flip_y) = Orientation::from_u16(metadata.orientation).to_flips();
        let (sw, sh) = if transpose {
            (metadata.height, metadata.width)
        } else {
            (metadata.width, metadata.height)
        };
        Self {
            transpose,
            flip_x,
            flip_y,
            sw,
            sh,
            scale: [
                metadata.crop[2] as f64 / sw as f64,
                metadata.crop[3] as f64 / sh as f64,
            ],
            crop: [metadata.crop[0] as f64, metadata.crop[1] as f64],
        }
    }

    fn to_output(&self, raw_x: f64, raw_y: f64) -> (f64, f64) {
        let mut x = (raw_x - self.crop[0] + 0.5) / self.scale[0] - 0.5;
        let mut y = (raw_y - self.crop[1] + 0.5) / self.scale[1] - 0.5;
        if self.flip_x {
            x = self.sw as f64 - 1.0 - x;
        }
        if self.flip_y {
            y = self.sh as f64 - 1.0 - y;
        }
        if self.transpose { (y, x) } else { (x, y) }
    }

    fn to_raw(&self, x: usize, y: usize) -> (f64, f64) {
        let (mut sx, mut sy) = if self.transpose { (y, x) } else { (x, y) };
        if self.flip_x {
            sx = self.sw - 1 - sx;
        }
        if self.flip_y {
            sy = self.sh - 1 - sy;
        }
        (
            self.crop[0] + (sx as f64 + 0.5) * self.scale[0] - 0.5,
            self.crop[1] + (sy as f64 + 0.5) * self.scale[1] - 0.5,
        )
    }

    // Output-pixel bounds (inclusive) of a raw-coordinate rectangle.
    fn bounds(&self, origin: [f64; 2], size: [f64; 2]) -> (f64, f64, f64, f64) {
        let a = self.to_output(origin[0], origin[1]);
        let b = self.to_output(origin[0] + size[0], origin[1] + size[1]);
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }
}

type Bounds = (f64, f64, f64, f64);

// Where each planned gain lands in one developed image.
struct Layout<'a> {
    mapping: Mapping,
    patches: Vec<(Bounds, &'a Patch)>,
    fields: Vec<(Bounds, &'a Field)>,
}

impl Layout<'_> {
    fn touches(&self, y: usize) -> bool {
        let inside = |bounds: &Bounds| (bounds.1..=bounds.3).contains(&(y as f64));
        self.patches.iter().any(|(bounds, _)| inside(bounds))
            || self.fields.iter().any(|(bounds, _)| inside(bounds))
    }

    // Adds the planned log gains of output row `y`, columns `left..left + gains.len()`:
    // compact shadows accumulate, then every other shape takes the maximum.
    fn row(&self, y: usize, left: usize, gains: &mut [f64]) {
        let right = left + gains.len();
        let columns = |bounds: &Bounds| {
            let start = (bounds.0.ceil().max(0.0) as usize).max(left);
            let end = (bounds.2.floor().max(0.0) as usize + 1).min(right);
            (bounds.1..=bounds.3)
                .contains(&(y as f64))
                .then_some(start..end)
        };
        for (bounds, patch) in &self.patches {
            for x in columns(bounds).into_iter().flatten() {
                let (raw_x, raw_y) = self.mapping.to_raw(x, y);
                gains[x - left] += bilinear(
                    &patch.values,
                    patch.side,
                    patch.side,
                    (raw_x - patch.origin[0]) / patch.spacing,
                    (raw_y - patch.origin[1]) / patch.spacing,
                );
            }
        }
        for (bounds, field) in &self.fields {
            for x in columns(bounds).into_iter().flatten() {
                let (raw_x, raw_y) = self.mapping.to_raw(x, y);
                if let Some(measured) = field.sample(&field.gain, raw_x, raw_y) {
                    gains[x - left] = gains[x - left].max(measured);
                }
            }
        }
    }
}

impl Plan {
    // Fields listed in `scenery` were disproved on the developed samples and stay out.
    fn layout(&self, metadata: &SensorMetadata, scenery: &[usize]) -> Layout<'_> {
        let mapping = Mapping::new(metadata);
        let patches = self
            .patches
            .iter()
            .map(|patch| {
                let size = (patch.side - 1) as f64 * patch.spacing;
                (mapping.bounds(patch.origin, [size; 2]), patch)
            })
            .collect();
        let fields = self
            .fields
            .iter()
            .enumerate()
            .filter(|(index, field)| field.spot.is_some() && !scenery.contains(index))
            .map(|(_, field)| {
                let size = [
                    (field.size[0] - 1) as f64 * field.spacing,
                    (field.size[1] - 1) as f64 * field.spacing,
                ];
                (mapping.bounds(field.origin, size), field)
            })
            .collect();
        Layout {
            mapping,
            patches,
            fields,
        }
    }
}

struct Crop {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    delta: Vec<f32>,
}

// Full-resolution corrections that only the developed samples can supply, and the planned
// fields they disprove.
pub struct Refinement {
    crops: Vec<Crop>,
    scenery: Vec<usize>,
}

fn bilinear(pixels: &[f32], width: usize, height: usize, x: f64, y: f64) -> f64 {
    let x = x.clamp(0.0, (width - 1) as f64);
    let y = y.clamp(0.0, (height - 1) as f64);
    let (left, top) = (x.floor() as usize, y.floor() as usize);
    let (right, bottom) = ((left + 1).min(width - 1), (top + 1).min(height - 1));
    let (dx, dy) = (x - left as f64, y - top as f64);
    let a = f64::from(pixels[top * width + left]) * (1.0 - dx)
        + f64::from(pixels[top * width + right]) * dx;
    let b = f64::from(pixels[bottom * width + left]) * (1.0 - dx)
        + f64::from(pixels[bottom * width + right]) * dx;
    a * (1.0 - dy) + b * dy
}

// Saved calibrations: everything a later run needs in place of the reference RAWs.
const SAVED_MAGIC: &[u8; 8] = b"DNGMFDL\0";
// Raised whenever analysis or planning changes what the saved numbers mean.
const SAVED_VERSION: u32 = 1;
const SAVED_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const SAVED_DAMAGED: &str = "saved sensor-dirt data is damaged";

// FNV-1a.
fn saved_hash(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

struct Encoder<W> {
    writer: W,
    hash: u64,
}

impl<W: Write> Encoder<W> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.hash = saved_hash(self.hash, bytes);
        self.writer
            .write_all(bytes)
            .context("writing sensor-dirt data")
    }

    fn count(&mut self, value: usize) -> Result<()> {
        self.bytes(&(value as u64).to_le_bytes())
    }

    fn real(&mut self, value: f64) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }

    fn flag(&mut self, value: bool) -> Result<()> {
        self.bytes(&[u8::from(value)])
    }

    fn text(&mut self, value: &str) -> Result<()> {
        self.count(value.len())?;
        self.bytes(value.as_bytes())
    }

    // Seals everything written since the previous seal.
    fn seal(&mut self) -> Result<()> {
        let hash = std::mem::replace(&mut self.hash, SAVED_SEED);
        self.writer
            .write_all(&hash.to_le_bytes())
            .context("writing sensor-dirt data")
    }

    fn geometry(&mut self, geometry: &Geometry) -> Result<()> {
        self.text(&geometry.make)?;
        self.text(&geometry.model)?;
        geometry
            .raw
            .into_iter()
            .chain(geometry.crop)
            .try_for_each(|value| self.count(value))
    }
}

struct Decoder<R> {
    reader: R,
    hash: u64,
}

impl<R: Read> Decoder<R> {
    fn bytes(&mut self, buffer: &mut [u8]) -> Result<()> {
        self.reader
            .read_exact(buffer)
            .context("saved sensor-dirt data ends early or cannot be read")?;
        self.hash = saved_hash(self.hash, buffer);
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        self.bytes(&mut bytes)?;
        Ok(bytes)
    }

    fn count(&mut self, limit: usize) -> Result<usize> {
        let value = u64::from_le_bytes(self.array()?);
        ensure!(value <= limit as u64, SAVED_DAMAGED);
        Ok(value as usize)
    }

    fn real(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array()?))
    }

    fn flag(&mut self) -> Result<bool> {
        match self.array::<1>()?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => bail!(SAVED_DAMAGED),
        }
    }

    fn text(&mut self) -> Result<String> {
        let mut bytes = vec![0; self.count(1 << 16)?];
        self.bytes(&mut bytes)?;
        String::from_utf8(bytes).ok().context(SAVED_DAMAGED)
    }

    fn seal(&mut self) -> Result<()> {
        let hash = std::mem::replace(&mut self.hash, SAVED_SEED);
        let mut stored = [0; 8];
        self.reader
            .read_exact(&mut stored)
            .context("saved sensor-dirt data ends early or cannot be read")?;
        ensure!(u64::from_le_bytes(stored) == hash, SAVED_DAMAGED);
        Ok(())
    }

    fn geometry(&mut self) -> Result<Geometry> {
        Ok(Geometry {
            make: self.text()?,
            model: self.text()?,
            raw: [self.count(1 << 20)?, self.count(1 << 20)?],
            crop: [
                self.count(1 << 20)?,
                self.count(1 << 20)?,
                self.count(1 << 20)?,
                self.count(1 << 20)?,
            ],
        })
    }
}

impl Calibration {
    // Writes the calibration and the paths of its references, which must be in the order
    // they were analyzed in. Extra detection needs the residual of every reference.
    pub fn save(&self, writer: impl Write, extra: bool, references: &[&Path]) -> Result<()> {
        ensure!(
            references.len() == self.reference_images
                && self.stacks.len() == if extra { references.len() } else { 0 },
            "saving sensor-dirt data needs every analyzed reference"
        );
        let mut encoder = Encoder {
            writer,
            hash: SAVED_SEED,
        };
        encoder.bytes(SAVED_MAGIC)?;
        encoder.bytes(&SAVED_VERSION.to_le_bytes())?;
        encoder.flag(extra)?;
        encoder.count(references.len())?;
        for path in references {
            encoder.text(path.to_str().with_context(|| {
                format!(
                    "reference path {} is not UTF-8 and cannot be saved",
                    path.display()
                )
            })?)?;
        }
        encoder.count(self.groups.len())?;
        for group in &self.groups {
            encoder.geometry(&group.geometry)?;
            encoder.count(group.images)?;
            encoder.count(group.distinct_views)?;
            encoder.count(group.spots.len())?;
            for spot in &group.spots {
                for value in [spot.x, spot.y, spot.sigma, spot.depth] {
                    encoder.real(value)?;
                }
                encoder.flag(spot.irregular)?;
                encoder.count(spot.supporting_images)?;
                encoder.count(spot.distinct_views)?;
                encoder.count(spot.support.len())?;
                for patch in &spot.support {
                    for value in [patch.origin[0], patch.origin[1], patch.spacing] {
                        encoder.real(value)?;
                    }
                    encoder.count(patch.side)?;
                    encoder.flag(patch.irregular)?;
                    ensure!(
                        patch.values.len() == patch.side * patch.side,
                        "sensor-dirt shape of unexpected size"
                    );
                    let bytes: Vec<u8> = patch
                        .values
                        .iter()
                        .flat_map(|value| value.to_le_bytes())
                        .collect();
                    encoder.bytes(&bytes)?;
                }
            }
        }
        encoder.seal()?;
        for frame in &self.stacks {
            ensure!(
                frame.data.len() == frame.width * frame.height,
                "sensor-dirt residual of unexpected size"
            );
            encoder.count(frame.index)?;
            encoder.geometry(&frame.geometry)?;
            for value in [frame.width, frame.height, frame.step] {
                encoder.count(value)?;
            }
            encoder.count(frame.signature.len())?;
            for value in &frame.signature {
                encoder.real(*value)?;
            }
            encoder.seal()?;
            let bytes: Vec<u8> = frame
                .data
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            encoder.bytes(&bytes)?;
            encoder.seal()?;
        }
        encoder.writer.flush().context("writing sensor-dirt data")
    }

    // Drops the residuals of the references that `keep` does not select by index.
    pub fn keep_stacks(&mut self, keep: impl Fn(usize) -> bool) {
        self.stacks.retain(|frame| keep(frame.index));
    }
}

// Saved sensor-dirt data whose residuals are still to be read.
pub struct Saved<R> {
    pub extra: bool,
    // The references in the order they were analyzed in.
    pub references: Vec<PathBuf>,
    calibration: Calibration,
    decoder: Decoder<R>,
}

impl<R: Read + Seek> Saved<R> {
    pub fn open(reader: R) -> Result<Self> {
        let mut decoder = Decoder {
            reader,
            hash: SAVED_SEED,
        };
        ensure!(
            &decoder.array::<8>()? == SAVED_MAGIC,
            "not sensor-dirt data saved by --fix-dirty-save"
        );
        let version = u32::from_le_bytes(decoder.array()?);
        ensure!(
            version == SAVED_VERSION,
            "sensor-dirt data was saved in format {version} but this version reads format {SAVED_VERSION}; save it again"
        );
        let extra = decoder.flag()?;
        let references = (0..decoder.count(1 << 24)?)
            .map(|_| decoder.text().map(PathBuf::from))
            .collect::<Result<Vec<_>>>()?;
        let groups = (0..decoder.count(1 << 16)?)
            .map(|_| {
                let geometry = decoder.geometry()?;
                let images = decoder.count(1 << 24)?;
                let distinct_views = decoder.count(1 << 24)?;
                let spots = (0..decoder.count(1 << 24)?)
                    .map(|_| {
                        let [x, y, sigma, depth] = [
                            decoder.real()?,
                            decoder.real()?,
                            decoder.real()?,
                            decoder.real()?,
                        ];
                        let irregular = decoder.flag()?;
                        let supporting_images = decoder.count(1 << 24)?;
                        let distinct_views = decoder.count(1 << 24)?;
                        let support = (0..decoder.count(1 << 24)?)
                            .map(|_| {
                                let origin = [decoder.real()?, decoder.real()?];
                                let spacing = decoder.real()?;
                                let side = decoder.count(1 << 12)?;
                                let irregular = decoder.flag()?;
                                let mut bytes = vec![0; 4 * side * side];
                                decoder.bytes(&mut bytes)?;
                                Ok(Patch {
                                    origin,
                                    spacing,
                                    side,
                                    irregular,
                                    values: bytes
                                        .chunks_exact(4)
                                        .map(|value| {
                                            f32::from_le_bytes([
                                                value[0], value[1], value[2], value[3],
                                            ])
                                        })
                                        .collect(),
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(ConfirmedSpot {
                            x,
                            y,
                            sigma,
                            irregular,
                            supporting_images,
                            distinct_views,
                            support,
                            depth,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Group {
                    geometry,
                    images,
                    distinct_views,
                    spots,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        decoder.seal()?;
        Ok(Self {
            extra,
            calibration: Calibration {
                reference_images: references.len(),
                groups,
                stacks: Vec::new(),
            },
            references,
            decoder,
        })
    }

    // Reads the residuals of the references that `keep` selects by index and skips the
    // others, which only voted.
    pub fn calibration(mut self, keep: impl Fn(usize) -> bool) -> Result<Calibration> {
        let frames = if self.extra { self.references.len() } else { 0 };
        for _ in 0..frames {
            let index = self.decoder.count(frames - 1)?;
            let geometry = self.decoder.geometry()?;
            let [width, height, step] = [
                self.decoder.count(1 << 16)?,
                self.decoder.count(1 << 16)?,
                self.decoder.count(1 << 16)?,
            ];
            let signature = (0..self.decoder.count(1 << 16)?)
                .map(|_| self.decoder.real())
                .collect::<Result<Vec<_>>>()?;
            self.decoder.seal()?;
            let length = 2 * width * height;
            if keep(index) {
                let mut bytes = vec![0; length];
                self.decoder.bytes(&mut bytes)?;
                self.decoder.seal()?;
                self.calibration.stacks.push(StackFrame {
                    index,
                    geometry,
                    width,
                    height,
                    step,
                    signature,
                    data: Arc::new(
                        bytes
                            .chunks_exact(2)
                            .map(|value| i16::from_le_bytes([value[0], value[1]]))
                            .collect(),
                    ),
                });
            } else {
                self.decoder
                    .reader
                    .seek(SeekFrom::Current(length as i64 + 8))
                    .context("saved sensor-dirt data cannot be read")?;
            }
        }
        Ok(self.calibration)
    }
}
