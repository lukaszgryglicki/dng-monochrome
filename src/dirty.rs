use crate::raw::{MonoImage, SensorMetadata};
use anyhow::{Result, ensure};
use rawler::decoders::Orientation;
use rayon::prelude::*;
use serde::Serialize;
use std::sync::OnceLock;

const PATCH_SIDE: usize = 33;
const MAX_LOG_GAIN: f64 = std::f64::consts::LN_2 * 2.0;
const MAX_FIT_ERROR: f64 = 0.025;

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
}

pub fn analyze(image: &MonoImage) -> Result<Evidence> {
    analyze_with_extra(image, false)
}

pub fn analyze_with_extra(image: &MonoImage, extra: bool) -> Result<Evidence> {
    let preview = Preview::new(image)?;
    Ok(Evidence {
        geometry: Geometry::new(&image.metadata),
        width: preview.width,
        height: preview.height,
        step: preview.step,
        signature: preview.signature(),
        candidates: preview.candidates(extra),
    })
}

struct ConfirmedSpot {
    x: f64,
    y: f64,
    sigma: f64,
    irregular: bool,
    supporting_images: usize,
    distinct_views: usize,
    support: Vec<Patch>,
}

struct Group {
    geometry: Geometry,
    images: usize,
    distinct_views: usize,
    spots: Vec<ConfirmedSpot>,
}

pub struct Calibration {
    reference_images: usize,
    groups: Vec<Group>,
}

