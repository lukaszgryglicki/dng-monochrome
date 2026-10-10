use anyhow::{Result, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Png,
    #[serde(rename = "jpg")]
    Jpeg,
    Heic,
    Avif,
    J2k,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Heic => "heic",
            Self::Avif => "avif",
            Self::J2k => "jp2",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Heic => "HEIC",
            Self::Avif => "AVIF",
            Self::J2k => "JPEG2000",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Lossless,
    Lossy,
}

#[derive(Debug, Args)]
pub struct OutputOptions {
    #[arg(
        long,
        overrides_with = "no_png",
        help = "Save 16-bit grayscale PNG (default)"
    )]
    pub png: bool,
    #[arg(long, overrides_with = "png", help = "Do not save PNG")]
    pub no_png: bool,
    #[arg(
        long,
        visible_alias = "jpeg",
        overrides_with = "no_jpg",
        help = "Also save the original 8-bit grayscale JPEG"
    )]
    pub jpg: bool,
    #[arg(
        long,
        visible_alias = "no-jpeg",
        overrides_with = "jpg",
        help = "Do not save JPEG (default)"
    )]
    pub no_jpg: bool,
    #[arg(
        long,
        overrides_with = "no_heic",
        help = "Save monochrome HEIC using x265 at maximum effort"
    )]
    pub heic: bool,
    #[arg(long, overrides_with = "heic", help = "Do not save HEIC (default)")]
    pub no_heic: bool,
    #[arg(
        long,
        overrides_with = "no_avif",
        help = "Save monochrome AVIF using libaom at maximum effort"
    )]
    pub avif: bool,
    #[arg(long, overrides_with = "avif", help = "Do not save AVIF (default)")]
    pub no_avif: bool,
    #[arg(
        long,
        overrides_with = "no_j2k",
        help = "Save grayscale JPEG2000 in a .jp2 container"
    )]
    pub j2k: bool,
    #[arg(long, overrides_with = "j2k", help = "Do not save JPEG2000 (default)")]
    pub no_j2k: bool,

    #[arg(
        long,
        overrides_with = "lossy",
        help = "Use lossless HEIC/AVIF/JPEG2000 compression (default); PNG/JPEG unchanged"
    )]
    pub lossless: bool,
    #[arg(
        long,
        overrides_with = "lossless",
        help = "Use lossy HEIC/AVIF/JPEG2000 compression; per-format modes override"
    )]
    pub lossy: bool,
    #[arg(
        long,
        value_enum,
        help = "HEIC compression; overrides --lossless/--lossy"
    )]
    pub heic_mode: Option<Compression>,
    #[arg(
        long,
        value_enum,
        help = "AVIF compression; overrides --lossless/--lossy"
    )]
    pub avif_mode: Option<Compression>,
    #[arg(
        long,
        value_enum,
        help = "JPEG2000 compression; overrides --lossless/--lossy"
    )]
    pub j2k_mode: Option<Compression>,
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100),
        help = "HEIC lossy quality, 1-100; ignored in lossless mode")]
    pub heic_quality: u8,
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100),
        help = "AVIF lossy quality, 1-100; ignored in lossless mode")]
    pub avif_quality: u8,
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100),
        help = "JPEG2000 lossy quality, 1-100; ignored in lossless mode")]
    pub j2k_quality: u8,
}

impl OutputOptions {
    pub fn enabled(&self) -> Vec<Format> {
        [
            (Format::Png, self.png || !self.no_png),
            (Format::Jpeg, self.jpg && !self.no_jpg),
            (Format::Heic, self.heic && !self.no_heic),
            (Format::Avif, self.avif && !self.no_avif),
            (Format::J2k, self.j2k && !self.no_j2k),
        ]
        .into_iter()
        .filter_map(|(format, enabled)| enabled.then_some(format))
        .collect()
    }

    pub fn validate(&self, analyze: bool) -> Result<()> {
        ensure!(
            analyze || !self.enabled().is_empty(),
            "at least one output format must be enabled (--png, --jpg, --heic, --avif or --j2k)"
        );
        Ok(())
    }

    pub fn encodings(&self, measured_dr: Option<f64>) -> Result<Vec<Encoding>> {
        let default = if self.lossy {
            Compression::Lossy
        } else {
            Compression::Lossless
        };
        self.enabled()
            .into_iter()
            .filter(|format| !matches!(format, Format::Png | Format::Jpeg))
            .map(|format| {
                let (mode, quality) = match format {
                    Format::Heic => (self.heic_mode, self.heic_quality),
                    Format::Avif => (self.avif_mode, self.avif_quality),
                    Format::J2k => (self.j2k_mode, self.j2k_quality),
                    Format::Png | Format::Jpeg => unreachable!(),
                };
                Encoding::new(format, measured_dr, mode.unwrap_or(default), quality)
            })
            .collect()
    }

