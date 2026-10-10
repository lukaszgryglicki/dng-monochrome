mod common;

use clap::CommandFactory;
use dng_monochrome::{
    cli::Cli,
    expression::{Expression, FunctionPolicy},
    parameters::Parameters,
    range::{self, Histogram, RangeOptions},
    raw,
    tone::{self, ToneCurve, Transfers},
};
use std::{ffi::OsString, fs, io::BufReader, path::Path, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_dng-monochrome");

fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Cli, clap::Error> {
    Cli::try_parse_compat(
        ["dng-monochrome".to_owned(), "unused.DNG".to_owned()]
            .into_iter()
            .chain(arguments)
            .map(OsString::from),
    )
}

fn declarations() -> Vec<(String, String, String)> {
    Cli::command()
        .get_arguments()
        .filter_map(|argument| {
            let name = argument.get_long()?;
            name.starts_with("param-").then(|| {
                (
                    name.to_owned(),
                    argument.get_id().to_string(),
                    argument.get_default_values()[0]
                        .to_str()
                        .unwrap()
                        .to_owned(),
                )
            })
        })
        .collect()
}

fn command(input: &Path, output: &Path, arguments: &[String]) -> std::process::Output {
    Command::new(BIN)
        .arg(input)
        .arg("--output")
        .arg(output)
        .args(["--jpg", "--threads", "2", "--report"])
        .args(arguments)
        .output()
        .unwrap()
}

fn success(output: std::process::Output) -> std::process::Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn pixels(path: &Path) -> Vec<u16> {
    let mut decoder = png::Decoder::new(BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut bytes = vec![0; decoder.output_buffer_size().unwrap()];
    let frame = decoder.next_frame(&mut bytes).unwrap();
    bytes[..frame.buffer_size()]
        .chunks_exact(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect()
}

fn jpeg(path: &Path) -> Vec<u8> {
    jpeg_decoder::Decoder::new(BufReader::new(fs::File::open(path).unwrap()))
        .decode()
        .unwrap()
}

const OVERRIDES: &[(&str, &str)] = &[
    ("histogram-chunks-per-thread", "7"),
    ("histogram-min-chunk", "2048"),
    ("auto-cap", "0.03"),
    ("tail-steps", "80"),
    ("tail-noise-sigma", "3"),
    ("tail-min-span", "8"),
    ("tail-knee-score", "0.3"),
    ("dark-snr", "3"),
    ("strength-target-base", "1.2"),
    ("strength-knee-weight", "0.7"),
    ("strength-target-min", "0.8"),
    ("strength-target-max", "3"),
    ("tied-span", "2"),
    ("noise-warning-sigmas", "10"),
    ("coherent-samples", "10000"),
    ("coherent-radius", "2"),
    ("coherent-neighbors", "6"),
    ("coherent-min-support", "6"),
    ("coherent-tail-fraction", "0.01"),
    ("coherent-noise-margin", "3"),
    ("noise-patch-side", "8"),
    ("noise-max-patches", "4000"),
    ("noise-haar-lag-small", "2"),
    ("noise-haar-lag-medium", "3"),
    ("noise-haar-lag-large", "6"),
    ("noise-texture-max", "1.7"),
    ("noise-lag-growth-max", "1.5"),
    ("noise-min-patches", "16"),
    ("noise-patches-per-bin", "32"),
    ("noise-max-bins", "20"),
    ("noise-variance-quantile", "0.4"),
    ("noise-min-sigma", "0.6"),
    ("noise-fit-min-bins", "5"),
    ("noise-fit-min-span", "20"),
    ("noise-fit-pair-separation", "0.2"),
    ("noise-fit-iterations", "5"),
    ("noise-fit-residual-scale", "0.2"),
    ("noise-fit-negative-slope", "0.2"),
    ("noise-fit-max-error", "0.3"),
    ("noise-refinement-passes", "2"),
    ("noise-fit-success-fraction", "0.7"),
    ("noise-sensitivity-tail", "0.15"),
    ("noise-read-ratio-max", "4"),
    ("noise-extrapolation-fraction", "0.4"),
    ("noise-shadow-read-sigmas", "7"),
    ("noise-tail-divisor", "4"),
    ("noise-correlation-min-variance", "0.36"),
    ("noise-correlation-warning", "1.4"),
    ("noise-profile-tolerance", "3"),
    ("optimize-strength", "0.65"),
    ("tone-exposure-bias", "0.5"),
    ("tone-min-exposure-ev", "-1"),
    ("tone-max-exposure-ev", "5"),
    ("tone-exposure-steps", "4"),
    ("tone-midtone-quantile", "0.4"),
    ("tone-shadow-quantile", "0.05"),
    ("tone-highlight-quantile", "0.95"),
    ("tone-target-midtone", "0.5"),
    ("tone-midtone-tolerance", "0.2"),
    ("tone-target-contrast", "0.7"),
    ("tone-contrast-tolerance", "0.5"),
    ("tone-contrast-weight", "0.2"),
    ("tone-noise-threshold", "0.12"),
    ("tone-noise-tolerance", "0.12"),
    ("tone-noise-weight", "0.4"),
    ("tone-contrast-strength", "0.5"),
    ("tone-bins", "256"),
    ("tone-smoothing-passes", "2"),
    ("tone-smoothing-radius", "3"),
    ("tone-density-prior", "0.2"),
    ("tone-density-power", "0.6"),
    ("tone-noise-target", "0.05"),
    ("tone-cap-min", "1.2"),
    ("tone-cap-max", "4"),
    ("tone-slope-min", "0.4"),
    ("tone-solver-upper", "64"),
    ("tone-solver-iterations", "60"),
    ("expression-max-bytes", "32768"),
    ("expression-max-parentheses", "192"),
    ("expression-max-depth", "384"),
    ("mapping-steps", "10"),
    ("mapping-input-decimals", "3"),
    ("mapping-output-decimals", "6"),
    ("progress-dr-decimals", "3"),
    ("progress-range-decimals", "2"),
    ("progress-clip-decimals", "4"),
    ("progress-elapsed-decimals", "2"),
    ("progress-aperture-decimals", "1"),
    ("jpeg-optimize-huffman", "false"),
];

#[test]
fn every_parameter_has_identical_rust_cli_and_explicit_defaults() {
    let defaults = Parameters::default();
    defaults.validate().unwrap();
    assert_eq!(parse([]).unwrap().parameters, defaults);
    let declarations = declarations();
    let serialized = serde_json::to_value(&defaults).unwrap();
    assert_eq!(declarations.len(), serialized.as_object().unwrap().len());
    assert_eq!(declarations.len(), OVERRIDES.len());
    let mut all = Vec::new();
    for (name, field, value) in &declarations {
        assert!(serialized.get(field).is_some(), "unreported {name}");
        for arguments in [
            vec![format!("--{name}={value}")],
            vec![format!("--{name}"), value.clone()],
            vec![format!("-{name}={value}")],
            vec![format!("-{name}"), value.clone()],
        ] {
            assert_eq!(parse(arguments).unwrap().parameters, defaults, "{name}");
        }
        all.push(format!("--{name}={value}"));
    }
    assert_eq!(parse(all).unwrap().parameters, defaults);
}

#[test]
fn all_nondefault_parameters_work_together_and_are_reported_and_applied() {
    let flags: Vec<_> = OVERRIDES
        .iter()
        .enumerate()
        .flat_map(|(i, (name, value))| {
            if i % 2 == 0 {
                vec![format!("--param-{name}={value}")]
            } else {
                vec![format!("-param-{name}"), value.to_string()]
            }
        })
        .collect();
    let cli = parse(flags.clone()).unwrap();
    let values = serde_json::to_value(&cli.parameters).unwrap();
    let defaults = serde_json::to_value(Parameters::default()).unwrap();
    for &(name, value) in OVERRIDES {
        let field = name.replace('-', "_");
        assert_ne!(values[&field], defaults[&field], "{name} did not change");
        if let Some(actual) = values[&field].as_f64() {
            assert_eq!(actual, value.parse::<f64>().unwrap(), "{name}");
        } else {
            assert_eq!(
                values[&field].as_bool().unwrap(),
                value.parse::<bool>().unwrap()
            );
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(256, 128).write(&input);
    let output = tmp.path().join("output");
    let result = success(command(&input, &output, &flags));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("photo.json")).unwrap()).unwrap();
    assert_eq!(report["parameters"], values);
    assert_eq!(report["range"]["noise"]["patch_side"], 8);
    assert_eq!(
        report["tone"]["perceptual_curve"].as_array().unwrap().len(),
        257
    );
    assert_eq!(report["tone"]["optimize_strength"], 0.65);
    let image = raw::decode(&input, false).unwrap();
    let hist = Histogram::with_parameters(&image.pixels, &cli.parameters).unwrap();
    let range =
        range::analyze_with_parameters(&image, &hist, RangeOptions::default(), &cli.parameters)
            .unwrap();
    let expected_report: serde_json::Value =
        serde_json::from_slice(&serde_json::to_vec(&range).unwrap()).unwrap();
    assert_eq!(report["range"], expected_report);
    let expected = tone::render_with_parameters(
        &image,
        &hist,
        &range,
        true,
        None,
        FunctionPolicy::Clip,
        Transfers::default(),
        &cli.parameters,
    )
    .unwrap();
    assert_eq!(pixels(&output.join("photo.png")), expected.png);
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("f/2.8, t 1/125s"));
    let mapping = stderr
        .lines()
        .find(|line| line.contains("best mapping"))
        .unwrap();
    let points: Vec<_> = mapping.split(": ").nth(1).unwrap().split(", ").collect();
    assert_eq!(points.len(), 9);
    assert!(points[0].starts_with("0.100->") && points[8].starts_with("0.900->"));
    assert!(points.iter().all(|point| {
        point
            .split("->")
            .nth(1)
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap()
            .len()
            == 6
    }));
    let progress = stderr
        .lines()
        .find(|line| line.starts_with("[1/1]"))
        .unwrap();
    assert_eq!(
        progress
            .split("clip ")
            .nth(1)
            .unwrap()
            .split('%')
            .next()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        progress
            .split("raw-code span ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        progress
            .split("(elapsed ")
            .nth(1)
            .unwrap()
            .split('s')
            .next()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn explicitly_setting_every_default_does_not_change_encoded_images() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(128, 64).write(&input);
    let implicit = tmp.path().join("implicit");
    let explicit = tmp.path().join("explicit");
    success(command(&input, &implicit, &[]));
    let flags = declarations()
        .iter()
        .map(|(name, _, value)| format!("--{name}={value}"))
        .collect::<Vec<_>>();
    success(command(&input, &explicit, &flags));
    for extension in ["png", "jpg"] {
        assert_eq!(
            fs::read(implicit.join(format!("photo.{extension}"))).unwrap(),
            fs::read(explicit.join(format!("photo.{extension}"))).unwrap(),
        );
    }
}

#[test]
fn every_parameter_rejects_nonfinite_or_out_of_bounds_values() {
    for (name, _, default) in declarations() {
        let invalid: &[&str] = if default == "true" {
            &["maybe", "1", "0"]
        } else {
            &["NaN", "inf", "-inf", "1e100", "-1e100"]
        };
        for value in invalid {
            let error = parse([format!("--{name}={value}")]).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{name}={value}");
        }
    }
    for args in [
        vec![
            "--param-strength-target-min=3",
            "--param-strength-target-max=2",
        ],
        vec!["--param-coherent-radius=1", "--param-coherent-neighbors=9"],
        vec![
            "--param-noise-patch-side=5",
            "--param-noise-haar-lag-large=5",
        ],
        vec!["--param-noise-haar-lag-small=2"],
        vec!["--param-noise-fit-min-bins=17"],
        vec!["--param-tone-min-exposure-ev=7"],
        vec!["--param-tone-shadow-quantile=0.6"],
        vec!["--param-tone-highlight-quantile=0.4"],
        vec!["--param-tone-cap-min=4"],
        vec!["--param-tone-density-prior=0.01"],
    ] {
        assert!(parse(args.into_iter().map(str::to_owned)).is_err());
    }
    for name in [
        "levels",
        "png-bit-depth",
        "png-compression",
        "srgb-gamma",
        "not-a-parameter",
    ] {
        assert!(parse([format!("-param-{name}=2")]).is_err());
    }
}

#[test]
fn invalid_parameters_fail_before_any_input_io_and_do_not_disappear_in_silent_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("must-not-exist");
    for flags in [
        vec!["--param-tone-bins=0", "--silent"],
        vec!["--param-auto-cap=NaN", "--silent"],
        vec!["--param-tone-slope-min=2", "--silent"],
        vec!["--param-noise-max-patches=0", "--silent"],
    ] {
        let flags = flags.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let result = command(&tmp.path().join("missing.DNG"), &output, &flags);
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("--param-"));
        assert!(!String::from_utf8_lossy(&result.stderr).contains("locating input"));
        assert!(!output.exists());
    }
}

#[test]
fn analysis_and_verbose_output_include_effective_parameters_without_rendering() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(128, 64).write(&input);
    let output = tmp.path().join("unused");
    let result = success(
        Command::new(BIN)
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .args([
                "--analyze",
                "--verbose",
                "--threads=1",
                "--param-noise-patch-side=8",
                "-param-tail-steps",
                "50",
            ])
            .output()
            .unwrap(),
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["parameters"]["noise_patch_side"], 8);
    assert_eq!(report["parameters"]["tail_steps"], 50);
    assert_eq!(report["range"]["noise"]["patch_side"], 8);
    let diagnostic: serde_json::Value = serde_json::from_slice(&result.stderr).unwrap();
    assert_eq!(report["parameters"], diagnostic["parameters"]);
    assert!(!output.exists());
}

#[test]
fn optimize_amount_exposure_and_histogram_controls_have_distinct_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    let mut dng = common::Dng::ramp(256, 256);
    dng.pixels.iter_mut().for_each(|value| {
        *value =
            (1023.0 + ((f64::from(*value) - 1023.0) / 15360.0).powi(3) * 15360.0).round() as u16;
    });
    dng.write(&input);
    let image = raw::decode(&input, false).unwrap();
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
    let full = ToneCurve::fit(&hist, &range, true, image.metadata.black_level);
    for amount in [0.0, 0.25, 0.7, 1.0] {
        let parameters = Parameters {
            optimize_strength: amount,
            ..Parameters::default()
        };
        let curve = ToneCurve::fit_with_parameters(
            &hist,
            &range,
            true,
            image.metadata.black_level,
            &parameters,
        )
        .unwrap();
        let mut previous = 0.0;
        for i in 0..=1000 {
            let x = f64::from(i) / 1000.0;
            let expected = (1.0 - amount) * x + amount * full.map(x);
            assert!((curve.map(x) - expected).abs() < 1e-14);
            assert!(curve.map(x) >= previous);
            previous = curve.map(x);
        }
    }
    let fixed = Parameters {
        tone_min_exposure_ev: 1.0,
        tone_max_exposure_ev: 1.0,
        tone_exposure_bias: 0.5,
        tone_contrast_strength: 0.0,
        ..Parameters::default()
    };
    let curve =
        ToneCurve::fit_with_parameters(&hist, &range, true, image.metadata.black_level, &fixed)
            .unwrap();
    assert_eq!(curve.exposure_factor, 2.0 * 0.5f64.exp2());
    assert!(curve.perceptual_curve.is_empty());
    for i in 1..100 {
        let x = f64::from(i) / 100.0;
        let a = curve.exposure_factor;
        assert_eq!(curve.map(x), a * x / (1.0 + (a - 1.0) * x));
    }
    let mut low = Parameters {
        tone_target_midtone: 0.2,
        tone_noise_weight: 0.0,
        ..Parameters::default()
    };
    let dark =
        ToneCurve::fit_with_parameters(&hist, &range, true, image.metadata.black_level, &low)
            .unwrap();
    low.tone_target_midtone = 0.7;
    let bright =
        ToneCurve::fit_with_parameters(&hist, &range, true, image.metadata.black_level, &low)
            .unwrap();
    assert!(bright.exposure_factor > dark.exposure_factor);
    for (bins, radius, passes, power, minimum, cap) in [
        (16, 0, 0, 0.0, 0.0, 1.0),
        (257, 4, 2, 0.7, 0.2, 2.0),
        (2048, 8, 1, 1.2, 0.5, 4.0),
    ] {
        let parameters = Parameters {
            tone_bins: bins,
            tone_smoothing_radius: radius,
            tone_smoothing_passes: passes,
            tone_density_power: power,
            tone_slope_min: minimum,
            tone_cap_min: cap,
            tone_cap_max: cap,
            tone_contrast_strength: 1.0,
            ..Parameters::default()
        };
        let curve = ToneCurve::fit_with_parameters(
            &hist,
            &range,
            true,
            image.metadata.black_level,
            &parameters,
        )
        .unwrap();
        assert_eq!(curve.perceptual_curve.len(), bins + 1);
        for pair in curve.perceptual_curve.windows(2) {
            let slope = (pair[1] - pair[0]) * bins as f64;
            assert!((minimum - 1e-7..=cap + 1e-7).contains(&slope), "{slope}");
        }
    }
    let failed = Parameters {
        tone_solver_iterations: 1,
        ..Parameters::default()
    };
    assert!(
        ToneCurve::fit_with_parameters(&hist, &range, true, image.metadata.black_level, &failed)
            .unwrap_err()
            .to_string()
            .contains("did not converge")
    );
    let output = tmp.path().join("nonconvergent");
    let result = command(
        &input,
        &output,
        &["--param-tone-solver-iterations=1".into()],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("did not converge"));
    assert!(!output.exists());
}

