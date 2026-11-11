use crate::{
    denoise, dirty,
    expression::{Expression, FunctionPolicy, ToneBand},
    formats::{Encoding, Format, OutputOptions},
    output::{self, OutputPaths},
    parameters::Parameters,
    range::{self, Histogram, RangeOptions},
    raw,
    tone::{self, Transfer, Transfers},
};
use anyhow::{Context, Result, ensure};
use clap::{ArgGroup, CommandFactory, Parser};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};
use walkdir::WalkDir;

mod color;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Develop DNG/ARW raw images as monochrome or explicit --color RGB into selectable image formats.",
    long_about = "Develop integer DNG/ARW raw rasters as monochrome by default, or use --color for independent \
        half-resolution R/G1/G2/B extraction from 2x2 cells. Mode is never autodetected; \
        --color assumes RGGB when no CFA layout is declared. Directories are searched recursively. \
        Automatic range detection estimates sparse histogram tails and spatial noise; it cannot measure \
        true sensor dynamic range or recover clipped detail. PNGs are always 16-bit (RGB with --color) with \
        maximum lossless compression; only PNG is saved by default. --jpg restores the original \
        8-bit JPEG companion. --all enables all six formats; --none disables them before \
        selecting individual formats. HEIC/AVIF use 8/10/12 bits; \
        JPEG2000/JPEG XL use 8-16 bits (1-16 with --j2k-allow-low-bits/--jxl-allow-low-bits), automatically selected \
        from the existing noise-measured DR, capped at the codec maximum. \
        New formats default to lossless compression of the selected-depth samples and share the \
        developed PNG transfer. All formats carry photographic EXIF. Optimization is on by default, \
        clipping strength is 3, PNG and JPEG transfers are both linear. \
        Tunable constants are exposed as --param-* options.",
    after_help = "Single-dash long options also work: -dark 0.8 -light 1.2% -clip-strength 9 -func 'x^.5' -best.\n\
        Pipeline: crop/orient -> optional sensor-dirt correction -> optional noise reduction -> range stretch -> optimize -> function/tone-band -> policy -> transfers.\n\
        --fix-dirty/-fd completes all RAW analysis before development; --no-optimize skips optimization.\n\
        --noise-reduction/-nr defaults to level 1; --noise-reduction-level/-nrl accepts 0..2 and also enables it.\n\
        Noise/range/DR and tone fitting use the original pre-denoising samples.\n\
        Final RGB: color scaling -> --channel-mix/-cm or --bw-from-color/-bw -> --hue -> max-color -> channel selection.\n\
        --natural-color-level/-ncl accepts 0..2: 0 off, 1 current correction, 2 stronger correction.\n\
        --max-color/-mc enhances final RGB only; --max-color-level/-mcl: 0 off, 1 gamut-safe vivid, 2 extreme.\n\
        x is normalized LINEAR light in [0,1], before the final display transfer.\n\
        Examples:\n  \
        dng-monochrome photos/ -o dng-mono --both --report\n  \
        dng-monochrome shot.DNG -clip-strength 9 -best\n  \
        dng-monochrome shot.DNG -dark 0.8 -light 1.2% -func 'sin(pi*x)^2' -func-scale\n  \
        dng-monochrome shot.DNG --no-optimize --dark 0 --light 0\n  \
        dng-monochrome shot.DNG -tone-band 0:0.3:0.4\n  \
        dng-monochrome shot.DNG -jpg --png-transfer linear --jpeg-transfer srgb\n  \
        dng-monochrome shot.DNG -no-png -heic -avif -j2k -jxl\n  \
        dng-monochrome shot.DNG -heic -heic-mode lossy -heic-quality 95\n\
        dng-monochrome --color --none --jxl --j2k --avif shot.ARW\n  \
        dng-monochrome -c -4 -ga -kc --both --report photos/\n\
        dng-monochrome shot.ARW -c -nc --transfer srgb -fdl reference-raws/ -o cleaned\n\
        Expressions: + - * / % ^, unary +/- and parentheses; pi, e; sqrt, abs, exp, ln, log,\n\
        log2, log10, log1p, exp2, expm1, sin/cos/tan, asin/acos/atan/atan2, sinh/cosh/tanh,\n\
        asinh/acosh/atanh, floor/ceil/round, sign/signum, min/max, pow, hypot, clamp,\n\
        srgb(x), srgb_band(x,start,end,amount). Tone bands use [0,1] intensities, not percentiles;\n\
        0 amount is unchanged, 1 is full sRGB shape within the band. Bands must not overlap.\n\
        Powers are right-associative; trig uses radians; write multiplication explicitly.\n\
        NaN/infinity are errors even with clipping. See README for algorithms and limitations.",
    group(ArgGroup::new("function-policy").args(["func_clip", "func_scale", "func_wrap"])),
    group(ArgGroup::new("curve").args(["function", "tone_bands"]))
)]
pub struct Cli {
    #[arg(required = true, num_args = 1.., value_name = "RAW_OR_DIRECTORY")]
    pub inputs: Vec<PathBuf>,

