use crate::{
    formats::{Encoding, Format},
    parameters::Parameters,
    raw::{MonoImage, SensorMetadata},
    tone::{Rendered, Transfer, Transfers},
};
use anyhow::{Context, Result, ensure};
use rawler::{
    formats::tiff::{
        Rational, Value,
        writer::{DirectoryWriter, TiffWriter},
    },
    tags::{ExifTag, TiffCommonTag},
};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    fs,
    io::{BufWriter, Cursor, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

mod native;

#[derive(Debug)]
pub struct OutputPaths {
    pub png: PathBuf,
    pub jpeg: PathBuf,
    pub heic: PathBuf,
    pub avif: PathBuf,
    pub j2k: PathBuf,
    pub report: Option<PathBuf>,
    formats: Vec<Format>,
}

impl OutputPaths {
    pub fn new(stem: &Path, report: bool) -> Self {
        Self::with_formats(stem, report, &[Format::Png, Format::Jpeg])
    }

    pub fn with_formats(stem: &Path, report: bool, formats: &[Format]) -> Self {
        Self {
            png: stem.with_extension("png"),
            jpeg: stem.with_extension("jpg"),
            heic: stem.with_extension("heic"),
            avif: stem.with_extension("avif"),
            j2k: stem.with_extension("jp2"),
            report: report.then(|| stem.with_extension("json")),
            formats: formats.to_vec(),
        }
    }

    pub fn path(&self, format: Format) -> &Path {
        match format {
            Format::Png => &self.png,
            Format::Jpeg => &self.jpeg,
            Format::Heic => &self.heic,
            Format::Avif => &self.avif,
            Format::J2k => &self.j2k,
        }
    }

    pub fn primary(&self) -> Result<&Path> {
        self.formats
            .first()
            .map(|&format| self.path(format))
            .context("at least one output format must be selected")
    }

    pub fn all(&self) -> impl Iterator<Item = &Path> {
        self.formats
            .iter()
            .map(|&format| self.path(format))
            .chain(self.report.as_deref())
    }

    pub fn check(&self, overwrite: bool) -> Result<()> {
        self.primary()?;
        for (index, format) in self.formats.iter().enumerate() {
            ensure!(
                !self.formats[..index].contains(format),
                "duplicate output format: {format}"
            );
        }
        for path in self.all() {
            check_destination(path, overwrite)?;
        }
        if overwrite && self.report.is_none() {
            let sidecar = self.png.with_extension("json");
            ensure!(
                !sidecar
                    .try_exists()
                    .with_context(|| format!("checking {}", sidecar.display()))?,
                "existing sidecar {} would become stale; include --report when overwriting",
                sidecar.display()
            );
        }
        Ok(())
    }
}

pub fn check_destination(path: &Path, overwrite: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "output is not a regular file: {}",
                path.display()
            );
            ensure!(
                overwrite,
                "output exists: {}; use --overwrite to replace it",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("checking output {}", path.display()));
        }
    }
    Ok(())
}