#[test]
fn zero_optimization_amount_matches_no_optimize_even_with_functions() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(128, 64).write(&input);
    let off = tmp.path().join("off");
    let zero = tmp.path().join("zero");
    let ignored = tmp.path().join("ignored");
    success(command(
        &input,
        &off,
        &["--no-optimize", "--func=x^2"].map(str::to_owned),
    ));
    success(command(
        &input,
        &zero,
        &["--param-optimize-strength=0", "--func=x^2"].map(str::to_owned),
    ));
    success(command(
        &input,
        &ignored,
        &[
            "--no-optimize",
            "--func=x^2",
            "--param-tone-exposure-bias=5",
            "--param-tone-contrast-strength=1",
            "--param-tone-solver-iterations=1",
            "--param-optimize-strength=0.4",
        ]
        .map(str::to_owned),
    ));
    for extension in ["png", "jpg"] {
        assert_eq!(
            fs::read(off.join(format!("photo.{extension}"))).unwrap(),
            fs::read(zero.join(format!("photo.{extension}"))).unwrap()
        );
        assert_eq!(
            fs::read(off.join(format!("photo.{extension}"))).unwrap(),
            fs::read(ignored.join(format!("photo.{extension}"))).unwrap()
        );
    }
}

#[test]
fn expression_resource_limits_are_configurable_without_changing_the_grammar() {
    let small = Parameters {
        expression_max_bytes: 3,
        expression_max_parentheses: 2,
        expression_max_depth: 4,
        ..Parameters::default()
    };
    assert!(Expression::parse_with_parameters("x^2", &small).is_ok());
    assert!(
        Expression::parse_with_parameters("x^20", &small)
            .unwrap_err()
            .to_string()
            .contains("3 bytes")
    );
    let limits = Parameters {
        expression_max_bytes: 1000,
        ..small
    };
    assert!(Expression::parse_with_parameters("((x))", &limits).is_ok());
    assert!(
        Expression::parse_with_parameters("(((x)))", &limits)
            .unwrap_err()
            .to_string()
            .contains("2 nested parentheses")
    );
    assert!(
        Expression::parse_with_parameters("-----x", &limits)
            .unwrap_err()
            .to_string()
            .contains("4 nested operations")
    );
    let extended = Parameters {
        expression_max_parentheses: 200,
        expression_max_depth: 400,
        expression_max_bytes: 32768,
        ..Parameters::default()
    };
    let source = format!("{}sin(pi*x){}", "(".repeat(150), ")".repeat(150));
    assert!(Expression::parse(&source).is_err());
    let expression = Expression::parse_with_parameters(&source, &extended).unwrap();
    assert_eq!(expression.bind()(0.5), 1.0);
    let chain = format!("{}x", "x+".repeat(9000));
    assert!(Expression::parse(&chain).is_err());
    let expression = Expression::parse_with_parameters(&chain, &extended).unwrap();
    assert_eq!(expression.bind()(0.5), 4500.5);
}