    #[arg(
        short = 'c',
        long,
        help = "Force 2x2 Bayer splitting into half-size RGB; assume RGGB if CFA metadata is absent"
    )]
    pub color: bool,

    #[arg(
        short = '4',
        long,
        requires = "color",
        help = "Also save independently processed R/G1/G2/B files, e.g. photoG1.png"
    )]
    pub channels: bool,

    #[arg(long, requires = "color", overrides_with_all = ["g1", "g2", "ga"],
        help = "Use the first green in each cropped Bayer cell (default in --color mode)")]
    pub g1: bool,

    #[arg(long, requires = "color", overrides_with_all = ["g1", "g2", "ga"],
        help = "Use the second green in each cropped Bayer cell")]
    pub g2: bool,

    #[arg(long, requires = "color", overrides_with_all = ["g1", "g2", "ga"],
        help = "Average independently developed G1/G2 after aligning their precision")]
    pub ga: bool,

    #[arg(long, requires = "color", value_parser = parse_channel_order, value_name = "RGB",
        help = "Select final RGB channels with three r/g/b letters (default rgb); shortcuts include -bgr, -ggr and -rbb; companions unchanged")]
    pub channel_order: Option<[usize; 3]>,

    #[arg(long, visible_alias = "cm", requires = "color",
        conflicts_with_all = ["bw_from_color", "analyze"],
        value_parser = parse_channel_mix, allow_hyphen_values = true, value_name = "RR,RG,RB,GR,GG,GB,BR,BG,BB",
        help = "Mix final RGB with nine nonnegative weights, normalized separately per output R/G/B row; every row needs positive total weight")]
    pub channel_mix: Option<[[f64; 3]; 3]>,

    #[arg(
        long,
        visible_alias = "bw",
        requires = "color",
        conflicts_with = "analyze",
        help = "Make final RGB black-and-white using 0.299 R + 0.587 G + 0.114 B in all three outputs; shortcut for --channel-mix"
    )]
    pub bw_from_color: bool,

    #[arg(long, requires = "color", conflicts_with = "analyze",
        value_parser = parse_hue, allow_hyphen_values = true, value_name = "DEGREES",
        help = "Rotate final RGB hue in HSV after mixing; positive clockwise (red -> yellow), negative counterclockwise; wraps at 360 degrees")]
    pub hue: Option<f64>,

    #[arg(long, visible_aliases = ["kc", "kcr"], requires = "color",
        help = "Scale final RGB by black-subtracted RAW mean ratios; keep the strongest channel and separate channel files unchanged")]
    pub keep_color_ratios: bool,

    #[arg(
        long,
        visible_alias = "wb",
        requires = "color",
        conflicts_with = "keep_color_ratios",
        help = "Scale only final RGB by recorded RAW white-balance gains normalized to a maximum of 1; separate channels stay unchanged"
    )]
    pub white_balance: bool,

    #[arg(
        long,
        visible_alias = "nc",
        requires = "color",
        conflicts_with_all = ["keep_color_ratios", "white_balance", "analyze"],
        help = "Estimate white balance from developed final RGB, without changing separate channels; use --report for factors"
    )]
    pub natural_color: bool,

    #[arg(
        long,
        visible_alias = "ncl",
        requires = "color",
        conflicts_with_all = ["keep_color_ratios", "white_balance", "analyze"],
        value_parser = parse_natural_color_level,
        allow_hyphen_values = true,
        value_name = "LEVEL",
        help = "Natural-color strength 0..2 (default 1 with --natural-color); also enables it. 0 bypasses correction, 1 is unchanged behavior, 2 squares the estimated factors"
    )]
    pub natural_color_level: Option<f64>,

    #[arg(
        long,
        visible_alias = "mc",
        requires = "color",
        conflicts_with = "analyze",
        help = "Enhance final RGB chroma at level 1 without added clipping; after color scaling, before channel selection; default off"
    )]
    pub max_color: bool,

    #[arg(
        long,
        visible_alias = "mcl",
        requires = "color",
        conflicts_with = "analyze",
        value_parser = parse_color_level,
        allow_hyphen_values = true,
        value_name = "LEVEL",
        help = "Max-color level 0..2 (default 1 when enabled); implies --max-color. 0 is unchanged; >1 subtracts competing channels for extreme colors and may exaggerate noise"
    )]
    pub max_color_level: Option<f64>,

    #[arg(
        long,
        visible_alias = "fd",
        help = "Analyze all RAWs first, then brighten confidently recurring sensor-dirt shadows before development"
    )]
    pub fix_dirty: bool,

    #[arg(
        long,
        visible_aliases = ["fdx", "fix-dust-extra"],
        help = "Also detect enclosed irregular dirt/curved fibers; implies --fix-dirty; default off; requires recurring shape evidence and a smooth surrounding ring"
    )]
    pub fix_dirty_extra: bool,

    #[arg(
        long,
        visible_alias = "fdl",
        num_args = 1..,
        action = clap::ArgAction::Append,
        value_name = "RAW_OR_DIRECTORY",
        help = "Separate dirt-reference files/directories; implies --fix-dirty; default: processing inputs. Put target inputs before this list"
    )]
    pub fix_dirty_list: Vec<PathBuf>,

    #[arg(
        long,
        visible_alias = "fds",
        value_parser = parse_dust_strength,
        allow_hyphen_values = true,
        value_name = "STRENGTH",
        help = "Sensor-dirt correction strength 0..2 (default 1); implies --fix-dirty. 0 leaves pixels unchanged; >1 may amplify noise or create bright patches"
    )]
    pub fix_dust_strength: Option<f64>,

    #[arg(
        long,
        visible_alias = "nr",
        help = "Enable noise-adaptive, edge-preserving RAW denoising at level 1; disabled by default"
    )]
    pub noise_reduction: bool,

    #[arg(
        long,
        visible_alias = "nrl",
        value_parser = parse_noise_level,
        allow_hyphen_values = true,
        value_name = "LEVEL",
        help = "Noise reduction level 0..2 (default 1 when enabled); implies --noise-reduction. 0 is unchanged; higher levels may soften fine detail"
    )]
    pub noise_reduction_level: Option<f64>,

    #[arg(
        short = 'o',
        long,
        alias = "output-dir",
        default_value = "dng-mono",
        help = "Output directory; recursive input subdirectories are preserved"
    )]
    pub output: PathBuf,

    #[arg(long, value_parser = parse_percentage, allow_hyphen_values = true, value_name = "PERCENT",
        help = "Discard this percent of darkest pixels, e.g. 0.8 or 0.8%; default: detect")]
    pub dark: Option<f64>,

    #[arg(long, value_parser = parse_percentage, allow_hyphen_values = true, value_name = "PERCENT",
        help = "Discard this percent of lightest pixels from the full histogram; default: detect")]
    pub light: Option<f64>,

    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=9),
        help = "Automatic clipping strength: 1 conservative, 9 about 1-2% per tail; manual ends override")]
    pub clip_strength: u8,

    #[arg(
        long = "func",
        allow_hyphen_values = true,
        value_name = "EXPRESSION",
        help = "Map normalized linear light x; quote expressions, e.g. 'x^2' or 'sqrt(x)'"
    )]
    pub function: Option<String>,

    #[arg(
        long = "tone-band",
        value_name = "START:END:AMOUNT",
        allow_hyphen_values = true,
        help = "Lift a normalized intensity band toward sRGB; repeat for shadows/highlights; shortcut for --func"
    )]
    pub tone_bands: Vec<ToneBand>,

    #[arg(
        long,
        requires = "curve",
        help = "Clamp finite function results to [0,1] (default policy)"
    )]
    pub func_clip: bool,

    #[arg(
        long,
        requires = "curve",
        help = "Scale actual function min/max to [0,1]; constant results are errors"
    )]
    pub func_scale: bool,

    #[arg(
        long,
        requires = "curve",
        help = "Wrap out-of-range finite results modulo 1; preserve values already in [0,1]"
    )]
    pub func_wrap: bool,

    #[arg(
        long,
        visible_alias = "best",
        conflicts_with_all = ["both", "no_optimize"],
        help = "Apply photographic exposure/contrast optimization (default) and print its interior mapping"
    )]
    pub optimize: bool,

    #[arg(
        long,
        conflicts_with = "both",
        help = "Disable photographic optimization; apply range selection and any custom function only"
    )]
    pub no_optimize: bool,

    #[arg(
        long,
        help = "Write auto/ and best/ versions, decoding each raw image only once"
    )]
    pub both: bool,

    #[arg(
        long,
        value_enum,
        help = "Set the PNG/master and JPEG transfers; format-specific options take precedence"
    )]
    pub transfer: Option<Transfer>,

    #[arg(
        long,
        value_enum,
        help = "PNG/master transfer, also used by HEIC/AVIF/JPEG2000/JPEG XL; default linear, overrides --transfer"
    )]
    pub png_transfer: Option<Transfer>,

    #[arg(
        long,
        visible_alias = "jpg-transfer",
        value_enum,
        help = "JPEG transfer; default linear, overrides --transfer; srgb has finer shadows"
    )]
    pub jpeg_transfer: Option<Transfer>,

    #[arg(long, visible_alias = "jpg-quality", default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100),
        help = "Grayscale JPEG quality, 1-100")]
    pub jpeg_quality: u8,

    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u16).range(0..=256),
        help = "Total worker budget, shared between active files; 0 detects available CPU parallelism")]
    pub threads: u16,

    #[arg(short = 'j', long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=256),
        help = "Maximum files processed at once; each gets a share of --threads; 1 processes files sequentially")]
    pub jobs: u16,

    #[arg(
        long,
        help = "Keep the full raw raster instead of the recommended crop; orientation still applies"
    )]
    pub no_crop: bool,

    #[arg(
        long,
        help = "Write a JSON sidecar per developed image with ranges, noise estimates and codec settings"
    )]
    pub report: bool,

    #[arg(long, conflicts_with_all = ["optimize", "no_optimize", "both", "function", "tone_bands", "report", "overwrite"],
        help = "Only print one JSON range-analysis object per input; create no output files")]
    pub analyze: bool,

    #[arg(
        long,
        help = "Explicitly allow atomic replacement of existing output files"
    )]
    pub overwrite: bool,

    #[arg(
        short = 'v',
        long,
        visible_alias = "debug",
        conflicts_with = "silent",
        help = "Print detailed per-photo metadata, noise analysis and range diagnostics"
    )]
    pub verbose: bool,

    #[arg(
        short = 'q',
        long,
        help = "Suppress progress, warnings and completion; errors and --analyze JSON remain"
    )]
    pub silent: bool,

    #[command(flatten)]
    pub formats: OutputOptions,

    #[command(flatten)]
    pub parameters: Parameters,
}