pub fn save(
    paths: &OutputPaths,
    image: &MonoImage,
    rendered: &Rendered,
    report: &impl Serialize,
    transfers: Transfers,
    quality: u8,
    overwrite: bool,
) -> Result<()> {
    save_with_parameters(
        paths,
        image,
        rendered,
        report,
        transfers,
        quality,
        overwrite,
        &Parameters::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn save_with_parameters(
    paths: &OutputPaths,
    image: &MonoImage,
    rendered: &Rendered,
    report: &impl Serialize,
    transfers: Transfers,
    quality: u8,
    overwrite: bool,
    parameters: &Parameters,
) -> Result<()> {
    save_with_encodings(
        paths,
        image,
        rendered,
        report,
        transfers,
        quality,
        overwrite,
        parameters,
        &[],
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub fn save_with_encodings(
    paths: &OutputPaths,
    image: &MonoImage,
    rendered: &Rendered,
    report: &impl Serialize,
    transfers: Transfers,
    quality: u8,
    overwrite: bool,
    parameters: &Parameters,
    encodings: &[Encoding],
) -> Result<Vec<String>> {
    parameters.validate()?;
    for &format in &paths.formats {
        if !matches!(format, Format::Png | Format::Jpeg) {
            ensure!(
                encodings
                    .iter()
                    .filter(|encoding| encoding.format == format)
                    .count()
                    == 1,
                "{format} requires exactly one native encoding plan"
            );
        }
    }
    ensure!(
        encodings
            .iter()
            .all(|encoding| paths.formats.contains(&encoding.format)
                && !matches!(encoding.format, Format::Png | Format::Jpeg)),
        "encoding plan includes a disabled or non-native format"
    );
    let parent = paths
        .primary()?
        .parent()
        .context("output has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    paths.check(overwrite)?;
    let (png, jpeg) = rayon::join(
        || {
            if !paths.formats.contains(&Format::Png) {
                return Ok(None);
            }
            let png_exif = photographic_exif(image, transfers.png, 16, Format::Png)?;
            encode_png(
                parent,
                &image.metadata,
                &rendered.png,
                transfers.png,
                &png_exif,
            )
            .map(Some)
        },
        || {
            if !paths.formats.contains(&Format::Jpeg) {
                return Ok(None);
            }
            let jpeg_exif = photographic_exif(image, transfers.jpeg, 8, Format::Jpeg)?;
            ensure!(
                jpeg_exif.len() <= 65527,
                "photographic EXIF exceeds JPEG's 65527-byte metadata limit; no metadata was silently discarded"
            );
            encode_jpeg(
                parent,
                &image.metadata,
                &rendered.jpeg,
                quality,
                &jpeg_exif,
                parameters.jpeg_optimize_huffman,
            )
            .map(Some)
        },
    );
    let png = png.context("encoding 16-bit PNG")?;
    let jpeg = jpeg.context("encoding grayscale JPEG")?;
    let mut additional = Vec::new();
    let mut warnings = Vec::new();
    for encoding in encodings {
        let exif = photographic_exif(
            image,
            transfers.png,
            u16::from(encoding.bits),
            encoding.format,
        )?;
        let (file, notes) = native::encode(
            parent,
            &image.metadata,
            &rendered.png,
            transfers.png,
            &exif,
            encoding,
        )
        .with_context(|| format!("encoding {} at {} bits", encoding.format, encoding.bits))?;
        additional.push((encoding.format, file));
        warnings.extend(
            notes
                .into_iter()
                .map(|note| format!("{}: {note}", encoding.format)),
        );
    }
    let json = if paths.report.is_some() {
        let mut file = temporary(parent)?;
        serde_json::to_writer_pretty(file.as_file_mut(), report).context("encoding JSON report")?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        Some(file)
    } else {
        None
    };
    if let Some(png) = png {
        persist(png, &paths.png, overwrite)?;
    }
    if let Some(jpeg) = jpeg {
        persist(jpeg, &paths.jpeg, overwrite)?;
    }
    for (format, file) in additional {
        persist(file, paths.path(format), overwrite)?;
    }
    if let (Some(file), Some(path)) = (json, &paths.report) {
        persist(file, path, overwrite)?;
    }
    Ok(warnings)
}

fn temporary(parent: &Path) -> Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(".dng-monochrome-")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary output in {}", parent.display()))
}

fn encode_png(
    parent: &Path,
    metadata: &SensorMetadata,
    pixels: &[u16],
    transfer: Transfer,
    exif: &[u8],
) -> Result<NamedTempFile> {
    let mut file = temporary(parent)?;
    let mut bytes = vec![0u8; pixels.len() * 2];
    bytes
        .par_chunks_exact_mut(2)
        .zip(pixels.par_iter())
        .for_each(|(out, &v)| {
            out.copy_from_slice(&v.to_be_bytes());
        });
    {
        let mut buffer = BufWriter::new(file.as_file_mut());
        let mut info = png::Info::with_size(metadata.width as u32, metadata.height as u32);
        info.exif_metadata = Some(exif.into());
        let mut encoder = png::Encoder::with_info(&mut buffer, info)?;
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder.set_compression(png::Compression::High);
        match transfer {
            Transfer::Srgb => encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual),
            Transfer::Linear => encoder.set_source_gamma(png::ScaledFloat::new(1.0)),
        }
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&bytes)?;
        writer.finish()?;
        buffer.flush()?;
    }
    file.as_file().sync_all()?;
    Ok(file)
}

fn encode_jpeg(
    parent: &Path,
    metadata: &SensorMetadata,
    pixels: &[u8],
    quality: u8,
    exif: &[u8],
    optimize_huffman: bool,
) -> Result<NamedTempFile> {
    ensure!(
        metadata.width <= usize::from(u16::MAX) && metadata.height <= usize::from(u16::MAX),
        "image dimensions exceed the JPEG limit of 65535 per axis"
    );
    let mut file = temporary(parent)?;
    {
        let mut buffer = BufWriter::new(file.as_file_mut());
        let mut encoder = jpeg_encoder::Encoder::new(&mut buffer, quality);
        encoder.add_exif_metadata(exif)?;
        encoder.set_optimized_huffman_tables(optimize_huffman);
        encoder.encode(
            pixels,
            metadata.width as u16,
            metadata.height as u16,
            jpeg_encoder::ColorType::Luma,
        )?;
        buffer.flush()?;
    }
    file.as_file().sync_all()?;
    Ok(file)
}

fn photographic_exif(
    image: &MonoImage,
    transfer: Transfer,
    bits: u16,
    format: Format,
) -> Result<Vec<u8>> {
    let metadata = &image.metadata;
    let mut bytes = Cursor::new(Vec::new());
    let mut tiff = TiffWriter::new(&mut bytes)?;
    let mut root = DirectoryWriter::new();
    let mut exif = DirectoryWriter::new();
    image
        .source_metadata
        .write_exif_tags(&mut tiff, &mut root, &mut exif)?;
    root.add_tag(TiffCommonTag::Make, metadata.make.clone());
    root.add_tag(TiffCommonTag::Model, metadata.model.clone());
    root.add_tag(
        TiffCommonTag::Software,
        concat!("dng-monochrome ", env!("CARGO_PKG_VERSION")),
    );
    root.add_tag(ExifTag::Orientation, 1u16);
    root.add_tag(TiffCommonTag::ImageWidth, metadata.width as u32);
    root.add_tag(TiffCommonTag::ImageLength, metadata.height as u32);
    root.add_tag(TiffCommonTag::BitsPerSample, bits);
    root.add_tag(TiffCommonTag::SamplesPerPixel, 1u16);
    root.add_tag(TiffCommonTag::PhotometricInt, 1u16);
    exif.add_tag_undefined(ExifTag::ExifVersion, b"0232".to_vec());
    exif.add_untyped_tag(0x9101, Value::Undefined(vec![1, 0, 0, 0]));
    exif.add_tag(ExifTag::ExifImageWidth, metadata.width as u32);
    exif.add_tag(ExifTag::ExifImageHeight, metadata.height as u32);
    exif.add_tag(
        ExifTag::ColorSpace,
        if transfer == Transfer::Srgb {
            1u16
        } else {
            65535u16
        },
    );
    if let Some(iso) = metadata.standard_output_sensitivity {
        exif.add_tag(ExifTag::StandardOutputSensitivity, iso);
    }
    if format != Format::Png && transfer == Transfer::Linear {
        exif.add_untyped_tag(0xa500, Rational { n: 1, d: 1 });
    }
    let exif_offset = exif.build(&mut tiff)?;
    root.add_tag(ExifTag::ExifOffset, exif_offset);
    tiff.build(root)?;
    Ok(bytes.into_inner())
}

fn persist(file: NamedTempFile, path: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        file.persist(path)
    } else {
        file.persist_noclobber(path)
    }
    .map_err(|error| error.error)
    .with_context(|| {
        format!(
            "publishing {}; any already-published companion files remain intact",
            path.display()
        )
    })?;
    Ok(())
}
