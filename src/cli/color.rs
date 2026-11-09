use super::*;
use crate::color::{self, Channel, Green};
use rayon::prelude::*;
use std::{borrow::Cow, collections::BTreeMap};

struct Plane {
    channel: Channel,
    image: raw::MonoImage,
    histogram: Histogram,
    range: range::RangeAnalysis,
    encodings: Vec<Encoding>,
    dirty: Option<dirty::Report>,
}

fn channel_paths(
    cli: &Cli,
    input: &Input,
    optimized: bool,
    channel: Channel,
) -> Result<OutputPaths> {
    let mut name = input
        .relative
        .file_stem()
        .context("raw input has no filename stem")?
        .to_os_string();
    name.push(channel.name());
    if let Some(extension) = input.relative.extension() {
        name.push(".");
        name.push(extension);
    }
    let relative = input.relative.with_file_name(name);
    Ok(output_paths(
        cli,
        &Input {
            path: input.path.clone(),
            relative,
        },
        optimized,
    ))
}

pub(super) fn output_sets(cli: &Cli, input: &Input, optimized: bool) -> Result<Vec<OutputPaths>> {
    let mut paths = vec![output_paths(cli, input, optimized)];
    if cli.color && cli.channels {
        for &channel in cli.green().channels(true) {
            paths.push(channel_paths(cli, input, optimized, channel)?);
        }
    }
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process(
    cli: &Cli,
    input: &Input,
    index: usize,
    count: usize,
    threads: usize,
    jobs: usize,
    modes: &[bool],
    range_options: RangeOptions,
    expression: Option<&Expression>,
    dirty_plan: Option<&dirty::Plan>,
) -> Result<FileOutcome> {
    let start = Instant::now();
    let green = cli.green();
    let decoded = color::decode_with_balance(
        &input.path,
        cli.no_crop,
        green,
        cli.channels,
        cli.keep_color_ratios,
        cli.white_balance,
    )?;
    let scales = decoded
        .color_ratios
        .map(|ratios| ratios.scale_factors)
        .or_else(|| decoded.white_balance.map(|wb| wb.scale_factors));
    let planes: Vec<Plane> = decoded
        .channels
        .into_par_iter()
        .map(|(channel, mut image)| -> Result<_> {
            let dirty = dirty_plan
                .map(|plan| {
                    plan.apply_with_strength(&mut image, cli.fix_dust_strength.unwrap_or(1.0))
                })
                .transpose()?;
            let histogram = Histogram::with_parameters(&image.pixels, &cli.parameters)?;
            let range =
                range::analyze_with_parameters(&image, &histogram, range_options, &cli.parameters)?;
            let encodings = if cli.analyze {
                Vec::new()
            } else {
                cli.formats.encodings(range.dynamic_range.snr1_stops)?
            };
            Ok(Plane {
                channel,
                image,
                histogram,
                range,
                encodings,
                dirty,
            })
        })
        .collect::<Result<_>>()?;
    let find = |channel| {
        planes
            .iter()
            .position(|plane| plane.channel == channel)
            .context("missing developed Bayer channel")
    };
    let selected = [
        find(Channel::R)?,
        find(if green == Green::G2 {
            Channel::G2
        } else {
            Channel::G1
        })?,
        find(Channel::B)?,
    ];
    let second_green = (green == Green::Average)
        .then(|| find(Channel::G2))
        .transpose()?;
    let file_threads = rayon::current_num_threads();
    let first = &planes[selected[0]].image;
    let dirty_report = planes[selected[0]].dirty.clone().map(|mut report| {
        report.changed_samples = planes
            .iter()
            .filter_map(|plane| plane.dirty.as_ref())
            .map(|report| report.changed_samples)
            .sum();
        report.clipped_samples = planes
            .iter()
            .filter_map(|plane| plane.dirty.as_ref())
            .map(|report| report.clipped_samples)
            .sum();
        report.maximum_gain = planes
            .iter()
            .filter_map(|plane| plane.dirty.as_ref())
            .map(|report| report.maximum_gain)
            .fold(1.0, f64::max);
        report
    });
    let natural_reference_mask = if cli.natural_color {
        decoded
            .recorded_white_balance
            .map(|wb| {
                color::NaturalColor::raw_neutral_mask(
                    selected.map(|index| &planes[index].image),
                    second_green.map(|index| &planes[index].image),
                    wb,
                )
            })
            .transpose()?
    } else {
        None
    };
    let analysis_channels: BTreeMap<_, _> = planes
        .iter()
        .map(|plane| {
            (
                plane.channel.name(),
                serde_json::json!({ "metadata": plane.image.metadata, "range": plane.range }),
            )
        })
        .collect();
    let mut analysis = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"), "source": input.path.to_string_lossy(),
        "color": true, "cfa": decoded.cfa, "green": green,
        "cfa_assumed": decoded.cfa_assumed,
        "keep_color_ratios": cli.keep_color_ratios, "color_ratios": decoded.color_ratios,
        "white_balance": decoded.white_balance,
        "natural_color": cli.natural_color,
        "width": first.metadata.width, "height": first.metadata.height,
        "channels": analysis_channels, "parameters": cli.parameters,
        "threads": threads, "jobs": jobs, "file_threads": file_threads,
    });
    if let Some(report) = &dirty_report {
        analysis["dirty_correction"] = serde_json::to_value(report)?;
    }
    if cli.verbose {
        eprintln!("{}", serde_json::to_string_pretty(&analysis)?);
    }
    if !cli.silent {
        if let Some(report) = &dirty_report {
            dirty_progress(report, &input.path);
        }
        if decoded.cfa_assumed {
            eprintln!(
                "WARNING {}: no CFA layout; --color assumes RGGB at the raw origin",
                input.path.display()
            );
        }
        if cli.keep_color_ratios && decoded.color_ratios.is_none() {
            eprintln!(
                "WARNING {}: raw color ratios unavailable (all reference signals are zero); RGB is unchanged",
                input.path.display()
            );
        }
        for plane in &planes {
            for warning in plane
                .range
                .warnings
                .iter()
                .cloned()
                .chain(plane.encodings.iter().filter_map(Encoding::warning))
            {
                eprintln!(
                    "WARNING {} [{}]: {warning}",
                    input.path.display(),
                    plane.channel.name()
                );
            }
        }
    }
    if cli.analyze {
        return Ok(FileOutcome {
            successes: 1,
            analysis: Some(analysis),
            ..FileOutcome::default()
        });
    }
    let measured_dr = selected
        .into_iter()
        .chain(second_green)
        .map(|index| planes[index].range.dynamic_range.snr1_stops)
        .collect::<Option<Vec<_>>>()
        .map(|values| values.into_iter().fold(0.0, f64::max));
    let encodings = cli.formats.encodings(measured_dr)?;
    let transfers = cli.transfers();
    let mut outcome = FileOutcome::default();
    for &optimized in modes {
        let mode = if optimized { "best" } else { "auto" };
        let result = (|| -> Result<()> {
            let rendered: Vec<_> = planes
                .par_iter()
                .map(|plane| {
                    tone::render_with_parameters(
                        &plane.image,
                        &plane.histogram,
                        &plane.range,
                        optimized,
                        expression,
                        cli.policy(),
                        transfers,
                        &cli.parameters,
                    )
                })
                .collect::<Result<_>>()?;
            let rgb = selected.map(|index| &rendered[index]);
            let other = second_green.map(|index| &rendered[index]);
            let enabled = cli.formats.enabled();
            let natural_balance = if cli.natural_color {
                let (pixels, _) = color::combine(
                    rgb.map(|rendered| (rendered.png.as_slice(), 16)),
                    other.map(|rendered| (rendered.png.as_slice(), 16)),
                )?;
                color::NaturalColor::from_rgb_with_neutral_mask(
                    &pixels,
                    natural_reference_mask.as_deref(),
                )?
            } else {
                None
            };
            let natural_jpeg_balance = if cli.natural_color && enabled.contains(&Format::Jpeg) {
                if transfers.png == transfers.jpeg {
                    natural_balance
                } else {
                    let pixels = color::combine_jpeg(
                        rgb.map(|rendered| rendered.jpeg.as_slice()),
                        other.map(|rendered| rendered.jpeg.as_slice()),
                        None,
                    )?;
                    let pixels: Vec<_> = pixels
                        .into_par_iter()
                        .map(|value| u16::from(value) * 257)
                        .collect();
                    color::NaturalColor::from_rgb_with_neutral_mask(
                        &pixels,
                        natural_reference_mask.as_deref(),
                    )?
                }
            } else {
                None
            };
            let scales = if cli.natural_color {
                natural_balance.map(|balance| balance.scale_factors)
            } else {
                scales
            };
            let jpeg_scales = if cli.natural_color {
                natural_jpeg_balance.map(|balance| balance.scale_factors)
            } else {
                scales
            };
            if cli.natural_color && !cli.silent {
                if natural_balance.is_none() {
                    eprintln!(
                        "WARNING {}: no unclipped RGB reference samples for natural white balance; RGB is unchanged",
                        input.path.display()
                    );
                } else if enabled.contains(&Format::Jpeg) && natural_jpeg_balance.is_none() {
                    eprintln!(
                        "WARNING {}: no unclipped JPEG reference samples for natural white balance; JPEG is unchanged",
                        input.path.display()
                    );
                }
            }
            let reports: Vec<_> = planes
                .iter()
                .zip(&rendered)
                .map(|(plane, rendered)| ConversionReport {
                    version: env!("CARGO_PKG_VERSION"),
                    source: input.path.to_string_lossy().into_owned(),
                    mode,
                    metadata: &plane.image.metadata,
                    range: &plane.range,
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
                    additional_formats: plane.encodings.clone(),
                    dirty_correction: plane.dirty.as_ref(),
                })
                .collect();
            let channel_reports: BTreeMap<_, _> = planes
                .iter()
                .zip(&reports)
                .map(|(plane, report)| (plane.channel.name(), report))
                .collect();
            let mut report = serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"), "source": input.path.to_string_lossy(), "mode": mode,
                "color": true, "cfa": decoded.cfa, "green": green,
                "cfa_assumed": decoded.cfa_assumed,
                "keep_color_ratios": cli.keep_color_ratios, "color_ratios": decoded.color_ratios,
                "white_balance": decoded.white_balance,
                "natural_color": cli.natural_color, "natural_balance": natural_balance,
                "natural_jpeg_balance": natural_jpeg_balance,
                "natural_reference_white_balance": cli.natural_color.then_some(decoded.recorded_white_balance).flatten(),
                "width": first.metadata.width, "height": first.metadata.height,
                "precision_alignment": "left_shift", "channels": channel_reports,
                "parameters": cli.parameters, "png_transfer": transfers.png, "jpeg_transfer": transfers.jpeg,
                "jpeg_quality": cli.jpeg_quality,
                "additional_formats": encodings, "threads": threads, "jobs": jobs, "file_threads": file_threads,
            });
            if let Some(dirty) = &dirty_report {
                report["dirty_correction"] = serde_json::to_value(dirty)?;
            }
            let png = if enabled.contains(&Format::Png) {
                color::combine_with_scales(
                    rgb.map(|rendered| (rendered.png.as_slice(), 16)),
                    other.map(|r| (r.png.as_slice(), 16)),
                    scales.as_ref(),
                )?
                .0
            } else {
                Vec::new()
            };
            let jpeg = if enabled.contains(&Format::Jpeg) {
                color::combine_jpeg(
                    rgb.map(|rendered| rendered.jpeg.as_slice()),
                    other.map(|r| r.jpeg.as_slice()),
                    jpeg_scales.as_ref(),
                )?
            } else {
                Vec::new()
            };
            let paths = output_paths(cli, input, optimized);
            let mut warnings = output::save_components_with_encodings(
                &paths,
                first,
                &png,
                &jpeg,
                3,
                &report,
                transfers,
                cli.jpeg_quality,
                cli.overwrite,
                &cli.parameters,
                &encodings,
                |encoding| {
                    let samples = |index: usize| -> Result<_> {
                        let depth = planes[index]
                            .encodings
                            .iter()
                            .find(|plan| plan.format == encoding.format)
                            .context("missing channel encoding plan")?
                            .bits;
                        Ok((rendered[index].png.as_slice(), depth))
                    };
                    let (pixels, bits) = color::combine_with_scales(
                        [
                            samples(selected[0])?,
                            samples(selected[1])?,
                            samples(selected[2])?,
                        ],
                        second_green.map(samples).transpose()?,
                        scales.as_ref(),
                    )?;
                    ensure!(
                        bits == encoding.bits,
                        "RGB precision must equal the maximum selected channel precision"
                    );
                    Ok(Cow::Owned(pixels))
                },
            )?;
            if cli.channels {
                for ((plane, rendered), report) in planes.iter().zip(&rendered).zip(&reports) {
                    let paths = channel_paths(cli, input, optimized, plane.channel)?;
                    warnings.extend(output::save_with_encodings(
                        &paths,
                        &plane.image,
                        rendered,
                        report,
                        transfers,
                        cli.jpeg_quality,
                        cli.overwrite,
                        &cli.parameters,
                        &plane.encodings,
                    )?);
                }
            }
            if !cli.silent {
                for warning in warnings {
                    eprintln!("WARNING {}: {warning}", input.path.display());
                }
                let mut progress = format!(
                    "[{}/{}] {} [{mode}, color {green:?}, {file_threads} threads] {}; {}x{} RGB -> {} (elapsed {:.1}s)",
                    index + 1,
                    count,
                    input.relative.display(),
                    exposure_summary(first, &cli.parameters),
                    first.metadata.width,
                    first.metadata.height,
                    paths.primary()?.display(),
                    start.elapsed().as_secs_f64()
                );
                if let Some(scales) = scales {
                    progress.push_str(&format!(
                        "\n  {} color scale R {:.6}, G {:.6}, B {:.6}",
                        if cli.natural_color {
                            "natural"
                        } else if cli.white_balance {
                            "WB"
                        } else {
                            "raw"
                        },
                        scales[0],
                        scales[1],
                        scales[2]
                    ));
                }
                if cli.natural_color
                    && transfers.jpeg != transfers.png
                    && let Some(scales) = jpeg_scales
                {
                    progress.push_str(&format!(
                        "\n  natural JPEG color scale R {:.6}, G {:.6}, B {:.6}",
                        scales[0], scales[1], scales[2]
                    ));
                }
                for (plane, rendered) in planes.iter().zip(&rendered) {
                    let dr = plane.range.dynamic_range.snr1_stops.map_or_else(
                        || "unavailable".into(),
                        |dr| {
                            format!(
                                "{dr:.digits$}",
                                digits = cli.parameters.progress_dr_decimals
                            )
                        },
                    );
                    progress.push_str(&format!("\n  {}: {:.range_digits$}..{:.range_digits$}, clip {:.clip_digits$}%/{:.clip_digits$}%, approx DR {dr}",
                        plane.channel.name(), plane.range.lower, plane.range.upper,
                        plane.range.clipped_dark_percent, plane.range.clipped_light_percent,
                        range_digits = cli.parameters.progress_range_decimals, clip_digits = cli.parameters.progress_clip_decimals));
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
                        progress.push_str(&format!("\n    best mapping (normalized linear, before --func and output transfer): {mapping}"));
                    }
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
}