    pub fn summary(&self) -> String {
        let formats = self.enabled();
        let names = formats
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("/");
        let unit = if formats == [Format::Png, Format::Jpeg] {
            "pair(s)"
        } else if formats.len() == 1 {
            "image(s)"
        } else {
            "set(s)"
        };
        format!("{names} {unit}")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Encoding {
    pub format: Format,
    pub bits: u8,
    pub compression: Compression,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    pub measured_dr: Option<f64>,
    pub depth_capped: bool,
}

impl Encoding {
    pub fn new(
        format: Format,
        measured_dr: Option<f64>,
        compression: Compression,
        quality: u8,
    ) -> Result<Self> {
        ensure!(
            (1..=100).contains(&quality),
            "codec quality must be in 1..=100"
        );
        ensure!(
            measured_dr.is_none_or(|value| value.is_finite() && value >= 0.0),
            "noise-measured DR must be finite and nonnegative"
        );
        let supported: &[u8] = match format {
            Format::Heic | Format::Avif => &[8, 10, 12],
            Format::J2k => &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            Format::Png | Format::Jpeg => {
                anyhow::bail!("PNG/JPEG retain their fixed legacy precision")
            }
        };
        let maximum = supported[supported.len() - 1];
        let bits = measured_dr
            .and_then(|dr| {
                supported
                    .iter()
                    .copied()
                    .find(|&bits| f64::from(bits) >= dr)
            })
            .unwrap_or(maximum);
        Ok(Self {
            format,
            bits,
            compression,
            quality: (compression == Compression::Lossy).then_some(quality),
            measured_dr,
            depth_capped: measured_dr.is_some_and(|dr| dr > f64::from(maximum)),
        })
    }

    pub fn warning(&self) -> Option<String> {
        match self.measured_dr {
            None => Some(format!(
                "{}: noise-based DR unavailable; using the maximum {}-bit output precision",
                self.format, self.bits
            )),
            Some(dr) if self.depth_capped => Some(format!(
                "{}: noise-based DR {dr:.3} bits exceeds the codec limit; using its maximum {}-bit precision",
                self.format, self.bits
            )),
            Some(_) => None,
        }
    }
}

pub(crate) fn quantize(value: u16, bits: u8) -> u16 {
    ((u32::from(value) * ((1u32 << bits) - 1) + 32767) / 65535) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_covers_measured_noise_dr_or_caps_at_the_codec_limit() {
        for format in [Format::Heic, Format::Avif] {
            for (dr, bits, capped) in [
                (0.0, 8, false),
                (8.0, 8, false),
                (8.001, 10, false),
                (10.0, 10, false),
                (10.001, 12, false),
                (12.0, 12, false),
                (12.001, 12, true),
                (16.0, 12, true),
            ] {
                let output = Encoding::new(format, Some(dr), Compression::Lossless, 90).unwrap();
                assert_eq!((output.bits, output.depth_capped), (bits, capped));
                assert_eq!(output.warning().is_some(), capped);
                assert_eq!(output.quality, None);
            }
        }
        for bits in 1..=16 {
            let output = Encoding::new(
                Format::J2k,
                Some(f64::from(bits) - 0.01),
                Compression::Lossy,
                90,
            )
            .unwrap();
            assert_eq!(output.bits, bits);
            assert!(!output.depth_capped);
            assert_eq!(output.quality, Some(90));
        }
        let output = Encoding::new(Format::J2k, Some(17.0), Compression::Lossless, 90).unwrap();
        assert_eq!(output.bits, 16);
        assert!(output.depth_capped);
    }

    #[test]
    fn unknown_dr_is_explicit_and_uses_maximum_precision() {
        for (format, bits) in [(Format::Heic, 12), (Format::Avif, 12), (Format::J2k, 16)] {
            let output = Encoding::new(format, None, Compression::Lossless, 90).unwrap();
            assert_eq!(output.bits, bits);
            assert!(!output.depth_capped);
            assert!(output.warning().unwrap().contains("unavailable"));
        }
        for dr in [f64::NAN, f64::INFINITY, -0.1] {
            assert!(Encoding::new(Format::Heic, Some(dr), Compression::Lossless, 90).is_err());
        }
    }

    #[test]
    fn quantization_preserves_endpoints_and_every_selected_depth_level() {
        for bits in 1..=16 {
            let maximum = (1u32 << bits) - 1;
            assert_eq!(quantize(0, bits), 0);
            assert_eq!(u32::from(quantize(u16::MAX, bits)), maximum);
            let mut previous = 0;
            for value in 0..=u16::MAX {
                let quantized = quantize(value, bits);
                assert!(
                    quantized >= previous && quantized <= previous + u16::from(previous < u16::MAX)
                );
                previous = quantized;
            }
        }
    }
}
