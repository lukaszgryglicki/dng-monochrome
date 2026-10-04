use crate::{
    expression::{Expression, FunctionPolicy},
    output::{self, OutputPaths},
    range::{self, Histogram, RangeOptions},
    raw,
    tone::{self, Transfer, Transfers},
};
use anyhow::{Context, Result, ensure};
use clap::{ArgGroup, Parser};
use serde::Serialize;
use std::{
    collections::HashSet,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Develop monochrome DNGs into full-precision 16-bit grayscale PNGs and display-matched JPEGs.",
    long_about = "Develop integer monochrome DNGs without demosaicing. Directories are searched recursively. \
        Automatic range detection estimates sparse histogram tails and spatial noise; it cannot measure \
        true sensor dynamic range or recover clipped detail. PNGs are always 16-bit grayscale with \
        maximum lossless compression and default linear transfer; JPEGs are display-encoded 8-bit grayscale. \
        Both carry photographic EXIF. Optimization is on by default, clipping strength is 3, \
        PNG transfer is linear and JPEG transfer is sRGB.",
    after_help = "Single-dash long options also work: -dark 0.8 -light 1.2% -clip-strength 9 -func 'x^.5' -best.\n\
        Pipeline: crop/orient -> range stretch -> optional optimize -> function -> boundary policy -> transfer.\n\
        x is normalized LINEAR light in [0,1], before the final display transfer.\n\
        Examples:\n  \
        dng-monochrome photos/ -o dng-mono --both --report\n  \
        dng-monochrome shot.DNG -clip-strength 9 -best\n  \
        dng-monochrome shot.DNG -dark 0.8 -light 1.2% -func 'sin(pi*x)^2' -func-scale\n  \
        dng-monochrome shot.DNG --dark 0 --light 0 --transfer linear\n\
        Expressions: + - * / % ^, unary +/- and parentheses; pi, e; sqrt, abs, exp, ln, log,\n\
        log2, log10, log1p, exp2, expm1, sin/cos/tan, asin/acos/atan/atan2, sinh/cosh/tanh,\n\
        asinh/acosh/atanh, floor/ceil/round, sign/signum, min/max, pow, hypot, clamp.\n\
        Powers are right-associative; trig uses radians; write multiplication explicitly.\n\
        NaN/infinity are errors even with clipping. See README for algorithms and limitations.",
    group(ArgGroup::new("function-policy").args(["func_clip", "func_scale", "func_wrap"]))
)]
pub struct Cli {
    #[arg(required = true, num_args = 1.., value_name = "DNG_OR_DIRECTORY")]
    pub inputs: Vec<PathBuf>,

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
        long,
        requires = "function",
        help = "Clamp finite function results to [0,1] (default policy)"
    )]
    pub func_clip: bool,

    #[arg(
        long,
        requires = "function",
        help = "Scale actual function min/max to [0,1]; constant results are errors"
    )]
    pub func_scale: bool,

    #[arg(
        long,
        requires = "function",
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
        help = "Write auto/ and best/ versions, decoding each DNG only once"
    )]
    pub both: bool,

    #[arg(
        long,
        value_enum,
        help = "Set BOTH PNG and JPEG transfer; format-specific options take precedence"
    )]
    pub transfer: Option<Transfer>,

    #[arg(
        long,
        value_enum,
        help = "PNG transfer; default linear, overrides --transfer"
    )]
    pub png_transfer: Option<Transfer>,

    #[arg(
        long,
        visible_alias = "jpg-transfer",
        value_enum,
        help = "JPEG transfer; default srgb, overrides --transfer; linear has coarser shadows"
    )]
    pub jpeg_transfer: Option<Transfer>,

    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u8).range(1..=100),
        help = "Grayscale JPEG quality, 1-100")]
    pub jpeg_quality: u8,

    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u16).range(0..=256),
        help = "Worker threads; 0 detects available CPU parallelism (files are processed sequentially)")]
    pub threads: u16,

    #[arg(
        long,
        help = "Keep the full raw raster instead of the recommended crop; orientation still applies"
    )]
    pub no_crop: bool,

    #[arg(
        long,
        help = "Write a JSON sidecar per PNG/JPEG pair with ranges, noise estimates and settings"
    )]
    pub report: bool,

    #[arg(long, conflicts_with_all = ["optimize", "no_optimize", "both", "function", "report", "overwrite"],
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
}

