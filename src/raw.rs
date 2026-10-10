use anyhow::{Context, Result, ensure};
use rawler::{
    RawImageData,
    decoders::{RawDecodeParams, WellKnownIFD},
    formats::tiff::Value,
    imgop::{Dim2, Point, Rect},
    rawimage::RawPhotometricInterpretation,
    rawsource::RawSource,
    tags::{DngTag, ExifTag},
};
use rayon::prelude::*;
use serde::Serialize;
use std::{fs::File, io::Read, path::Path};

#[derive(Clone, Debug, Serialize)]
pub struct SensorMetadata {
    pub make: String,
    pub model: String,
    pub raw_width: usize,
    pub raw_height: usize,
    pub width: usize,
    pub height: usize,
    pub crop: [usize; 4],
    pub orientation: u16,
    pub storage_bits: usize,
    pub black_level: f64,
    pub white_level: u16,
    pub iso: Option<u32>,
    pub standard_output_sensitivity: Option<u32>,
    pub exposure_seconds: Option<f64>,
    pub aperture_f_number: Option<f64>,
    pub aperture_value_apex: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub lens_model: Option<String>,
    pub noise_profile: Option<[f64; 2]>,
}

#[derive(Debug)]
pub struct MonoImage {
    pub pixels: Vec<u16>,
    pub metadata: SensorMetadata,
    pub source_metadata: rawler::decoders::RawMetadata,
}

pub fn decode(path: &Path, no_crop: bool) -> Result<MonoImage> {
    let mut header = [0; 8];
    File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .read_exact(&mut header)
        .context("reading DNG TIFF header")?;
    ensure!(
        header[..4] == *b"II\x2a\0" || header[..4] == *b"MM\0\x2a",
        "expected a classic TIFF-based DNG, not an embedded JPEG or another file type"
    );
    let source = RawSource::new(path).context("opening raw data source")?;
    let params = RawDecodeParams::default();
    let raw = rawler::decode(&source, &params)
        .with_context(|| format!("decoding raw samples from {}", path.display()))?;
    ensure!(raw.camera.mode == "dng", "input is not a DNG");
    let mut image = from_raw(raw, no_crop)?;
    // Keep the third-party metadata parser behind the same explicit panic boundary as raw decoding.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let decoder = rawler::get_decoder(&source)?;
        let source_metadata = decoder.raw_metadata(&source, &params)?;
        let exif = &source_metadata.exif;
        let exif_ifd = decoder.ifd(WellKnownIFD::Exif)?;
        let standard_iso = exif_ifd
            .as_ref()
            .and_then(|ifd| ifd.get_entry(ExifTag::StandardOutputSensitivity))
            .map(|entry| entry.value.get_u32(0))
            .transpose()
            .map_err(|_| anyhow::anyhow!("StandardOutputSensitivity must be an unsigned integer"))?
            .flatten();
        image.metadata.iso = exif
            .iso_speed
            .filter(|v| *v > 0)
            .or(standard_iso.filter(|v| *v > 0))
            .or(exif.recommended_exposure_index.filter(|v| *v > 0))
            .or(exif
                .iso_speed_ratings
                .filter(|v| *v > 0 && *v < u16::MAX)
                .map(u32::from));
        if let Some(time) = exif.exposure_time {
            ensure!(time.d != 0, "invalid EXIF exposure-time denominator");
            image.metadata.exposure_seconds = Some(f64::from(time.n) / f64::from(time.d));
        }
        image.metadata.standard_output_sensitivity = standard_iso;
        if let Some(aperture) = exif.fnumber {
            ensure!(aperture.d != 0, "invalid EXIF aperture denominator");
            image.metadata.aperture_f_number = Some(f64::from(aperture.n) / f64::from(aperture.d));
        }
        if let Some(aperture) = exif.aperture_value {
            ensure!(aperture.d != 0, "invalid EXIF APEX aperture denominator");
            let apex = f64::from(aperture.n) / f64::from(aperture.d);
            ensure!(
                (apex * 0.5).exp2().is_finite(),
                "EXIF APEX aperture is out of range"
            );
            image.metadata.aperture_value_apex = Some(apex);
        }
        if let Some(focal) = exif.focal_length {
            ensure!(focal.d != 0, "invalid EXIF focal-length denominator");
            image.metadata.focal_length_mm = Some(f64::from(focal.n) / f64::from(focal.d));
        }
        image.metadata.lens_model = exif.lens_model.clone();
        if let Some(ifd) = decoder.ifd(WellKnownIFD::Raw)?
            && let Some(entry) = ifd.get_entry(DngTag::NoiseProfile)
        {
            let Value::Double(values) = &entry.value else {
                anyhow::bail!("monochrome NoiseProfile must contain two DOUBLE coefficients");
            };
            ensure!(
                values.len() == 2 && values.iter().all(|v| v.is_finite() && *v >= 0.0),
                "monochrome NoiseProfile must contain two finite, nonnegative coefficients"
            );
            ensure!(
                values.iter().any(|v| *v > 0.0),
                "NoiseProfile cannot have zero total noise"
            );
            let span = f64::from(image.metadata.white_level) - image.metadata.black_level;
            ensure!(
                (values[0] * span * 65535.0 + values[1] * span * span).is_finite(),
                "NoiseProfile coefficients overflow the raw-code noise model"
            );
            image.metadata.noise_profile = Some([values[0], values[1]]);
        }
        image.source_metadata = source_metadata;
        Ok(())
    }))
    .map_err(|_| anyhow::anyhow!("DNG metadata decoder panicked on malformed metadata"))?
    .context("reading DNG sensitivity and noise metadata")?;
    Ok(image)
}