#[test]
fn jpeg_huffman_parameter_changes_encoding_but_not_decoded_pixels() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(128, 64).write(&input);
    let optimized = tmp.path().join("optimized");
    let standard = tmp.path().join("standard");
    success(command(&input, &optimized, &[]));
    success(command(
        &input,
        &standard,
        &["--param-jpeg-optimize-huffman=false".into()],
    ));
    assert_eq!(
        fs::read(optimized.join("photo.png")).unwrap(),
        fs::read(standard.join("photo.png")).unwrap()
    );
    assert_ne!(
        fs::read(optimized.join("photo.jpg")).unwrap(),
        fs::read(standard.join("photo.jpg")).unwrap()
    );
    assert_eq!(
        jpeg(&optimized.join("photo.jpg")),
        jpeg(&standard.join("photo.jpg"))
    );
}

#[test]
fn parameter_options_do_not_consume_expressions_or_end_of_options_filenames() {
    let cli =
        parse(["-param-tone-min-exposure-ev", "-3", "-func", "-x+1"].map(str::to_owned)).unwrap();
    assert_eq!(cli.parameters.tone_min_exposure_ev, -3.0);
    assert_eq!(cli.function.as_deref(), Some("-x+1"));
    let cli = parse(["--", "-param-tone-bins.DNG"].map(str::to_owned)).unwrap();
    assert!(
        cli.inputs
            .iter()
            .any(|path| path == Path::new("-param-tone-bins.DNG"))
    );
    assert_eq!(cli.parameters, Parameters::default());
}

