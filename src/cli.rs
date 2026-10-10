use crate::{
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
    collections::HashSet,
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

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Develop monochrome DNGs into 16-bit PNG, with optional JPEG, HEIC, AVIF and JPEG2000.",
    long_about = "Develop integer monochrome DNGs without demosaicing. Directories are searched recursively. \
        Automatic range detection estimates sparse histogram tails and spatial noise; it cannot measure \
        true sensor dynamic range or recover clipped detail. PNGs are always 16-bit grayscale with \
        maximum lossless compression; only PNG is saved by default. --jpg restores the original \
        8-bit JPEG companion. HEIC/AVIF use 8/10/12 bits; JPEG2000 uses 1-16 bits, automatically \
        selected from the existing noise-measured DR, capped at the codec maximum. \
        New formats default to lossless compression of the selected-depth samples and share the \
        developed PNG transfer. All formats carry photographic EXIF. Optimization is on by default, \
        clipping strength is 3, PNG and JPEG transfers are both linear. \
        Tunable constants are exposed as --param-* options.",
    after_help = "Single-dash long options also work: -dark 0.8 -light 1.2% -clip-strength 9 -func 'x^.5' -best.\n\
        Pipeline: crop/orient -> range stretch -> optimize (unless --no-optimize) -> function/tone-band -> policy -> transfers.\n\
        x is normalized LINEAR light in [0,1], before the final display transfer.\n\
        Examples:\n  \
        dng-monochrome photos/ -o dng-mono --both --report\n  \
        dng-monochrome shot.DNG -clip-strength 9 -best\n  \
        dng-monochrome shot.DNG -dark 0.8 -light 1.2% -func 'sin(pi*x)^2' -func-scale\n  \
        dng-monochrome shot.DNG --no-optimize --dark 0 --light 0\n  \
        dng-monochrome shot.DNG -tone-band 0:0.35:0.4\n  \
        dng-monochrome shot.DNG -jpg --png-transfer linear --jpeg-transfer srgb\n  \
        dng-monochrome shot.DNG -no-png -heic -avif -j2k\n  \
        dng-monochrome shot.DNG -heic -heic-mode lossy -heic-quality 95\n\
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
        help = "Write auto/ and best/ versions, decoding each DNG only once"
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
        help = "PNG/master transfer, also used by HEIC/AVIF/JPEG2000; default linear, overrides --transfer"
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

fn normalize_arguments(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    const VALUES: &[&str] = &[
        "output",
        "output-dir",
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
        "heic-quality",
        "avif-quality",
        "j2k-quality",
        "threads",
        "jobs",
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
        "lossless",
        "lossy",
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
    let jobs = usize::from(cli.jobs).min(threads).min(inputs.len());
    let results = process_files(
        inputs.len(),
        threads,
        jobs,
        |index| -> Result<FileOutcome> {
            let input = &inputs[index];
            let file_threads = rayon::current_num_threads();
            let mut outcome = FileOutcome::default();
            let start = Instant::now();
            let decoded = (|| -> Result<_> {
                let image = raw::decode(&input.path, cli.no_crop)?;
                let hist = Histogram::with_parameters(&image.pixels, &cli.parameters)?;
                let range =
                    range::analyze_with_parameters(&image, &hist, range_options, &cli.parameters)?;
                let encodings = if cli.analyze {
                    Vec::new()
                } else {
                    cli.formats.encodings(range.dynamic_range.snr1_stops)?
                };
                Ok((image, hist, range, encodings))
            })();
            let (image, hist, range, encodings) = match decoded {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("ERROR {}: {error:#}", input.path.display());
                    outcome.failures += 1;
                    return Ok(outcome);
                }
            };
            if !cli.silent {
                for warning in &range.warnings {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
                for warning in encodings.iter().filter_map(Encoding::warning) {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
            }
            if cli.verbose {
                let diagnostic = serde_json::json!({
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                    "parameters": cli.parameters,
                    "threads": threads,
                    "jobs": jobs,
                    "file_threads": file_threads,
                });
                eprintln!("{}", serde_json::to_string_pretty(&diagnostic)?);
            }
            if cli.analyze {
                let report = serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "source": input.path.to_string_lossy(),
                    "metadata": image.metadata,
                    "range": range,
                    "parameters": cli.parameters,
                    "threads": threads,
                    "jobs": jobs,
                    "file_threads": file_threads,
                });
                outcome.analysis = Some(report);
                outcome.successes += 1;
                return Ok(outcome);
            }
            for &optimized in &modes {
                let mode = if optimized { "best" } else { "auto" };
                let result = (|| -> Result<()> {
                    let rendered = tone::render_with_parameters(
                        &image,
                        &hist,
                        &range,
                        optimized,
                        expression.as_ref(),
                        cli.policy(),
                        transfers,
                        &cli.parameters,
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
            "Saved {successes} {} from {} DNG(s) using {threads} threads ({jobs} files at once) to {}",
            cli.formats.summary(),
            inputs.len(),
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