impl Cli {
    fn natural_level(&self) -> f64 {
        self.natural_color_level
            .unwrap_or(if self.natural_color { 1.0 } else { 0.0 })
    }

    fn color_level(&self) -> f64 {
        self.max_color_level
            .unwrap_or(if self.max_color { 1.0 } else { 0.0 })
    }

    fn noise_level(&self) -> f64 {
        self.noise_reduction_level
            .unwrap_or(if self.noise_reduction { 1.0 } else { 0.0 })
    }

    pub fn green(&self) -> crate::color::Green {
        if self.ga {
            crate::color::Green::Average
        } else if self.g2 {
            crate::color::Green::G2
        } else {
            crate::color::Green::G1
        }
    }

    pub fn try_parse_compat(args: impl IntoIterator<Item = OsString>) -> Result<Self, clap::Error> {
        let cli = Self::try_parse_from(normalize_arguments(args))?;
        cli.parameters.validate().map_err(|error| {
            Self::command().error(clap::error::ErrorKind::ValueValidation, error.to_string())
        })?;
        cli.expression_source().map_err(|error| {
            Self::command().error(clap::error::ErrorKind::ValueValidation, error.to_string())
        })?;
        cli.formats.validate(cli.analyze).map_err(|error| {
            Self::command().error(clap::error::ErrorKind::ValueValidation, error.to_string())
        })?;
        Ok(cli)
    }

    pub fn expression_source(&self) -> Result<Option<String>> {
        ensure!(
            self.function.is_none() || self.tone_bands.is_empty(),
            "--tone-band is a shortcut for --func; combine curves explicitly inside --func instead"
        );
        if self.tone_bands.is_empty() {
            Ok(self.function.clone())
        } else {
            ToneBand::expression(&self.tone_bands).map(Some)
        }
    }

    pub fn policy(&self) -> FunctionPolicy {
        if self.func_scale {
            FunctionPolicy::Scale
        } else if self.func_wrap {
            FunctionPolicy::Wrap
        } else {
            FunctionPolicy::Clip
        }
    }

    pub fn optimization_enabled(&self) -> bool {
        !self.no_optimize
    }

    pub fn transfers(&self) -> Transfers {
        let defaults = Transfers::default();
        Transfers {
            png: self.png_transfer.or(self.transfer).unwrap_or(defaults.png),
            jpeg: self
                .jpeg_transfer
                .or(self.transfer)
                .unwrap_or(defaults.jpeg),
        }
    }
}