#[test]
fn library_parameter_entrypoints_reject_invalid_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(32, 32).write(&input);
    let image = raw::decode(&input, false).unwrap();
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
    let invalid = Parameters {
        auto_cap: f64::NAN,
        ..Parameters::default()
    };
    assert!(Histogram::with_parameters(&image.pixels, &invalid).is_err());
    assert!(
        range::analyze_with_parameters(&image, &hist, RangeOptions::default(), &invalid).is_err()
    );
    assert!(
        dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &invalid).is_err()
    );
    assert!(Expression::parse_with_parameters("x", &invalid).is_err());
    assert!(ToneCurve::fit_with_parameters(&hist, &range, true, 1023.0, &invalid).is_err());
    assert!(
        tone::render_with_parameters(
            &image,
            &hist,
            &range,
            true,
            None,
            FunctionPolicy::Clip,
            Transfers::default(),
            &invalid,
        )
        .is_err()
    );
    let rendered = tone::render(
        &image,
        &hist,
        &range,
        true,
        None,
        FunctionPolicy::Clip,
        Transfers::default(),
    )
    .unwrap();
    let paths = dng_monochrome::output::OutputPaths::new(&tmp.path().join("output/photo"), false);
    assert!(
        dng_monochrome::output::save_with_parameters(
            &paths,
            &image,
            &rendered,
            &serde_json::json!({}),
            Transfers::default(),
            90,
            false,
            &invalid,
        )
        .is_err()
    );
    assert!(!tmp.path().join("output").exists());
}

#[test]
fn readme_lists_every_parameter_with_its_current_default() {
    let readme = include_str!("../README.md");
    for (name, _, value) in declarations() {
        let row = format!("| `--{name}` | `{value}` |");
        assert!(
            readme.contains(&row),
            "missing/stale parameter documentation: {row}"
        );
    }
}