impl Cli {
    pub fn try_parse_compat(args: impl IntoIterator<Item = OsString>) -> Result<Self, clap::Error> {
        Self::try_parse_from(normalize_arguments(args))
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

fn normalize_arguments(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    const VALUES: &[&str] = &[
        "output",
        "output-dir",
        "dark",
        "light",
        "clip-strength",
        "func",
        "transfer",
        "png-transfer",
        "jpeg-transfer",
        "jpg-transfer",
        "jpeg-quality",
        "threads",
    ];
    const FLAGS: &[&str] = &[
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
    ];
    let mut positional = false;
    let mut value_next = false;
    args.into_iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 || positional {
                return arg;
            }
            if value_next {
                value_next = false;
                return arg;
            }
            let Some(text) = arg.to_str() else {
                return arg;
            };
            if text == "--" {
                positional = true;
                return arg;
            }
            let name = text.trim_start_matches('-').split('=').next().unwrap_or("");
            let known_value = VALUES.contains(&name);
            if !text.contains('=') && (known_value && text.starts_with('-') || text == "-o") {
                value_next = true;
            }
            if text.starts_with('-')
                && !text.starts_with("--")
                && (known_value || FLAGS.contains(&name))
            {
                OsString::from(format!("-{text}"))
            } else {
                arg
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
                if entry.file_type().is_file() && is_dng(entry.path()) {
                    found = true;
                    if seen.insert(entry.path().to_path_buf()) {
                        inputs.push(Input {
                            relative: prefix.join(entry.path().strip_prefix(&root)?),
                            path: entry.into_path(),
                        });
                    }
                }
            }
            ensure!(found, "no DNG files in {}", path.display());
        } else {
            ensure!(
                root.is_file() && is_dng(&root),
                "not a DNG file: {}",
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
    ensure!(!inputs.is_empty(), "no DNG files found");
    inputs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(inputs)
}

fn is_dng(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dng"))
}

pub fn run(cli: Cli) -> Result<()> {
    let range_options = RangeOptions {
        dark: cli.dark,
        light: cli.light,
        clip_strength: cli.clip_strength,
    };
    range_options.validate()?;
    let expression = cli.function.as_deref().map(Expression::parse).transpose()?;
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
                let paths = output_paths(&cli, input, optimized);
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
    let threads = if cli.threads == 0 {
        std::thread::available_parallelism()
            .context("detecting available CPU threads; alternatively set --threads")?
            .get()
    } else {
        usize::from(cli.threads)
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("dng-mono-{i}"))
        .build()
        .context("creating processing thread pool")?;
    pool.install(|| {
        let mut successes = 0usize;
        let mut failures = 0usize;
        for (index, input) in inputs.iter().enumerate() {
            let start = Instant::now();
            let decoded = (|| -> Result<_> {
                let image = raw::decode(&input.path, cli.no_crop)?;
                let hist = Histogram::new(&image.pixels)?;
                let range = range::analyze(&image, &hist, range_options)?;
                Ok((image, hist, range))
            })();
            let (image, hist, range) = match decoded {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("ERROR {}: {error:#}", input.path.display());
                    failures += 1;
                    continue;
                }
            };
            if !cli.silent {
                for warning in &range.warnings {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
            }
            if cli.verbose {
                let diagnostic = serde_json::json!({
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                });
                eprintln!("{}", serde_json::to_string_pretty(&diagnostic)?);
            }
            if cli.analyze {
                let report = serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                });
                serde_json::to_writer(io::stdout().lock(), &report)?;
                writeln!(io::stdout().lock())?;
                successes += 1;
                continue;
            }
            for &optimized in &modes {
                let mode = if optimized { "best" } else { "auto" };
                let result = (|| -> Result<()> {
                    let rendered = tone::render(
                        &image, &hist, &range, optimized, expression.as_ref(), cli.policy(), transfers,
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
                    };
                    output::save(
                        &paths, &image, &rendered, &report, transfers,
                        cli.jpeg_quality, cli.overwrite,
                    )?;
                    if !cli.silent {
                        let dr = range.dynamic_range.snr1_stops.map_or_else(
                            || "unavailable (insufficient noise evidence)".to_owned(),
                            |value| format!("{value:.1} bits/stops (noise-limited, {:?} confidence)",
                                range.dynamic_range.confidence),
                        );
                        eprintln!(
                        "[{}/{}] {} [{mode}, strength {}] {}; {:.1}..{:.1}, clip {:.3}%/{:.3}%, approx DR {dr}, raw-code span {:.1} bits, {} codes -> {} (elapsed {:.1}s)",
                        index + 1, inputs.len(), input.relative.display(), cli.clip_strength, exposure_summary(&image), range.lower, range.upper,
                        range.clipped_dark_percent, range.clipped_light_percent,
                        range.retained_span_bits, rendered.stats.png_occupied_codes, paths.png.display(), start.elapsed().as_secs_f64(),
                        );
                        if optimized {
                            let mapping = (1..20).map(|i| {
                                let x = f64::from(i) / 20.0;
                                format!("{x:.2}->{:.5}", rendered.tone.map(x))
                            }).collect::<Vec<_>>().join(", ");
                            eprintln!("  best mapping (normalized linear, before --func and output transfer): {mapping}");
                        }
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => successes += 1,
                    Err(error) => {
                        eprintln!("ERROR {} [{mode}]: {error:#}", input.path.display());
                        failures += 1;
                    }
                }
            }
        }
        ensure!(
            failures == 0,
            "{failures} input/variant(s) failed; {successes} completed successfully"
        );
        if !cli.analyze && !cli.silent {
            writeln!(
                io::stdout().lock(),
                "Saved {successes} PNG/JPEG pair(s) from {} DNG(s) using {threads} threads to {}",
                inputs.len(), cli.output.display()
            )?;
        }
        Ok(())
    })
}

fn exposure_summary(image: &raw::MonoImage) -> String {
    let metadata = &image.metadata;
    let iso = metadata
        .iso
        .map_or_else(|| "unknown".into(), |v| v.to_string());
    let aperture = metadata.aperture_f_number.map_or_else(
        || {
            metadata.aperture_value_apex.map_or_else(
                || "f/unknown".into(),
                |v| format!("f/{:.2} (APEX)", (v * 0.5).exp2()),
            )
        },
        |v| format!("f/{v:.2}"),
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
    OutputPaths::new(&root.join(&input.relative), cli.report)
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
}