fn parse_percentage(input: &str) -> Result<f64, String> {
    let text = input.trim();
    let value: f64 = text
        .strip_suffix('%')
        .unwrap_or(text)
        .trim()
        .parse()
        .map_err(|_| "expected a percentage such as 0.8 or 1.2%".to_string())?;
    if value.is_finite() && (0.0..100.0).contains(&value) {
        Ok(value)
    } else {
        Err("percentage must be finite and in [0, 100)".into())
    }
}

fn parse_channel_order(input: &str) -> Result<[usize; 3], String> {
    let mut order = [0; 3];
    let error = "channel order must be three lowercase r/g/b letters, e.g. bgr or ggr";
    if input.len() != order.len() {
        return Err(error.into());
    }
    for (component, letter) in order.iter_mut().zip(input.bytes()) {
        *component = match letter {
            b'r' => 0,
            b'g' => 1,
            b'b' => 2,
            _ => return Err(error.into()),
        };
    }
    Ok(order)
}

fn parse_channel_mix(input: &str) -> Result<[[f64; 3]; 3], String> {
    let error = "channel mix requires nine finite nonnegative numbers separated by commas";
    let mut fields = input.split(',');
    let mut rows = [[0.0; 3]; 3];
    for value in rows.iter_mut().flatten() {
        *value = fields
            .next()
            .ok_or(error)?
            .trim()
            .parse::<f64>()
            .map_err(|_| error)?;
        if !value.is_finite() || *value < 0.0 {
            return Err(error.into());
        }
    }
    if fields.next().is_some() {
        return Err(error.into());
    }
    for row in &mut rows {
        let maximum = row.iter().copied().fold(0.0, f64::max);
        if maximum == 0.0 {
            return Err("every channel-mix output row must have positive total weight".into());
        }
        let mut total = row.iter().sum::<f64>();
        if !total.is_finite() {
            for value in row.iter_mut() {
                *value /= maximum;
            }
            total = row.iter().sum();
        }
        if maximum > 1.0 || (total - 1.0).abs() > 4.0 * f64::EPSILON {
            for value in row {
                *value /= total;
            }
        }
    }
    Ok(rows)
}

fn parse_hue(input: &str) -> Result<f64, String> {
    let value: f64 = input
        .parse()
        .map_err(|_| "expected a hue rotation in degrees, e.g. 10 or -10".to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("hue rotation must be finite".into())
    }
}

fn parse_dust_strength(input: &str) -> Result<f64, String> {
    let value: f64 = input
        .parse()
        .map_err(|_| "expected a correction strength such as 0.5 or 1.25".to_string())?;
    if value.is_finite() && (0.0..=2.0).contains(&value) {
        Ok(value)
    } else {
        Err("sensor-dirt correction strength must be finite and in [0, 2]".into())
    }
}

fn parse_noise_level(input: &str) -> Result<f64, String> {
    let value: f64 = input
        .parse()
        .map_err(|_| "expected a noise reduction level such as 0.5 or 1.25".to_string())?;
    if value.is_finite() && (0.0..=2.0).contains(&value) {
        Ok(value)
    } else {
        Err("noise reduction level must be finite and in [0, 2]".into())
    }
}

fn parse_color_level(input: &str) -> Result<f64, String> {
    let value: f64 = input
        .parse()
        .map_err(|_| "expected a max-color level such as 0.5 or 1.25".to_string())?;
    if value.is_finite() && (0.0..=2.0).contains(&value) {
        Ok(value)
    } else {
        Err("max-color level must be finite and in [0, 2]".into())
    }
}

fn parse_natural_color_level(input: &str) -> Result<f64, String> {
    let value: f64 = input
        .parse()
        .map_err(|_| "expected a natural-color level such as 0.5 or 1.25".to_string())?;
    if value.is_finite() && (0.0..=2.0).contains(&value) {
        Ok(value)
    } else {
        Err("natural-color level must be finite and in [0, 2]".into())
    }
}

