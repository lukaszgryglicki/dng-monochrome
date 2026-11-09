use crate::{
    formats::quantize,
    raw::{MonoImage, SensorMetadata, apply_exif_metadata},
};
use anyhow::{Context, Result, ensure};
use rawler::{
    RawImageData,
    decoders::{Orientation, RawDecodeParams, WellKnownIFD},
    formats::tiff::Value,
    imgop::{Dim2, Point, Rect},
    rawimage::RawPhotometricInterpretation,
    rawsource::RawSource,
    tags::{DngTag, ExifTag},
};
use rayon::prelude::*;
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Channel {
    R,
    G1,
    G2,
    B,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::R => "R",
            Self::G1 => "G1",
            Self::G2 => "G2",
            Self::B => "B",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::R => 0,
            Self::G1 => 1,
            Self::G2 => 2,
            Self::B => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Green {
    #[default]
    G1,
    G2,
    #[serde(rename = "ga")]
    Average,
}

impl Green {
    pub fn channels(self, separate: bool) -> &'static [Channel] {
        if separate || self == Self::Average {
            &[Channel::R, Channel::G1, Channel::G2, Channel::B]
        } else if self == Self::G2 {
            &[Channel::R, Channel::G2, Channel::B]
        } else {
            &[Channel::R, Channel::G1, Channel::B]
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ColorRatios {
    pub reference_means: [f64; 3],
    pub scale_factors: [f64; 3],
}

impl ColorRatios {
    fn from_raw_means(means: [f64; 4]) -> Option<Self> {
        let reference_means = [means[0], (means[1] + means[2]) * 0.5, means[3]];
        let maximum = reference_means.into_iter().fold(0.0, f64::max);
        (maximum > 0.0).then(|| Self {
            reference_means,
            scale_factors: reference_means.map(|mean| mean / maximum),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct WhiteBalance {
    pub coefficients: [f64; 3],
    pub scale_factors: [f64; 3],
}

impl WhiteBalance {
    fn from_coefficients(coefficients: [f64; 3]) -> Result<Self> {
        ensure!(
            coefficients
                .iter()
                .all(|gain| gain.is_finite() && *gain > 0.0),
            "recorded RAW white balance is missing or invalid; --white-balance requires finite positive R/G/B gains"
        );
        let maximum = coefficients.into_iter().fold(0.0, f64::max);
        Ok(Self {
            coefficients,
            scale_factors: coefficients.map(|gain| gain / maximum),
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct NaturalColor {
    pub method: &'static str,
    pub eligible_pixels: usize,
    pub reference_levels: [f64; 3],
    pub scale_factors: [f64; 3],
}

impl NaturalColor {
    pub fn raw_neutral_mask(
        channels: [&MonoImage; 3],
        second_green: Option<&MonoImage>,
        white_balance: WhiteBalance,
    ) -> Result<Vec<bool>> {
        WhiteBalance::from_coefficients(white_balance.coefficients)?;
        let count = channels[0].pixels.len();
        ensure!(
            count > 0
                && channels.iter().copied().chain(second_green).all(|image| {
                    image.pixels.len() == count
                        && image.metadata.black_level.is_finite()
                        && image.metadata.black_level >= 0.0
                        && image.metadata.black_level < f64::from(image.metadata.white_level)
                }),
            "natural color reference channels require equal nonempty dimensions and valid calibration"
        );
        let signal = |image: &MonoImage, index: usize| {
            let value = f64::from(image.pixels[index]);
            let black = image.metadata.black_level;
            let white = f64::from(image.metadata.white_level);
            (value > black && value < white).then(|| (value - black) / (white - black))
        };
        Ok((0..count)
            .into_par_iter()
            .map(|index| {
                let [Some(red), Some(mut green), Some(blue)] =
                    channels.map(|image| signal(image, index))
                else {
                    return false;
                };
                if let Some(other) = second_green {
                    let Some(other) = signal(other, index) else {
                        return false;
                    };
                    green = (green + other) * 0.5;
                }
                let mut reference = [red, green, blue];
                for (value, gain) in reference.iter_mut().zip(white_balance.coefficients) {
                    *value *= gain;
                }
                let minimum = reference.into_iter().fold(f64::INFINITY, f64::min);
                let maximum = reference.into_iter().fold(0.0, f64::max);
                minimum >= maximum * 0.9
            })
            .collect())
    }

    pub fn from_rgb(pixels: &[u16]) -> Result<Option<Self>> {
        Self::from_rgb_with_neutral_mask(pixels, None)
    }

    pub fn from_rgb_with_neutral_mask(
        pixels: &[u16],
        neutral_mask: Option<&[bool]>,
    ) -> Result<Option<Self>> {
        ensure!(
            !pixels.is_empty()
                && pixels.len().is_multiple_of(3)
                && neutral_mask.is_none_or(|mask| mask.len() == pixels.len() / 3),
            "natural color requires nonempty RGB16 samples and a matching reference mask"
        );
        if let Some(mask) = neutral_mask {
            let references: Vec<_> = pixels
                .as_chunks::<3>()
                .0
                .par_iter()
                .zip(mask.par_iter())
                .filter_map(|(pixel, &neutral)| {
                    if !neutral || pixel.contains(&0) || pixel.contains(&u16::MAX) {
                        return None;
                    }
                    let total = f64::from(pixel.map(u32::from).into_iter().sum::<u32>());
                    Some(pixel.map(|value| f64::from(value) / total))
                })
                .collect();
            let count = pixels.len() / 3;
            let eligible_pixels = references.len();
            if eligible_pixels >= count.div_ceil(1000).max(64).min(count) {
                let mut reference_levels = [0.0; 3];
                reference_levels
                    .par_iter_mut()
                    .enumerate()
                    .for_each(|(component, level)| {
                        let mut values: Vec<_> =
                            references.iter().map(|rgb| rgb[component]).collect();
                        let (lower, median, _) =
                            values.select_nth_unstable_by(eligible_pixels / 2, f64::total_cmp);
                        *level = if eligible_pixels.is_multiple_of(2) {
                            (lower.iter().copied().fold(0.0, f64::max) + *median) * 0.5
                        } else {
                            *median
                        };
                    });
                let minimum = reference_levels.into_iter().fold(f64::INFINITY, f64::min);
                return Ok(Some(Self {
                    method: "raw_neutral_chroma_median",
                    eligible_pixels,
                    reference_levels,
                    scale_factors: reference_levels.map(|value| minimum / value),
                }));
            }
        }
        // Fixed chunks keep the estimate identical across thread counts.
        let partials: Vec<_> = pixels
            .par_chunks(3 * 16_384)
            .map(|chunk| {
                let mut count = 0usize;
                let mut sums = [0.0; 3];
                for pixel in chunk.as_chunks::<3>().0 {
                    if pixel.contains(&0) || pixel.contains(&u16::MAX) {
                        continue;
                    }
                    count += 1;
                    for (sum, &value) in sums.iter_mut().zip(pixel) {
                        *sum += (f64::from(value) / 65535.0).powi(6);
                    }
                }
                (count, sums)
            })
            .collect();
        let (eligible_pixels, sums) = partials.into_iter().fold(
            (0usize, [0.0; 3]),
            |(count, mut sums), (partial_count, partial_sums)| {
                for (sum, value) in sums.iter_mut().zip(partial_sums) {
                    *sum += value;
                }
                (count + partial_count, sums)
            },
        );
        if eligible_pixels == 0 {
            return Ok(None);
        }
        let reference_levels = sums.map(|sum| (sum / eligible_pixels as f64).powf(1.0 / 6.0));
        let minimum = reference_levels.into_iter().fold(f64::INFINITY, f64::min);
        Ok(Some(Self {
            method: "shades_of_gray_6",
            eligible_pixels,
            reference_levels,
            scale_factors: reference_levels.map(|value| minimum / value),
        }))
    }
}

fn validate_scales(scales: &[f64; 3]) -> Result<()> {
    ensure!(
        scales
            .iter()
            .all(|factor| factor.is_finite() && (0.0..=1.0).contains(factor)),
        "color scale factors must be finite and within 0..1"
    );
    Ok(())
}

pub fn combine(
    channels: [(&[u16], u8); 3],
    second_green: Option<(&[u16], u8)>,
) -> Result<(Vec<u16>, u8)> {
    combine_with_ratios(channels, second_green, None)
}

pub fn combine_with_ratios(
    channels: [(&[u16], u8); 3],
    second_green: Option<(&[u16], u8)>,
    ratios: Option<&ColorRatios>,
) -> Result<(Vec<u16>, u8)> {
    combine_with_scales(
        channels,
        second_green,
        ratios.map(|ratios| &ratios.scale_factors),
    )
}

pub fn combine_with_scales(
    channels: [(&[u16], u8); 3],
    second_green: Option<(&[u16], u8)>,
    scales: Option<&[f64; 3]>,
) -> Result<(Vec<u16>, u8)> {
    if let Some(scales) = scales {
        validate_scales(scales)?;
    }
    let count = channels[0].0.len();
    ensure!(
        count > 0
            && channels
                .iter()
                .chain(second_green.iter())
                .all(|(samples, bits)| samples.len() == count && (1..=16).contains(bits)),
        "RGB channels must have equal nonempty dimensions and 1-16-bit precision"
    );
    let bits = channels
        .iter()
        .chain(second_green.iter())
        .map(|(_, bits)| *bits)
        .max()
        .unwrap();
    let green_bits = channels[1].1.max(second_green.map_or(0, |(_, bits)| bits));
    let mut pixels = vec![0; count.checked_mul(3).context("RGB sample count overflow")?];
    pixels
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(index, pixel)| {
            for component in [0, 2] {
                let (samples, depth) = channels[component];
                pixel[component] = quantize(samples[index], depth) << (bits - depth);
            }
            let (samples, depth) = channels[1];
            let mut green = u32::from(quantize(samples[index], depth)) << (green_bits - depth);
            if let Some((other, depth)) = second_green {
                let other = u32::from(quantize(other[index], depth)) << (green_bits - depth);
                green = (green + other).div_ceil(2);
            }
            pixel[1] = (green << (bits - green_bits)) as u16;
            if let Some(scales) = scales {
                for (value, factor) in pixel.iter_mut().zip(scales) {
                    *value = (f64::from(*value) * factor).round() as u16;
                }
            }
        });
    Ok((pixels, bits))
}

pub(crate) fn combine_jpeg(
    channels: [&[u8]; 3],
    second_green: Option<&[u8]>,
    scales: Option<&[f64; 3]>,
) -> Result<Vec<u8>> {
    if let Some(scales) = scales {
        validate_scales(scales)?;
    }
    let count = channels[0].len();
    ensure!(
        count > 0
            && channels
                .iter()
                .copied()
                .chain(second_green)
                .all(|samples| samples.len() == count),
        "RGB JPEG channels must have equal nonempty dimensions"
    );
    let mut pixels = vec![0; count.checked_mul(3).context("RGB sample count overflow")?];
    pixels
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(index, pixel)| {
            pixel[0] = channels[0][index];
            pixel[1] = second_green.map_or(channels[1][index], |other| {
                (u16::from(channels[1][index]) + u16::from(other[index])).div_ceil(2) as u8
            });
            pixel[2] = channels[2][index];
            if let Some(scales) = scales {
                for (value, factor) in pixel.iter_mut().zip(scales) {
                    *value = (f64::from(*value) * factor).round() as u8;
                }
            }
        });
    Ok(pixels)
}

#[derive(Debug)]
pub struct BayerImage {
    pub cfa: String,
    pub cfa_assumed: bool,
    pub color_ratios: Option<ColorRatios>,
    pub white_balance: Option<WhiteBalance>,
    pub recorded_white_balance: Option<WhiteBalance>,
    pub channels: Vec<(Channel, MonoImage)>,
}

pub fn decode(path: &Path, no_crop: bool, green: Green, separate: bool) -> Result<BayerImage> {
    decode_with_ratios(path, no_crop, green, separate, false)
}

pub fn decode_with_ratios(
    path: &Path,
    no_crop: bool,
    green: Green,
    separate: bool,
    keep_color_ratios: bool,
) -> Result<BayerImage> {
    decode_with_balance(path, no_crop, green, separate, keep_color_ratios, false)
}

pub fn decode_with_balance(
    path: &Path,
    no_crop: bool,
    green: Green,
    separate: bool,
    keep_color_ratios: bool,
    white_balance: bool,
) -> Result<BayerImage> {
    ensure!(
        !(keep_color_ratios && white_balance),
        "--keep-color-ratios and --white-balance are separate color modes"
    );
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decode_inner(
            path,
            no_crop,
            green,
            separate,
            keep_color_ratios,
            white_balance,
        )
    }))
    .map_err(|_| anyhow::anyhow!("color raw decoder panicked on malformed data or metadata"))?
}

fn decode_inner(
    path: &Path,
    no_crop: bool,
    green: Green,
    separate: bool,
    keep_color_ratios: bool,
    white_balance: bool,
) -> Result<BayerImage> {
    let source = RawSource::new(path).context("opening color raw data")?;
    let params = RawDecodeParams::default();
    let raw = rawler::decode(&source, &params)
        .with_context(|| format!("decoding Bayer samples from {}", path.display()))?;
    ensure!(
        raw.cpp == 1,
        "--color requires one raw sample per position, not developed RGB"
    );
    let pattern = if let RawPhotometricInterpretation::Cfa(config) = &raw.photometric {
        ensure!(
            config.cfa.width == 2 && config.cfa.height == 2,
            "--color supports 2x2 RGB Bayer layouts, not other declared CFA layouts"
        );
        Some(&config.cfa)
    } else {
        None
    };
    let RawImageData::Integer(data) = raw.data else {
        anyhow::bail!("--color requires integer raw samples; floating-point raw is not supported");
    };
    ensure!(
        raw.width > 0 && raw.height > 0 && raw.width.checked_mul(raw.height) == Some(data.len()),
        "invalid decoded Bayer dimensions or sample count"
    );
    let decoder = rawler::get_decoder(&source)?;
    let source_metadata = decoder.raw_metadata(&source, &params)?;
    let raw_ifd = decoder.ifd(WellKnownIFD::Raw)?;
    let exif_ifd = decoder.ifd(WellKnownIFD::Exif)?;
    let standard_iso = exif_ifd
        .as_ref()
        .and_then(|ifd| ifd.get_entry(ExifTag::StandardOutputSensitivity))
        .map(|entry| entry.value.get_u32(0))
        .transpose()
        .map_err(|_| anyhow::anyhow!("StandardOutputSensitivity must be an unsigned integer"))?
        .flatten();
    let mut plane_colors = [0u8, 1, 2];
    if pattern.is_some()
        && let Some(entry) = raw_ifd
            .as_ref()
            .and_then(|ifd| ifd.get_entry(DngTag::CFAPlaneColor))
    {
        let Value::Byte(colors) = &entry.value else {
            anyhow::bail!("CFAPlaneColor must contain RGB BYTE values");
        };
        ensure!(
            colors.len() == 3,
            "only three-color RGB Bayer planes are supported"
        );
        plane_colors.copy_from_slice(colors);
        let mut sorted = plane_colors;
        sorted.sort_unstable();
        ensure!(sorted == [0, 1, 2], "only RGB CFAPlaneColor is supported");
    }
    let mut profiles = [None; 3];
    if let Some(entry) = raw_ifd
        .as_ref()
        .and_then(|ifd| ifd.get_entry(DngTag::NoiseProfile))
    {
        let Value::Double(values) = &entry.value else {
            anyhow::bail!("Bayer NoiseProfile must contain DOUBLE coefficients");
        };
        ensure!(
            matches!(values.len(), 2 | 6)
                && values
                    .iter()
                    .all(|value| value.is_finite() && *value >= 0.0),
            "Bayer NoiseProfile must contain two or six finite, nonnegative coefficients"
        );
        for (plane, profile) in profiles.iter_mut().enumerate() {
            let offset = if values.len() == 2 { 0 } else { 2 * plane };
            let coefficients = [values[offset], values[offset + 1]];
            ensure!(
                coefficients.iter().any(|value| *value > 0.0),
                "NoiseProfile cannot have zero total noise"
            );
            *profile = Some(coefficients);
        }
    }
    let area = if no_crop {
        None
    } else {
        raw.crop_area.or(raw.active_area)
    }
    .unwrap_or_else(|| Rect::new(Point::zero(), Dim2::new(raw.width, raw.height)));
    let (cx, cy, cw, ch) = (area.p.x, area.p.y, area.d.w, area.d.h);
    ensure!(
        cw >= 2
            && ch >= 2
            && cx.checked_add(cw).is_some_and(|end| end <= raw.width)
            && cy.checked_add(ch).is_some_and(|end| end <= raw.height),
        "Bayer crop must contain a complete 2x2 cell and lie within the raw image"
    );
    let mut offsets = [(0usize, 0usize); 4];
    let mut planes = [0usize; 4];
    let mut counts = [0; 3];
    let mut cfa = String::new();
    for y in 0..2 {
        for x in 0..2 {
            let plane = pattern.map_or_else(
                || [0, 1, 1, 2][((cy + y) % 2) * 2 + (cx + x) % 2],
                |pattern| pattern.color_at(cy + y, cx + x),
            );
            let color = usize::from(*plane_colors.get(plane).context("non-RGB Bayer CFA value")?);
            let slot = match (color, counts[color]) {
                (0, 0) => 0,
                (1, 0) => 1,
                (1, 1) => 2,
                (2, 0) => 3,
                _ => anyhow::bail!("Bayer cell must contain one R, two G and one B sample"),
            };
            counts[color] += 1;
            offsets[slot] = (x, y);
            planes[slot] = plane;
            cfa.push(['R', 'G', 'B'][color]);
        }
    }
    ensure!(counts == [1, 2, 1], "Bayer cell must contain R/G/G/B");
    let recorded_balance = WhiteBalance::from_coefficients(
        [planes[0], planes[1], planes[3]].map(|plane| f64::from(raw.wb_coeffs[plane])),
    );
    let recorded_white_balance = recorded_balance.as_ref().ok().copied();
    let white_balance = if white_balance {
        Some(recorded_balance?)
    } else {
        None
    };
    let black = &raw.blacklevel;
    ensure!(
        black.cpp == 1
            && matches!(black.width, 1 | 2)
            && matches!(black.height, 1 | 2)
            && black.levels.len() == black.width * black.height,
        "Bayer black levels must be scalar or repeat within a 2x2 cell"
    );
    let black_values = black.as_vec();
    let black_origin = if raw.camera.mode == "dng" {
        raw.active_area.map_or(Point::zero(), |area| area.p)
    } else {
        Point::zero()
    };
    ensure!(
        matches!(raw.whitelevel.0.len(), 1 | 3),
        "Bayer white levels must be scalar or one per RGB plane"
    );
    let orientation = source_metadata
        .exif
        .orientation
        .map_or(raw.orientation, Orientation::from_u16);
    let (transpose, flip_x, flip_y) = orientation.to_flips();
    let (half_width, half_height) = (cw / 2, ch / 2);
    let (width, height) = if transpose {
        (half_height, half_width)
    } else {
        (half_width, half_height)
    };
    let mut channels = Vec::new();
    let mut raw_means = [0.0; 4];
    for &channel in green.channels(separate || keep_color_ratios) {
        let slot = channel.index();
        let (ox, oy) = offsets[slot];
        let bx = (cx + ox + black.width - black_origin.x % black.width) % black.width;
        let by = (cy + oy + black.height - black_origin.y % black.height) % black.height;
        let black_level = f64::from(black_values[by * black.width + bx]);
        let white_level = raw.whitelevel.0[if raw.whitelevel.0.len() == 1 {
            0
        } else {
            planes[slot]
        }];
        ensure!(
            black_level.is_finite()
                && black_level >= 0.0
                && black_level < f64::from(white_level)
                && white_level <= u32::from(u16::MAX),
            "invalid {} black/white calibration: {black_level}/{white_level}",
            channel.name()
        );
        if keep_color_ratios {
            let (sum, positive) = (0..half_height)
                .into_par_iter()
                .map(|y| {
                    let mut sum = 0u64;
                    let mut positive = 0u64;
                    for x in 0..half_width {
                        let value = data[(cy + 2 * y + oy) * raw.width + cx + 2 * x + ox];
                        if f64::from(value) > black_level {
                            sum += u64::from(value);
                            positive += 1;
                        }
                    }
                    (sum, positive)
                })
                .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            raw_means[slot] = (sum as f64 - positive as f64 * black_level).max(0.0)
                / (half_width * half_height) as f64;
        }
        if !green.channels(separate).contains(&channel) {
            continue;
        }
        let noise_profile = profiles[planes[slot]];
        if let Some([shot, read]) = noise_profile {
            let span = f64::from(white_level) - black_level;
            ensure!(
                (shot * span * 65535.0 + read * span * span).is_finite(),
                "NoiseProfile overflows the raw-code noise model"
            );
        }
        let mut metadata = SensorMetadata {
            make: raw.clean_make.clone(),
            model: raw.clean_model.clone(),
            raw_width: raw.width,
            raw_height: raw.height,
            width,
            height,
            crop: [cx, cy, half_width * 2, half_height * 2],
            orientation: orientation.to_u16(),
            storage_bits: raw.bps,
            black_level,
            white_level: white_level as u16,
            iso: None,
            standard_output_sensitivity: None,
            exposure_seconds: None,
            aperture_f_number: None,
            aperture_value_apex: None,
            focal_length_mm: None,
            lens_model: None,
            noise_profile,
        };
        apply_exif_metadata(&mut metadata, &source_metadata, standard_iso)?;
        let mut pixels = vec![0; width * height];
        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, pixel) in row.iter_mut().enumerate() {
                    let (mut sx, mut sy) = if transpose { (y, x) } else { (x, y) };
                    if flip_x {
                        sx = half_width - 1 - sx;
                    }
                    if flip_y {
                        sy = half_height - 1 - sy;
                    }
                    *pixel = data[(cy + 2 * sy + oy) * raw.width + cx + 2 * sx + ox];
                }
            });
        channels.push((
            channel,
            MonoImage {
                pixels,
                metadata,
                source_metadata: source_metadata.clone(),
            },
        ));
    }
    Ok(BayerImage {
        cfa,
        cfa_assumed: pattern.is_none(),
        color_ratios: keep_color_ratios
            .then(|| ColorRatios::from_raw_means(raw_means))
            .flatten(),
        white_balance,
        recorded_white_balance,
        channels,
    })
}