fn from_raw(raw: rawler::RawImage, no_crop: bool) -> Result<MonoImage> {
    ensure!(
        raw.cpp == 1
            && matches!(
                raw.photometric,
                RawPhotometricInterpretation::LinearRaw | RawPhotometricInterpretation::BlackIsZero
            ),
        "expected single-channel monochrome DNG; color/CFA input is not demosaiced"
    );
    let RawImageData::Integer(data) = raw.data else {
        anyhow::bail!("floating-point DNG is not supported; Leica integer samples are required");
    };
    ensure!(
        raw.width > 0 && raw.height > 0 && raw.width.checked_mul(raw.height) == Some(data.len()),
        "invalid decoded dimensions or sample count"
    );

    let levels = raw.blacklevel.as_vec();
    let black = f64::from(*levels.first().context("missing black level")?);
    ensure!(
        black.is_finite() && levels.iter().all(|v| f64::from(*v) == black),
        "non-uniform or invalid black levels are not supported"
    );
    let white = *raw.whitelevel.0.first().context("missing white level")?;
    ensure!(
        white <= u32::from(u16::MAX) && black >= 0.0 && black < f64::from(white),
        "invalid black/white calibration: {black}/{white}"
    );
    let area = if no_crop {
        None
    } else {
        raw.crop_area.or(raw.active_area)
    }
    .unwrap_or_else(|| Rect::new(Point::zero(), Dim2::new(raw.width, raw.height)));
    let (cx, cy, cw, ch) = (area.p.x, area.p.y, area.d.w, area.d.h);
    ensure!(
        cw > 0
            && ch > 0
            && cx.checked_add(cw).is_some_and(|v| v <= raw.width)
            && cy.checked_add(ch).is_some_and(|v| v <= raw.height),
        "DNG crop lies outside the raw image"
    );
    let (transpose, flip_x, flip_y) = raw.orientation.to_flips();
    let (width, height) = if transpose { (ch, cw) } else { (cw, ch) };
    let mut pixels = vec![0; width * height];
    pixels
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, pixel) in row.iter_mut().enumerate() {
                let (mut sx, mut sy) = if transpose { (y, x) } else { (x, y) };
                if flip_x {
                    sx = cw - 1 - sx;
                }
                if flip_y {
                    sy = ch - 1 - sy;
                }
                *pixel = data[(cy + sy) * raw.width + cx + sx];
            }
        });

    Ok(MonoImage {
        pixels,
        source_metadata: rawler::decoders::RawMetadata::default(),
        metadata: SensorMetadata {
            make: raw.clean_make,
            model: raw.clean_model,
            raw_width: raw.width,
            raw_height: raw.height,
            width,
            height,
            crop: [cx, cy, cw, ch],
            orientation: raw.orientation.to_u16(),
            storage_bits: raw.bps,
            black_level: black,
            white_level: white as u16,
            iso: None,
            standard_output_sensitivity: None,
            exposure_seconds: None,
            aperture_f_number: None,
            aperture_value_apex: None,
            focal_length_mm: None,
            lens_model: None,
            noise_profile: None,
        },
    })
}