fn normalize_arguments(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    const VALUES: &[&str] = &[
        "output",
        "output-dir",
        "channel-order",
        "channel-mix",
        "cm",
        "hue",
        "dark",
        "light",
        "clip-strength",
        "func",
        "tone-band",
        "transfer",
        "png-transfer",
        "jpeg-transfer",
        "jpg-transfer",
        "jpeg-quality",
        "jpg-quality",
        "heic-mode",
        "avif-mode",
        "j2k-mode",
        "jxl-mode",
        "heic-quality",
        "avif-quality",
        "j2k-quality",
        "jxl-quality",
        "threads",
        "jobs",
        "fix-dirty-list",
        "fdl",
        "fix-dust-strength",
        "fds",
        "noise-reduction-level",
        "nrl",
        "max-color-level",
        "mcl",
        "natural-color-level",
        "ncl",
    ];
    const FLAGS: &[&str] = &[
        "color",
        "channels",
        "g1",
        "g2",
        "ga",
        "bw-from-color",
        "bw",
        "keep-color-ratios",
        "kc",
        "kcr",
        "white-balance",
        "wb",
        "natural-color",
        "nc",
        "max-color",
        "mc",
        "fix-dirty",
        "fd",
        "fix-dirty-extra",
        "fix-dust-extra",
        "fdx",
        "noise-reduction",
        "nr",
        "func-clip",
        "func-scale",
        "func-wrap",
        "optimize",
        "no-optimize",
        "best",
        "both",
        "no-crop",
        "report",
        "analyze",
        "overwrite",
        "help",
        "version",
        "verbose",
        "debug",
        "silent",
        "all",
        "none",
        "png",
        "no-png",
        "jpg",
        "jpeg",
        "no-jpg",
        "no-jpeg",
        "heic",
        "no-heic",
        "avif",
        "no-avif",
        "j2k",
        "no-j2k",
        "j2k-allow-low-bits",
        "jxl",
        "no-jxl",
        "jxl-allow-low-bits",
        "lossless",
        "lossy",
    ];
    let mut positional = false;
    let mut value_next = false;
    args.into_iter()
        .enumerate()
        .flat_map(|(index, arg)| {
            if index == 0 || positional {
                return vec![arg];
            }
            if value_next {
                value_next = false;
                return vec![arg];
            }
            let Some(text) = arg.to_str() else {
                return vec![arg];
            };
            if text == "--" {
                positional = true;
                return vec![arg];
            }
            if let Some(order) = text.strip_prefix("--").or_else(|| text.strip_prefix('-'))
                && order.len() == 3
                && parse_channel_order(order).is_ok()
            {
                return vec![OsString::from("--channel-order"), OsString::from(order)];
            }
            if matches!(text, "-all" | "--all") {
                return [
                    "--all", "--jpg", "--png", "--heic", "--avif", "--j2k", "--jxl",
                ]
                .into_iter()
                .map(OsString::from)
                .collect();
            }
            if matches!(text, "-none" | "--none") {
                return [
                    "--none",
                    "--no-jpg",
                    "--no-png",
                    "--no-heic",
                    "--no-avif",
                    "--no-j2k",
                    "--no-jxl",
                ]
                .into_iter()
                .map(OsString::from)
                .collect();
            }
            let name = text.trim_start_matches('-').split('=').next().unwrap_or("");
            let known_value = VALUES.contains(&name) || name.starts_with("param-");
            if !text.contains('=')
                && (known_value && text.starts_with('-') || matches!(text, "-o" | "-j"))
            {
                value_next = true;
            }
            if text.starts_with('-')
                && !text.starts_with("--")
                && (known_value || FLAGS.contains(&name))
            {
                vec![OsString::from(format!("-{text}"))]
            } else {
                vec![arg]
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct Input {
    pub path: PathBuf,
    pub relative: PathBuf,
}

pub fn discover(paths: &[PathBuf]) -> Result<Vec<Input>> {
    let mut inputs = Vec::new();
    let mut seen = HashSet::new();
    let kind = "DNG/ARW";
    for path in paths {
        let root = path
            .canonicalize()
            .with_context(|| format!("locating input {}", path.display()))?;
        if root.is_dir() {
            let prefix = if paths.len() > 1 {
                PathBuf::from(root.file_name().context("input directory has no name")?)
            } else {
                PathBuf::new()
            };
            let mut found = false;
            for entry in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
                let entry = entry.with_context(|| format!("walking {}", root.display()))?;
                if entry.file_type().is_file() && is_raw(entry.path()) {
                    found = true;
                    if seen.insert(entry.path().to_path_buf()) {
                        inputs.push(Input {
                            relative: prefix.join(entry.path().strip_prefix(&root)?),
                            path: entry.into_path(),
                        });
                    }
                }
            }
            ensure!(found, "no {kind} files in {}", path.display());
        } else {
            ensure!(
                root.is_file() && is_raw(&root),
                "not a {kind} file: {}",
                path.display()
            );
            if seen.insert(root.clone()) {
                inputs.push(Input {
                    relative: PathBuf::from(root.file_name().context("input has no filename")?),
                    path: root,
                });
            }
        }
    }
    ensure!(!inputs.is_empty(), "no {kind} files found");
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inputs)
}

fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dng") || ext.eq_ignore_ascii_case("arw"))
}

fn process_files<T: Send>(
    count: usize,
    threads: usize,
    jobs: usize,
    process: impl Fn(usize) -> T + Sync,
) -> Result<Vec<T>> {
    ensure!(
        count > 0 && threads > 0 && jobs > 0,
        "file scheduling requires positive input, thread and job counts"
    );
    let jobs = jobs.min(count).min(threads);
    let pools = (0..jobs)
        .map(|job| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads / jobs + usize::from(job < threads % jobs))
                .thread_name(move |worker| format!("dng-mono-{job}-{worker}"))
                .build()
                .with_context(|| {
                    format!("creating processing thread pool for file slot {}", job + 1)
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let next = AtomicUsize::new(jobs);
    let (sender, receiver) = mpsc::channel();
    let work = |mut index| {
        while index < count {
            assert!(
                sender.send((index, process(index))).is_ok(),
                "file result receiver must remain alive until processing finishes"
            );
            index = next.fetch_add(1, Ordering::Relaxed);
        }
    };
    // Launch scopes in the caller, leaving every pool worker available for image work.
    fn launch(pools: &[rayon::ThreadPool], index: usize, work: &(impl Fn(usize) + Sync)) {
        if let Some((pool, rest)) = pools.split_first() {
            pool.in_place_scope(|scope| {
                scope.spawn(move |_| work(index));
                launch(rest, index + 1, work);
            });
        }
    }
    launch(&pools, 0, &work);
    drop(sender);
    let mut results: Vec<_> = receiver.into_iter().collect();
    results.sort_unstable_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

#[derive(Default)]
struct FileOutcome {
    successes: usize,
    failures: usize,
    analysis: Option<serde_json::Value>,
}

fn prepare_dirty(cli: &Cli, inputs: &[Input], threads: usize) -> Result<Option<Vec<dirty::Plan>>> {
    if !cli.fix_dirty
        && !cli.fix_dirty_extra
        && cli.fix_dirty_list.is_empty()
        && cli.fix_dust_strength.is_none()
    {
        return Ok(None);
    }
    let separate = (!cli.fix_dirty_list.is_empty())
        .then(|| discover(&cli.fix_dirty_list))
        .transpose()?;
    let references = separate.as_deref().unwrap_or(inputs);
    let parent = references[0].path.parent();
    ensure!(
        references.iter().all(|input| input.path.parent() == parent),
        "--fix-dirty reference RAWs must come from a single directory; run separate directories independently"
    );
    let mut paths = Vec::new();
    let mut indices = HashMap::new();
    for input in references.iter().chain(inputs) {
        if !indices.contains_key(&input.path) {
            indices.insert(input.path.clone(), paths.len());
            paths.push(input.path.as_path());
        }
    }
    if !cli.silent {
        eprintln!(
            "Sensor-dirt prepass: {} reference(s), {} additional target(s); analyzing all {} RAWs before conversion",
            references.len(),
            paths.len() - references.len(),
            paths.len()
        );
    }
    let completed = AtomicUsize::new(0);
    let results = process_files(paths.len(), threads, usize::from(cli.jobs), |index| {
        let result = raw::decode(paths[index], cli.no_crop)
            .and_then(|image| dirty::analyze_with_extra(&image, cli.fix_dirty_extra));
        if !cli.silent {
            let count = completed.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "  dirt analysis [{count}/{}] {}",
                paths.len(),
                paths[index].display()
            );
        }
        result
    })?;
    let mut evidence = Vec::with_capacity(results.len());
    let mut failures = 0;
    for (path, result) in paths.iter().zip(results) {
        match result {
            Ok(frame) => evidence.push(frame),
            Err(error) => {
                failures += 1;
                eprintln!("ERROR sensor-dirt analysis {}: {error:#}", path.display());
            }
        }
    }
    ensure!(
        failures == 0,
        "sensor-dirt prepass failed for {failures} RAW(s); no output files were written"
    );
    let calibration = dirty::Calibration::new(&evidence[..references.len()])?;
    let plans = inputs
        .iter()
        .map(|input| calibration.plan(&evidence[indices[&input.path]]))
        .collect();
    if !cli.silent {
        eprintln!(
            "Sensor-dirt analysis complete for all {} RAWs; starting target processing",
            paths.len()
        );
    }
    Ok(Some(plans))
}

fn dirty_progress(report: &dirty::Report, path: &Path) {
    if let Some(warning) = report.warning {
        eprintln!("WARNING {} [sensor dirt]: {warning}", path.display());
    } else {
        eprintln!(
            "  sensor dirt {}: {} spot(s), strength {:.3}, {} changed sample(s), maximum gain {:.3}",
            path.display(),
            report.spots.len(),
            report.strength,
            report.changed_samples,
            report.maximum_gain
        );
    }
}

fn noise_progress(report: &denoise::Report, path: &Path, channel: Option<&str>) {
    let label = channel.map_or_else(
        || path.display().to_string(),
        |channel| format!("{} [{channel}]", path.display()),
    );
    if report.applied {
        eprintln!(
            "  noise reduction {label}: level {:.3}, {} scales, {} changed sample(s)",
            report.level, report.scales, report.changed_samples
        );
    }
    for warning in &report.warnings {
        eprintln!("WARNING {label} [noise reduction]: {warning}");
    }
}

pub fn run(cli: Cli) -> Result<()> {
    cli.parameters.validate()?;
    cli.formats.validate(cli.analyze)?;
    let range_options = RangeOptions {
        dark: cli.dark,
        light: cli.light,
        clip_strength: cli.clip_strength,
    };
    range_options.validate()?;
    let source = cli.expression_source()?;
    let expression = source
        .as_deref()
        .map(|source| Expression::parse_with_parameters(source, &cli.parameters))
        .transpose()?;
    let inputs = discover(&cli.inputs)?;
    let transfers = cli.transfers();
    let modes = if cli.both {
        vec![false, true]
    } else {
        vec![cli.optimization_enabled()]
    };
    if !cli.analyze {
        let mut destinations = HashSet::new();
        for input in &inputs {
            for &optimized in &modes {
                for paths in color::output_sets(&cli, input, optimized)? {
                    for path in paths.all() {
                        ensure!(
                            destinations.insert(path.to_owned()),
                            "multiple inputs would overwrite the same output: {}",
                            path.display()
                        );
                    }
                    paths.check(cli.overwrite)?;
                }
            }
        }
    }
    let threads = if cli.threads == 0 {
        std::thread::available_parallelism()
            .context("detecting available CPU threads; alternatively set --threads")?
            .get()
    } else {
        usize::from(cli.threads)
    };
    let jobs = usize::from(cli.jobs).min(threads).min(inputs.len());
    let dirty_plans = prepare_dirty(&cli, &inputs, threads)?;
    let results = process_files(
        inputs.len(),
        threads,
        jobs,
        |index| -> Result<FileOutcome> {
            let input = &inputs[index];
            if cli.color {
                return match color::process(
                    &cli,
                    input,
                    index,
                    inputs.len(),
                    threads,
                    jobs,
                    &modes,
                    range_options,
                    expression.as_ref(),
                    dirty_plans.as_ref().map(|plans| &plans[index]),
                ) {
                    Ok(outcome) => Ok(outcome),
                    Err(error) => {
                        eprintln!("ERROR {}: {error:#}", input.path.display());
                        Ok(FileOutcome {
                            failures: 1,
                            ..FileOutcome::default()
                        })
                    }
                };
            }
            let file_threads = rayon::current_num_threads();
            let mut outcome = FileOutcome::default();
            let start = Instant::now();
            let decoded = (|| -> Result<_> {
                let mut image = raw::decode(&input.path, cli.no_crop)?;
                let dirty_report = dirty_plans
                    .as_ref()
                    .map(|plans| {
                        plans[index]
                            .apply_with_strength(&mut image, cli.fix_dust_strength.unwrap_or(1.0))
                    })
                    .transpose()?;
                let hist = Histogram::with_parameters(&image.pixels, &cli.parameters)?;
                let range =
                    range::analyze_with_parameters(&image, &hist, range_options, &cli.parameters)?;
                let noise_reduction = (cli.noise_level() != 0.0)
                    .then(|| denoise::reduce(&image, &range.noise, cli.noise_level()))
                    .transpose()?;
                let encodings = if cli.analyze {
                    Vec::new()
                } else {
                    cli.formats.encodings(range.dynamic_range.snr1_stops)?
                };
                Ok((image, hist, range, encodings, dirty_report, noise_reduction))
            })();
            let (image, hist, range, encodings, dirty_report, noise_reduction) = match decoded {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("ERROR {}: {error:#}", input.path.display());
                    outcome.failures += 1;
                    return Ok(outcome);
                }
            };
            if !cli.silent {
                if let Some(report) = &dirty_report {
                    dirty_progress(report, &input.path);
                }
                if let Some(reduction) = &noise_reduction {
                    noise_progress(&reduction.report, &input.path, None);
                }
                for warning in &range.warnings {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
                for warning in encodings.iter().filter_map(Encoding::warning) {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
            }
            if cli.verbose {
                let mut diagnostic = serde_json::json!({
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                    "parameters": cli.parameters,
                    "threads": threads,
                    "jobs": jobs,
                    "file_threads": file_threads,
                });
                if let Some(report) = &dirty_report {
                    diagnostic["dirty_correction"] = serde_json::to_value(report)?;
                }
                if let Some(reduction) = &noise_reduction {
                    diagnostic["noise_reduction"] = serde_json::to_value(&reduction.report)?;
                }
                eprintln!("{}", serde_json::to_string_pretty(&diagnostic)?);
            }
            if cli.analyze {
                let mut report = serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                    "parameters": cli.parameters,
                    "threads": threads,
                    "jobs": jobs,
                    "file_threads": file_threads,
                });
                if let Some(dirty) = &dirty_report {
                    report["dirty_correction"] = serde_json::to_value(dirty)?;
                }
                if let Some(reduction) = &noise_reduction {
                    report["noise_reduction"] = serde_json::to_value(&reduction.report)?;
                }
                outcome.analysis = Some(report);
                outcome.successes += 1;
                return Ok(outcome);
            }
            for &optimized in &modes {
                let mode = if optimized { "best" } else { "auto" };
                let result = (|| -> Result<()> {
                    let rendered = tone::render_with_noise_reduction(
                        &image,
                        &hist,
                        &range,
                        optimized,
                        expression.as_ref(),
                        cli.policy(),
                        transfers,
                        &cli.parameters,
                        noise_reduction.as_ref(),
                    )?;
                    let paths = output_paths(&cli, input, optimized);
                    let report = ConversionReport {
                        version: env!("CARGO_PKG_VERSION"),
                        source: input.path.to_string_lossy().into_owned(),
                        mode,
                        metadata: &image.metadata,
                        range: &range,
                        tone: &rendered.tone,
                        function: &rendered.function,
                        output: &rendered.stats,
                        transfer: transfers.png,
                        png_transfer: transfers.png,
                        jpeg_transfer: transfers.jpeg,
                        jpeg_quality: cli.jpeg_quality,
                        png_compression: "maximum (DEFLATE level 9, adaptive filtering)",
                        threads,
                        jobs,
                        file_threads,
                        parameters: &cli.parameters,
                        additional_formats: encodings.clone(),
                        dirty_correction: dirty_report.as_ref(),
                        noise_reduction: noise_reduction
                            .as_ref()
                            .map(|reduction| &reduction.report),
                    };
                    let warnings = output::save_with_encodings(
                        &paths,
                        &image,
                        &rendered,
                        &report,
                        transfers,
                        cli.jpeg_quality,
                        cli.overwrite,
                        &cli.parameters,
                        &encodings,
                    )?;
                    if !cli.silent {
                        for warning in warnings {
                            eprintln!("WARNING {}: {warning}", input.path.display());
                        }
                        let dr = range.dynamic_range.snr1_stops.map_or_else(
                            || "unavailable (insufficient noise evidence)".to_owned(),
                            |value| {
                                format!(
                                    "{value:.digits$} bits/stops (noise-limited, {:?} confidence)",
                                    range.dynamic_range.confidence,
                                    digits = cli.parameters.progress_dr_decimals
                                )
                            },
                        );
                        let code_label = if cli.formats.enabled().contains(&Format::Png) {
                            "codes"
                        } else {
                            "master codes"
                        };
                        let mut progress = format!(
                            "[{}/{}] {} [{mode}, strength {}, {file_threads} threads] {}; {:.range_digits$}..{:.range_digits$}, clip {:.clip_digits$}%/{:.clip_digits$}%, approx DR {dr}, raw-code span {:.dr_digits$} bits, {} {code_label} -> {} (elapsed {:.elapsed_digits$}s)",
                            index + 1,
                            inputs.len(),
                            input.relative.display(),
                            cli.clip_strength,
                            exposure_summary(&image, &cli.parameters),
                            range.lower,
                            range.upper,
                            range.clipped_dark_percent,
                            range.clipped_light_percent,
                            range.retained_span_bits,
                            rendered.stats.png_occupied_codes,
                            paths.primary()?.display(),
                            start.elapsed().as_secs_f64(),
                            range_digits = cli.parameters.progress_range_decimals,
                            clip_digits = cli.parameters.progress_clip_decimals,
                            dr_digits = cli.parameters.progress_dr_decimals,
                            elapsed_digits = cli.parameters.progress_elapsed_decimals,
                        );
                        if !encodings.is_empty() {
                            let outputs = encodings
                                .iter()
                                .map(|encoding| {
                                    let quality = encoding
                                        .quality
                                        .map_or_else(String::new, |q| format!(", quality {q}"));
                                    format!(
                                        "{} {}-bit {:?}{quality}",
                                        encoding.format, encoding.bits, encoding.compression
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("; ");
                            progress.push_str(&format!("\n  additional outputs (quantized from the 16-bit master): {outputs}"));
                        }
                        if optimized {
                            let mapping = (1..cli.parameters.mapping_steps)
                                .map(|i| {
                                    let x = i as f64 / cli.parameters.mapping_steps as f64;
                                    format!(
                                        "{x:.input_digits$}->{:.output_digits$}",
                                        rendered.tone.map(x),
                                        input_digits = cli.parameters.mapping_input_decimals,
                                        output_digits = cli.parameters.mapping_output_decimals
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            progress.push_str(&format!("\n  best mapping (normalized linear, before --func and output transfer): {mapping}"));
                        }
                        eprintln!("{progress}");
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => outcome.successes += 1,
                    Err(error) => {
                        eprintln!("ERROR {} [{mode}]: {error:#}", input.path.display());
                        outcome.failures += 1;
                    }
                }
            }
            Ok(outcome)
        },
    )?;
    let mut successes = 0usize;
    let mut failures = 0usize;
    for result in results {
        let result = result?;
        successes += result.successes;
        failures += result.failures;
        if let Some(report) = result.analysis {
            let mut stdout = io::stdout().lock();
            serde_json::to_writer(&mut stdout, &report)?;
            writeln!(stdout)?;
        }
    }
    ensure!(
        failures == 0,
        "{failures} input/variant(s) failed; {successes} completed successfully"
    );
    if !cli.analyze && !cli.silent {
        writeln!(
            io::stdout().lock(),
            "Saved {successes} {} from {} {} using {threads} threads ({jobs} files at once) to {}",
            cli.formats.summary(),
            inputs.len(),
            if cli.color { "color raw(s)" } else { "raw(s)" },
            cli.output.display()
        )?;
    }
    Ok(())
}

fn exposure_summary(image: &raw::MonoImage, parameters: &Parameters) -> String {
    let metadata = &image.metadata;
    let iso = metadata
        .iso
        .map_or_else(|| "unknown".into(), |v| v.to_string());
    let aperture = metadata.aperture_f_number.map_or_else(
        || {
            metadata.aperture_value_apex.map_or_else(
                || "f/unknown".into(),
                |v| {
                    format!(
                        "f/{:.digits$} (APEX)",
                        (v * 0.5).exp2(),
                        digits = parameters.progress_aperture_decimals
                    )
                },
            )
        },
        |v| {
            format!(
                "f/{v:.digits$}",
                digits = parameters.progress_aperture_decimals
            )
        },
    );
    let time = image.source_metadata.exif.exposure_time.map_or_else(
        || "unknown".into(),
        |v| {
            if v.d == 1 {
                format!("{}s", v.n)
            } else {
                format!("{}/{}s", v.n, v.d)
            }
        },
    );
    format!("ISO {iso}, {aperture}, t {time}")
}

fn output_paths(cli: &Cli, input: &Input, optimized: bool) -> OutputPaths {
    let root = if cli.both {
        cli.output.join(if optimized { "best" } else { "auto" })
    } else {
        cli.output.clone()
    };
    OutputPaths::with_formats(
        &root.join(&input.relative),
        cli.report,
        &cli.formats.enabled(),
    )
}

#[derive(Serialize)]
struct ConversionReport<'a> {
    version: &'static str,
    source: String,
    mode: &'static str,
    metadata: &'a raw::SensorMetadata,
    range: &'a range::RangeAnalysis,
    tone: &'a tone::ToneCurve,
    function: &'a crate::expression::FunctionReport,
    output: &'a tone::RenderStats,
    transfer: Transfer,
    png_transfer: Transfer,
    jpeg_transfer: Transfer,
    jpeg_quality: u8,
    png_compression: &'static str,
    threads: usize,
    jobs: usize,
    file_threads: usize,
    parameters: &'a Parameters,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_formats: Vec<Encoding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dirty_correction: Option<&'a dirty::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    noise_reduction: Option<&'a denoise::Report>,
}

#[cfg(test)]
mod scheduling_tests {
    use super::process_files;
    use std::{
        collections::HashSet,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[test]
    fn worker_budget_is_divided_between_capped_file_slots() {
        for (threads, jobs, expected) in [
            (16, 4, vec![16]),
            (16, 4, vec![8, 8]),
            (16, 4, vec![6, 5, 5]),
            (16, 4, vec![4; 16]),
            (4, 1, vec![4; 7]),
            (3, 4, vec![1; 7]),
            (1, 4, vec![1; 3]),
        ] {
            let actual = process_files(expected.len(), threads, jobs, |_| {
                rayon::current_num_threads()
            })
            .unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn files_and_their_nested_workers_run_concurrently_without_oversubscription() {
        let arrived = (Mutex::new(0), Condvar::new());
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let results = process_files(6, 16, 2, |index| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            assert_eq!(rayon::current_num_threads(), 8);
            let workers = rayon::broadcast(|_| {
                if index < 2 {
                    let (lock, condition) = &arrived;
                    let mut count = lock.lock().unwrap();
                    *count += 1;
                    condition.notify_all();
                    let (count, _) = condition
                        .wait_timeout_while(count, Duration::from_secs(10), |n| *n < 16)
                        .unwrap();
                    assert_eq!(*count, 16, "all workers must be able to run together");
                }
                std::thread::current().id()
            });
            active.fetch_sub(1, Ordering::SeqCst);
            (index, workers)
        })
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let mut workers = HashSet::new();
        for (index, (actual, ids)) in results.into_iter().enumerate() {
            assert_eq!(actual, index);
            assert_eq!(ids.len(), 8);
            workers.extend(ids);
        }
        assert_eq!(workers.len(), 16);
    }

    #[test]
    fn queued_files_are_processed_once_and_errors_remain_in_input_order() {
        let calls: Vec<_> = (0..19).map(|_| AtomicUsize::new(0)).collect();
        let results = process_files(calls.len(), 4, 2, |index| {
            assert_eq!(calls[index].fetch_add(1, Ordering::Relaxed), 0);
            assert_eq!(rayon::current_num_threads(), 2);
            if index % 3 == 0 {
                Err(index)
            } else {
                Ok(index)
            }
        })
        .unwrap();
        for (index, result) in results.into_iter().enumerate() {
            assert_eq!(
                result,
                if index % 3 == 0 {
                    Err(index)
                } else {
                    Ok(index)
                }
            );
            assert_eq!(calls[index].load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn scheduler_rejects_invalid_budgets_and_propagates_worker_panics() {
        for (count, threads, jobs) in [(0, 1, 1), (1, 0, 1), (1, 1, 0)] {
            assert!(process_files(count, threads, jobs, |_| unreachable!()).is_err());
        }
        let finished = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| {
            process_files(2, 2, 2, |index| {
                if index == 0 {
                    panic!("worker failure");
                }
                finished.store(true, Ordering::SeqCst);
            })
        });
        assert!(result.is_err());
        assert!(finished.load(Ordering::SeqCst));
    }
}