fn match_radius(a: f64, b: f64, irregular: bool) -> f64 {
    // Extra radii enclose entire shapes rather than their compact cores.
    let fraction = if irregular { 0.25 } else { 0.375 };
    (fraction * (a + b)).max(0.75)
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
                group.spots.push(ConfirmedSpot {
                    x: cluster.iter().map(|(_, c)| c.x).sum::<f64>() / cluster.len() as f64,
                    y: cluster.iter().map(|(_, c)| c.y).sum::<f64>() / cluster.len() as f64,
                    sigma: center.sigma,
                    irregular: center.patch.irregular,
                    supporting_images: cluster.len(),
                    distinct_views: distinct.len(),
                    support,
                });
            }
        }
        Ok(Self {
            reference_images: frames.len(),
            groups,
        })
    }

    pub fn plan(&self, target: &Evidence) -> Plan {
        let group = self
            .groups
            .iter()
            .find(|group| group.geometry == target.geometry);
        let mut patches = Vec::new();
        let mut spots = Vec::new();
        if let Some(group) = group {
            for candidate in &target.candidates {
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
                {
                    let mut patch = candidate.patch.clone();
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
                    patch.irregular = confirmed.irregular;
                    if patch.irregular {
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
                }
            }
        }
        // Compact contributions accumulate before extra fields take their maximum.
        patches.sort_by_key(|patch| patch.irregular);
        let warning = if group.is_none() {
            Some("no matching reference sensor geometry/crop; pixels are unchanged")
        } else if group.is_some_and(|group| group.distinct_views < 3) {
            Some(
                "at least three distinct reference views with matching geometry are needed; pixels are unchanged",
            )
        } else if patches.is_empty() {
            Some("no confidently recurring, locally measurable dirt shadows; pixels are unchanged")
        } else {
            None
        };
        Plan {
            geometry: target.geometry.clone(),
            patches,
            report: Report {
                method: "multi_image_local_flat_field",
                strength: 1.0,
                reference_images: self.reference_images,
                compatible_reference_images: group.map_or(0, |group| group.images),
                distinct_reference_views: group.map_or(0, |group| group.distinct_views),
                candidate_spots: target.candidates.len(),
                confirmed_sensor_spots: group.map_or(0, |group| group.spots.len()),
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
    pub spots: Vec<Spot>,
    pub changed_samples: usize,
    pub clipped_samples: usize,
    pub maximum_gain: f64,
    pub warning: Option<&'static str>,
}

pub struct Plan {
    geometry: Geometry,
    patches: Vec<Patch>,
    pub report: Report,
}

impl Plan {
    pub fn apply(&self, image: &mut MonoImage) -> Result<Report> {
        self.apply_with_strength(image, 1.0)
    }

    pub fn apply_with_strength(&self, image: &mut MonoImage, strength: f64) -> Result<Report> {
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
        if strength != 1.0 {
            for spot in &mut report.spots {
                spot.maximum_gain = spot.maximum_gain.powf(strength).min(MAX_LOG_GAIN.exp());
            }
        }
        if strength == 0.0 {
            report.warning = Some("zero sensor-dirt strength; pixels are unchanged");
            return Ok(report);
        }
        if self.patches.is_empty() {
            return Ok(report);
        }
        let (transpose, flip_x, flip_y) = Orientation::from_u16(metadata.orientation).to_flips();
        let (sw, sh) = if transpose {
            (metadata.height, metadata.width)
        } else {
            (metadata.width, metadata.height)
        };
        let scale_x = metadata.crop[2] as f64 / sw as f64;
        let scale_y = metadata.crop[3] as f64 / sh as f64;
        let to_output = |raw_x: f64, raw_y: f64| {
            let mut x = (raw_x - metadata.crop[0] as f64 + 0.5) / scale_x - 0.5;
            let mut y = (raw_y - metadata.crop[1] as f64 + 0.5) / scale_y - 0.5;
            if flip_x {
                x = sw as f64 - 1.0 - x;
            }
            if flip_y {
                y = sh as f64 - 1.0 - y;
            }
            if transpose { (y, x) } else { (x, y) }
        };
        let areas: Vec<_> = self
            .patches
            .iter()
            .map(|patch| {
                let size = (patch.side - 1) as f64 * patch.spacing;
                let a = to_output(patch.origin[0], patch.origin[1]);
                let b = to_output(patch.origin[0] + size, patch.origin[1] + size);
                (
                    a.0.min(b.0),
                    a.1.min(b.1),
                    a.0.max(b.0),
                    a.1.max(b.1),
                    patch,
                )
            })
            .collect();
        let black = metadata.black_level;
        let white = f64::from(metadata.white_level);
        let stats = image
            .pixels
            .par_chunks_mut(metadata.width)
            .enumerate()
            .map(|(y, row)| {
                if !areas
                    .iter()
                    .any(|area| (area.1..=area.3).contains(&(y as f64)))
                {
                    return (0usize, 0usize, 1.0f64);
                }
                let mut gains = vec![0.0f64; row.len()];
                for &(left, top, right, bottom, patch) in &areas {
                    if !(top..=bottom).contains(&(y as f64)) {
                        continue;
                    }
                    let start = left.ceil().max(0.0) as usize;
                    let end = (right.floor().max(0.0) as usize + 1).min(row.len());
                    for (x, gain) in gains.iter_mut().enumerate().take(end).skip(start) {
                        let (mut sx, mut sy) = if transpose { (y, x) } else { (x, y) };
                        if flip_x {
                            sx = sw - 1 - sx;
                        }
                        if flip_y {
                            sy = sh - 1 - sy;
                        }
                        let raw_x = metadata.crop[0] as f64 + (sx as f64 + 0.5) * scale_x - 0.5;
                        let raw_y = metadata.crop[1] as f64 + (sy as f64 + 0.5) * scale_y - 0.5;
                        let measured = bilinear(
                            &patch.values,
                            patch.side,
                            patch.side,
                            (raw_x - patch.origin[0]) / patch.spacing,
                            (raw_y - patch.origin[1]) / patch.spacing,
                        );
                        // Extra estimates may include compact shadows already covered above.
                        if patch.irregular {
                            *gain = gain.max(measured);
                        } else {
                            *gain += measured;
                        }
                    }
                }
                let (mut changed, mut clipped, mut maximum) = (0usize, 0usize, 1.0f64);
                for (pixel, log_gain) in row.iter_mut().zip(gains) {
                    let value = f64::from(*pixel);
                    if log_gain <= 0.0 || value <= black || value >= white {
                        continue;
                    }
                    let gain = (log_gain * strength).min(MAX_LOG_GAIN).exp();
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
