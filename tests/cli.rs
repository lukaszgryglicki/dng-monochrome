mod common;

use common::Dng;
use std::{
    fs,
    io::BufReader,
    path::Path,
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_dng-monochrome");

fn run(input: &Path, output: &Path, flags: &[&str]) -> Output {
    let mut command = Command::new(BIN);
    command.arg(input).arg("--output").arg(output);
    if !flags
        .iter()
        .any(|flag| flag.starts_with("--threads") || flag.starts_with("-threads"))
    {
        command.args(["--threads", "2"]);
    }
    command.args(flags).output().unwrap()
}

fn success(result: Output) -> Output {
    assert!(
        result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result
}

struct Png {
    width: u32,
    height: u32,
    pixels: Vec<u16>,
    srgb: bool,
    gamma: Option<f32>,
}

fn read_png(path: &Path) -> Png {
    let mut reader = png::Decoder::new(BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
    assert_eq!(reader.info().color_type, png::ColorType::Grayscale);
    let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut bytes).unwrap();
    assert_eq!(frame.bit_depth, png::BitDepth::Sixteen);
    assert_eq!(frame.color_type, png::ColorType::Grayscale);
    Png {
        width: frame.width,
        height: frame.height,
        pixels: bytes[..frame.buffer_size()]
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect(),
        srgb: reader.info().srgb.is_some(),
        gamma: reader.info().gamma().map(|v| v.into_value()),
    }
}

fn read_jpeg(path: &Path) -> (u16, u16, Vec<u8>) {
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(fs::File::open(path).unwrap()));
    let pixels = decoder.decode().unwrap();
    let info = decoder.info().unwrap();
    assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::L8);
    (info.width, info.height, pixels)
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn fixture(root: &Path) -> (std::path::PathBuf, Dng) {
    let path = root.join("photo.DNG");
    let dng = Dng::ramp(64, 32);
    dng.write(&path);
    (path, dng)
}

#[test]
fn help_version_and_missing_arguments() {
    let defaults = dng_monochrome::cli::Cli::try_parse_compat(
        ["dng-monochrome", "photo.DNG"]
            .into_iter()
            .map(std::ffi::OsString::from),
    )
    .unwrap();
    assert_eq!(defaults.jpeg_quality, 90);
    assert_eq!(defaults.clip_strength, 3);
    assert_eq!(
        defaults.transfers(),
        dng_monochrome::tone::Transfers::default()
    );
    assert_eq!(
        defaults.transfers().jpeg,
        dng_monochrome::tone::Transfer::Linear
    );
    assert!(defaults.optimization_enabled());
    for flag in ["--help", "-help", "-h"] {
        let output = success(Command::new(BIN).arg(flag).output().unwrap());
        let text = String::from_utf8(output.stdout).unwrap();
        for name in [
            "--output",
            "--dark",
            "--light",
            "--clip-strength",
            "--func",
            "--func-clip",
            "--func-scale",
            "--func-wrap",
            "--optimize",
            "--no-optimize",
            "--both",
            "--transfer",
            "--png-transfer",
            "--jpeg-transfer",
            "--jpeg-quality",
            "--threads",
            "--no-crop",
            "--report",
            "--analyze",
            "--overwrite",
        ] {
            assert!(text.contains(name), "missing {name} in {flag}");
        }
    }
    for flag in ["--version", "-version", "-V"] {
        let output = success(Command::new(BIN).arg(flag).output().unwrap());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains(env!("CARGO_PKG_VERSION"))
        );
    }
    assert!(!Command::new(BIN).output().unwrap().status.success());
}

#[test]
fn default_output_is_sixteen_bit_grayscale_at_maximum_compression() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let result = success(
        Command::new(BIN)
            .current_dir(tmp.path())
            .arg(&input)
            .output()
            .unwrap(),
    );
    let progress = String::from_utf8_lossy(&result.stderr);
    assert!(progress.contains("approx DR unavailable (insufficient noise evidence)"));
    assert!(progress.contains("raw-code span 13.9 bits"));
    let output = tmp.path().join("dng-mono");
    let png = read_png(&output.join("photo.png"));
    let (width, height, jpeg) = read_jpeg(&output.join("photo.jpg"));
    assert_eq!((png.width, png.height), (64, 32));
    assert_eq!((width, height), (64, 32));
    assert!(!png.srgb);
    assert_eq!(png.gamma, Some(1.0));
    assert_eq!(*png.pixels.iter().min().unwrap(), 0);
    assert_eq!(*png.pixels.iter().max().unwrap(), 65535);
    for (&p, &j) in png.pixels.iter().zip(&jpeg) {
        let expected = ((u32::from(p) + 128) / 257) as i32;
        assert!((expected - i32::from(j)).abs() <= 4);
    }
    assert!(!output.join("photo.json").exists());
    let bytes = fs::read(output.join("photo.png")).unwrap();
    let mut offset = 8;
    loop {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        if &bytes[offset + 4..offset + 8] == b"IDAT" {
            assert_eq!(
                bytes[offset + 9] >> 6,
                3,
                "zlib FLEVEL must indicate maximum compression"
            );
            break;
        }
        offset += length + 12;
        assert!(offset < bytes.len(), "missing PNG image data");
    }
}

#[test]
fn clip_strength_accepts_all_levels_alias_forms_and_rejects_invalid_values() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let mut previous = (0.0, 0.0);
    for strength in 1..=9 {
        let flag = format!("-clip-strength={strength}");
        let result = success(run(
            &input,
            &tmp.path().join("analysis"),
            &["--analyze", &flag],
        ));
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["range"]["clip_strength"], strength);
        let clipped = (
            report["range"]["clipped_dark_percent"].as_f64().unwrap(),
            report["range"]["clipped_light_percent"].as_f64().unwrap(),
        );
        assert!(clipped.0 >= previous.0 && clipped.1 >= previous.1);
        previous = clipped;
    }
    assert!((0.95..=1.0).contains(&previous.0) && (0.95..=1.0).contains(&previous.1));
    for value in ["0", "10", "-1", "1.5", "bad"] {
        let out = tmp.path().join(format!("invalid-{value}"));
        let result = run(&input, &out, &["--clip-strength", value]);
        assert_eq!(result.status.code(), Some(2));
        assert!(!out.exists());
    }
    let out = tmp.path().join("both");
    let result = success(run(
        &input,
        &out,
        &["-clip-strength", "9", "--both", "--report", "--silent"],
    ));
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    for mode in ["auto", "best"] {
        assert_eq!(
            json(&out.join(mode).join("photo.json"))["range"]["clip_strength"],
            9
        );
        assert_eq!(read_png(&out.join(mode).join("photo.png")).gamma, Some(1.0));
    }
}

#[test]
fn manual_clipping_outputs_do_not_depend_on_auto_strength() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let mut reference = None;
    for strength in ["1", "4", "9"] {
        let out = tmp.path().join(strength);
        success(run(
            &input,
            &out,
            &[
                "--clip-strength",
                strength,
                "--dark",
                "0.8",
                "--light",
                "1.2",
            ],
        ));
        let bytes = (
            fs::read(out.join("photo.png")).unwrap(),
            fs::read(out.join("photo.jpg")).unwrap(),
        );
        if let Some(expected) = &reference {
            assert_eq!(&bytes, expected);
        } else {
            reference = Some(bytes);
        }
    }
}

#[test]
fn best_prints_nineteen_interior_samples_before_the_custom_function() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let first = success(run(&input, &tmp.path().join("best"), &[]));
    let second = success(run(
        &input,
        &tmp.path().join("function"),
        &["--best", "--func", "x^2"],
    ));
    let line = |output: &Output| {
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|s| s.contains("best mapping"))
            .expect("best mapping must be printed")
            .to_owned()
    };
    assert_eq!(line(&first), line(&second));
    let text = line(&first);
    assert!(text.contains("before --func and output transfer"));
    let points: Vec<_> = text.split(": ").nth(1).unwrap().split(", ").collect();
    assert_eq!(points.len(), 19);
    let mut previous = 0.0;
    for (i, pair) in points.iter().enumerate() {
        let (x, y) = pair.split_once("->").unwrap();
        assert_eq!(x, format!("{:.2}", (i + 1) as f64 / 20.0));
        assert_eq!(y.split('.').nth(1).unwrap().len(), 5);
        let value: f64 = y.parse().unwrap();
        assert!(value >= previous && (0.0..=1.0).contains(&value));
        previous = value;
    }
    assert!(points[0].starts_with("0.05->"));
    assert!(points[18].starts_with("0.95->"));
    assert!(!text.contains("0.00->") && !text.contains("1.00->"));
    let quiet = success(run(
        &input,
        &tmp.path().join("quiet-best"),
        &["--best", "--silent"],
    ));
    assert!(quiet.stdout.is_empty() && quiet.stderr.is_empty());
}

#[test]
fn every_single_dash_option_and_complex_expression_execute_together() {
    let tmp = tempfile::tempdir().unwrap();
    let mut dng = Dng::ramp(64, 32);
    dng.crop = Some([1, 1, 62, 30]);
    let input = tmp.path().join("photo.DNG");
    dng.write(&input);
    let output = tmp.path().join("single");
    success(
        Command::new(BIN)
            .args([
                "-dark",
                "0.8",
                "-light",
                "1.2%",
                "-func",
                "((exp(2*x)-1)/(exp(2)-1)+(sin(pi*x)^2)*(1-x)/4+((1-(1-x)^3)-x)/8)",
                "-func-scale",
                "-best",
                "-report",
                "-no-crop",
                "-threads",
                "2",
                "-jpeg-quality",
                "100",
                "-transfer",
                "linear",
                "-o",
            ])
            .arg(&output)
            .arg(&input)
            .output()
            .unwrap(),
    );
    let png = read_png(&output.join("photo.png"));
    assert_eq!((png.width, png.height), (64, 32));
    assert!(!png.srgb);
    assert_eq!(png.gamma, Some(1.0));
    assert_eq!(
        (
            *png.pixels.iter().min().unwrap(),
            *png.pixels.iter().max().unwrap()
        ),
        (0, 65535)
    );
    let report = json(&output.join("photo.json"));
    assert_eq!(report["mode"], "best");
    assert_eq!(report["range"]["requested_dark_percent"], 0.8);
    assert_eq!(report["range"]["requested_light_percent"], 1.2);
    assert_eq!(report["function"]["policy"], "scale");
    assert_eq!(report["transfer"], "linear");
    assert_eq!(report["jpeg_quality"], 100);
    assert_eq!(report["threads"], 2);
    assert!(
        report["png_compression"]
            .as_str()
            .unwrap()
            .contains("level 9")
    );
}

#[test]
fn optimization_defaults_on_and_has_only_the_requested_opt_out() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let default = tmp.path().join("default");
    let explicit = tmp.path().join("explicit");
    let off = tmp.path().join("off");
    success(run(&input, &default, &["--report"]));
    success(run(&input, &explicit, &["--optimize", "--report"]));
    let result = success(run(&input, &off, &["-no-optimize", "--report"]));
    assert_eq!(json(&default.join("photo.json"))["mode"], "best");
    assert_eq!(json(&off.join("photo.json"))["mode"], "auto");
    assert_eq!(
        fs::read(default.join("photo.png")).unwrap(),
        fs::read(explicit.join("photo.png")).unwrap()
    );
    assert_ne!(
        read_png(&default.join("photo.png")).pixels,
        read_png(&off.join("photo.png")).pixels
    );
    assert!(!String::from_utf8_lossy(&result.stderr).contains("best mapping"));
    for flags in [
        vec!["--no-best"],
        vec!["-no-best"],
        vec!["--no-optimize", "--best"],
        vec!["--no-optimize", "--optimize"],
        vec!["--no-optimize", "--both"],
    ] {
        assert_eq!(
            run(&input, &tmp.path().join("invalid"), &flags)
                .status
                .code(),
            Some(2)
        );
    }
}

#[test]
fn independent_transfers_and_shared_overrides_have_exact_precedence() {
    use dng_monochrome::tone::{
        Transfer::{Linear, Srgb},
        Transfers,
    };
    for (flags, expected) in [
        (
            vec![],
            Transfers {
                png: Linear,
                jpeg: Linear,
            },
        ),
        (
            vec!["-transfer", "linear"],
            Transfers {
                png: Linear,
                jpeg: Linear,
            },
        ),
        (
            vec!["--transfer=srgb"],
            Transfers {
                png: Srgb,
                jpeg: Srgb,
            },
        ),
        (
            vec!["--png-transfer", "srgb"],
            Transfers {
                png: Srgb,
                jpeg: Linear,
            },
        ),
        (
            vec!["-jpg-transfer", "linear"],
            Transfers {
                png: Linear,
                jpeg: Linear,
            },
        ),
        (
            vec!["--transfer", "linear", "--jpeg-transfer", "srgb"],
            Transfers {
                png: Linear,
                jpeg: Srgb,
            },
        ),
        (
            vec!["--jpeg-transfer", "linear", "--transfer", "srgb"],
            Transfers {
                png: Srgb,
                jpeg: Linear,
            },
        ),
    ] {
        let args = ["dng-monochrome", "photo.DNG"].into_iter().chain(flags);
        let cli =
            dng_monochrome::cli::Cli::try_parse_compat(args.map(std::ffi::OsString::from)).unwrap();
        assert_eq!(cli.transfers(), expected);
    }
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    for png_transfer in ["linear", "srgb"] {
        for jpeg_transfer in ["linear", "srgb"] {
            let out = tmp.path().join(format!("{png_transfer}-{jpeg_transfer}"));
            success(run(
                &input,
                &out,
                &[
                    "--no-optimize",
                    "--dark",
                    "0",
                    "--light",
                    "0",
                    "--png-transfer",
                    png_transfer,
                    "--jpg-transfer",
                    jpeg_transfer,
                    "--jpeg-quality",
                    "100",
                    "--report",
                ],
            ));
            let png = read_png(&out.join("photo.png"));
            let (_, _, jpeg) = read_jpeg(&out.join("photo.jpg"));
            assert_eq!(png.srgb, png_transfer == "srgb");
            let report = json(&out.join("photo.json"));
            assert_eq!(report["png_transfer"], png_transfer);
            assert_eq!(report["jpeg_transfer"], jpeg_transfer);
            for (&p, &j) in png.pixels.iter().zip(&jpeg) {
                let stored = f64::from(p) / 65535.0;
                let expected = if png_transfer == jpeg_transfer {
                    stored
                } else if png_transfer == "linear" {
                    dng_monochrome::tone::srgb_encode(stored)
                } else {
                    dng_monochrome::tone::srgb_decode(stored)
                };
                assert!((i32::from(j) - (expected * 255.0).round() as i32).abs() <= 2);
            }
        }
    }
}

#[test]
fn default_progress_prints_real_iso_aperture_and_rational_exposure() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, mut dng) = fixture(tmp.path());
    let first = success(run(&input, &tmp.path().join("standard"), &[]));
    assert!(String::from_utf8_lossy(&first.stderr).contains("ISO 125, f/2.80, t 1/125s"));
    dng.iso = Some(160000);
    dng.aperture = None;
    dng.apex = Some([6, 1]);
    dng.exposure = [1, 50];
    dng.write(&input);
    let second = success(run(&input, &tmp.path().join("apex"), &["--report"]));
    assert!(String::from_utf8_lossy(&second.stderr).contains("ISO 160000, f/8.00 (APEX), t 1/50s"));
    let report = json(&tmp.path().join("apex/photo.json"));
    assert!(report["metadata"]["aperture_f_number"].is_null());
    assert_eq!(report["metadata"]["aperture_value_apex"], 6.0);
    dng.iso = None;
    dng.apex = None;
    dng.write(&input);
    let missing = success(run(&input, &tmp.path().join("missing"), &[]));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("ISO unknown, f/unknown"));
}

#[test]
fn long_expression_cli_result_matches_independent_pixel_calculation() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, dng) = fixture(tmp.path());
    let output = tmp.path().join("complex");
    success(run(
        &input,
        &output,
        &[
            "--no-optimize",
            "--dark",
            "0",
            "--light",
            "0",
            "--transfer",
            "linear",
            "--func-clip",
            "--func",
            "((exp(2*x)-1)/(exp(2)-1)+(sin(pi*x)^2)*(1-x)/4+((1-(1-x)^3)-x)/8)",
        ],
    ));
    let png = read_png(&output.join("photo.png"));
    for (&raw, &actual) in dng.pixels.iter().zip(&png.pixels) {
        let x = (f64::from(raw) - 1023.0) / 15360.0;
        let y = ((2.0 * x).exp() - 1.0) / (2.0f64.exp() - 1.0)
            + (std::f64::consts::PI * x).sin().powi(2) * (1.0 - x) / 4.0
            + ((1.0 - (1.0 - x).powi(3)) - x) / 8.0;
        assert_eq!(actual, (y.clamp(0.0, 1.0) * 65535.0).round() as u16);
    }
}

#[test]
fn all_function_policies_and_leading_minus_are_exercised_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    for (index, policy, expression) in [
        (0, "-func-clip", "-x^2"),
        (1, "--func-scale", "x/2"),
        (2, "-func-wrap", "2*x"),
        (3, "--func-clip", "x^.5"),
    ] {
        let output = tmp.path().join(index.to_string());
        success(run(
            &input,
            &output,
            &[
                "--no-optimize",
                "--dark=0",
                "-light=0%",
                "--transfer=linear",
                "--func",
                expression,
                policy,
                "--report",
            ],
        ));
        let png = read_png(&output.join("photo.png"));
        match index {
            0 => assert!(png.pixels.iter().all(|&p| p == 0)),
            1 => assert_eq!((png.pixels[0], *png.pixels.last().unwrap()), (0, 65535)),
            2 => assert_eq!((png.pixels[0], *png.pixels.last().unwrap()), (0, 0)),
            3 => assert!(png.pixels[1024] > 46000),
            _ => unreachable!(),
        }
    }
}

#[test]
fn recursive_inputs_both_modes_and_json_reports_preserve_relative_names() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir_all(input.join("nested")).unwrap();
    let dng = Dng::ramp(64, 32);
    dng.write(&input.join("first.DNG"));
    dng.write(&input.join("nested").join("second.dng"));
    fs::write(input.join("ignored.jpg"), b"not an input").unwrap();
    let output = tmp.path().join("both");
    success(run(&input, &output, &["-both", "-report"]));
    for mode in ["auto", "best"] {
        for stem in ["first", "nested/second"] {
            let base = output.join(mode).join(stem);
            let png = read_png(&base.with_extension("png"));
            assert_eq!((png.width, png.height), (64, 32));
            read_jpeg(&base.with_extension("jpg"));
            let report = json(&base.with_extension("json"));
            assert_eq!(report["mode"], mode);
            assert_eq!(
                report["output"]["png_min"],
                u64::from(*png.pixels.iter().min().unwrap())
            );
            assert_eq!(
                report["output"]["png_max"],
                u64::from(*png.pixels.iter().max().unwrap())
            );
        }
    }
    assert_eq!(
        json(&output.join("auto/first.json"))["range"],
        json(&output.join("best/first.json"))["range"]
    );
    assert_ne!(
        read_png(&output.join("auto/first.png")).pixels,
        read_png(&output.join("best/first.png")).pixels
    );
}

#[test]
fn shell_expanded_multiple_dngs_generate_matching_png_and_jpeg_names() {
    let tmp = tempfile::tempdir().unwrap();
    Dng::ramp(32, 16).write(&tmp.path().join("alpha.DNG"));
    Dng::ramp(32, 16).write(&tmp.path().join("beta.DNG"));
    success(
        Command::new("sh")
            .current_dir(tmp.path())
            .args([
                "-c",
                "\"$1\" --threads 2 -o . --dark 0 --light 0 ./*.DNG",
                "dng-glob-test",
                BIN,
            ])
            .output()
            .unwrap(),
    );
    for stem in ["alpha", "beta"] {
        read_png(&tmp.path().join(format!("{stem}.png")));
        read_jpeg(&tmp.path().join(format!("{stem}.jpg")));
    }
}

#[test]
fn duplicate_inputs_are_deduplicated_and_colliding_names_fail_before_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let output = tmp.path().join("duplicates");
    let result = success(
        Command::new(BIN)
            .arg(&input)
            .arg(&input)
            .args(["--threads", "1", "-o"])
            .arg(&output)
            .output()
            .unwrap(),
    );
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("from 1 DNG(s)")
    );
    let second = tmp.path().join("second");
    fs::create_dir(&second).unwrap();
    let (same_name, _) = fixture(&second);
    let conflict = tmp.path().join("conflict");
    let result = Command::new(BIN)
        .arg(&input)
        .arg(&same_name)
        .arg("-o")
        .arg(&conflict)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("same output"));
    assert!(!conflict.exists());
    let namespaced = tmp.path().join("namespaced");
    let first_dir = tmp.path().join("first");
    fs::create_dir(&first_dir).unwrap();
    fixture(&first_dir);
    success(
        Command::new(BIN)
            .arg(&first_dir)
            .arg(&second)
            .args(["--threads", "1", "-o"])
            .arg(&namespaced)
            .output()
            .unwrap(),
    );
    assert!(namespaced.join("first/photo.png").exists());
    assert!(namespaced.join("second/photo.png").exists());
}

#[test]
fn analyze_outputs_json_without_creating_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let output = tmp.path().join("unused");
    let result = success(run(
        &input,
        &output,
        &["-analyze", "--dark", "0", "--light", "0"],
    ));
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["metadata"]["white_level"], 16383);
    assert_eq!(report["range"]["white_level_bits"], 14.0);
    assert_eq!(report["range"]["lower"], 1023.0);
    assert_eq!(report["range"]["upper"], 16383.0);
    assert!(!output.exists());
}

#[test]
fn overwrite_is_explicit_atomic_and_keeps_reports_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let original = fs::read(&input).unwrap();
    let output = tmp.path().join("output");
    success(run(&input, &output, &["--report"]));
    let png = fs::read(output.join("photo.png")).unwrap();
    assert!(
        !run(&input, &output, &["--report", "--func", "x^2"])
            .status
            .success()
    );
    assert_eq!(fs::read(output.join("photo.png")).unwrap(), png);
    assert!(
        !run(&input, &output, &["--overwrite", "--func", "x^2"])
            .status
            .success()
    );
    success(run(
        &input,
        &output,
        &["-overwrite", "-report", "--func", "x^2"],
    ));
    assert_ne!(fs::read(output.join("photo.png")).unwrap(), png);
    assert_eq!(
        json(&output.join("photo.json"))["function"]["expression"],
        "x^2"
    );
    assert_eq!(fs::read(&input).unwrap(), original);
    assert!(fs::read_dir(&output).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".dng-monochrome-")
    }));
}

#[test]
fn invalid_flags_values_and_expressions_never_create_success_shaped_output() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let cases: &[&[&str]] = &[
        &["--dark", "-0.1"],
        &["--light", "100"],
        &["--dark", "NaN"],
        &["--dark", "inf"],
        &["--dark", "1.2%%"],
        &["--dark", "60", "--light", "40"],
        &["--func-scale"],
        &["--func", "x", "--func-clip", "--func-wrap"],
        &["--both", "--best"],
        &["--analyze", "--report"],
        &["--analyze", "--best"],
        &["--analyze", "--func", "x"],
        &["--analyze", "--overwrite"],
        &["--transfer", "bad"],
        &["--jpeg-quality", "0"],
        &["--jpeg-quality", "101"],
        &["--threads", "257"],
        &["--threads", "-1"],
        &["--png-compression", "fast"],
        &["--unknown"],
        &["--func", ""],
        &["--func", " "],
        &["--func", "sin()"],
        &["--func", "x+"],
        &["--func", "y"],
        &["--func", "exec(x)"],
        &["--func", "ln(x)", "--func-clip"],
        &["--func", "exp(1000)", "--func-scale"],
        &["--func", "0.5", "--func-scale"],
    ];
    for (i, flags) in cases.iter().enumerate() {
        let output = tmp.path().join(i.to_string());
        let result = run(&input, &output, flags);
        assert!(!result.status.success(), "accepted {flags:?}");
        assert!(!output.exists(), "created output for {flags:?}");
    }
}

#[test]
fn missing_empty_and_corrupt_inputs_report_failures_and_continue_the_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("output");
    assert!(
        !run(&tmp.path().join("missing.DNG"), &output, &[])
            .status
            .success()
    );
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(!run(&empty, &output, &[]).status.success());
    let (input, _) = fixture(tmp.path());
    let corrupt = tmp.path().join("corrupt.DNG");
    fs::write(&corrupt, b"not a DNG image").unwrap();
    let result = Command::new(BIN)
        .arg(&corrupt)
        .arg(&input)
        .args(["--threads", "2", "-o"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("corrupt.DNG"));
    assert!(!output.join("corrupt.png").exists());
    read_png(&output.join("photo.png"));
    let not_dir = tmp.path().join("not-directory");
    fs::write(&not_dir, b"keep me").unwrap();
    assert!(!run(&input, &not_dir, &[]).status.success());
    assert_eq!(fs::read(not_dir).unwrap(), b"keep me");
}

#[test]
fn thread_counts_do_not_change_encoded_pixels_or_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    let mut reference = None;
    for threads in ["0", "1", "4"] {
        let output = tmp.path().join(threads);
        success(run(&input, &output, &["--threads", threads, "--optimize"]));
        let pair = (
            fs::read(output.join("photo.png")).unwrap(),
            fs::read(output.join("photo.jpg")).unwrap(),
        );
        if let Some(reference) = &reference {
            assert_eq!(&pair, reference);
        } else {
            reference = Some(pair);
        }
    }
}

#[test]
fn console_reports_noise_limited_dr_separately_from_code_span() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("profile.DNG");
    let mut dng = common::Dng::ramp(32, 32);
    dng.noise_profile = Some([0.0, (20.0f64 / 15360.0).powi(2)]);
    dng.write(&path);
    let result = success(run(
        &path,
        &tmp.path().join("output"),
        &["-dark", "0", "-light", "0"],
    ));
    let progress = String::from_utf8_lossy(&result.stderr);
    assert!(
        progress.contains("approx DR 9.6 bits/stops (noise-limited"),
        "{progress}"
    );
    assert!(progress.contains("raw-code span 13.9 bits"));
}

#[test]
fn verbosity_aliases_and_silent_keep_errors_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let (input, _) = fixture(tmp.path());
    for (i, flag) in ["-verbose", "--verbose", "-debug", "--debug", "-v"]
        .into_iter()
        .enumerate()
    {
        let result = success(run(
            &input,
            &tmp.path().join(format!("verbose-{i}")),
            &[flag],
        ));
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains("\"metadata\""));
        assert!(stderr.contains("\"noise\""));
    }
    for (i, flag) in ["-silent", "--silent", "-q"].into_iter().enumerate() {
        let result = success(run(
            &input,
            &tmp.path().join(format!("silent-{i}")),
            &[flag],
        ));
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
    let result = run(
        &tmp.path().join("missing.DNG"),
        &tmp.path().join("error"),
        &["--silent"],
    );
    assert!(!result.status.success());
    assert!(!result.stderr.is_empty());
    assert!(
        !run(
            &input,
            &tmp.path().join("conflict"),
            &["--debug", "--silent"]
        )
        .status
        .success()
    );
    let analysis = success(run(
        &input,
        &tmp.path().join("analysis"),
        &["--analyze", "--silent"],
    ));
    assert!(analysis.stderr.is_empty());
    let _: serde_json::Value = serde_json::from_slice(&analysis.stdout).unwrap();
}

#[test]
fn output_alias_and_end_of_options_handle_flag_like_filenames() {
    let tmp = tempfile::tempdir().unwrap();
    Dng::ramp(32, 16).write(&tmp.path().join("-best.DNG"));
    success(
        Command::new(BIN)
            .current_dir(tmp.path())
            .args(["-output-dir", "out", "--threads=1", "--", "-best.DNG"])
            .output()
            .unwrap(),
    );
    read_png(&tmp.path().join("out/-best.png"));
}

#[cfg(unix)]
#[test]
fn symlink_outputs_are_not_followed_and_directory_loops_are_skipped() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("input");
    fs::create_dir(&input).unwrap();
    let (file, _) = fixture(&input);
    symlink(&input, input.join("loop")).unwrap();
    let output = tmp.path().join("output");
    success(run(&input, &output, &[]));
    fs::remove_file(output.join("photo.png")).unwrap();
    symlink(&file, output.join("photo.png")).unwrap();
    let original = fs::read(&file).unwrap();
    assert!(!run(&file, &output, &["--overwrite"]).status.success());
    assert_eq!(fs::read(file).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn non_utf8_filenames_are_supported() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp
        .path()
        .join(OsString::from_vec(b"photo-\xff.DNG".to_vec()));
    Dng::ramp(32, 16).write(&input);
    let output = tmp.path().join("output");
    success(run(&input, &output, &["--report"]));
    read_png(&output.join(OsString::from_vec(b"photo-\xff.png".to_vec())));
}

#[test]
#[ignore = "set DNG_MONO_SAMPLE to an original Leica DNG; outputs use a temporary directory"]
fn original_leica_sample_produces_both_full_precision_variants() {
    let sample = std::env::var_os("DNG_MONO_SAMPLE").expect("set DNG_MONO_SAMPLE");
    let input = Path::new(&sample);
    let tmp = tempfile::tempdir().unwrap();
    success(run(input, tmp.path(), &["--both", "--report"]));
    for mode in ["auto", "best"] {
        let base = tmp.path().join(mode).join(input.file_name().unwrap());
        let png = read_png(&base.with_extension("png"));
        let (w, h, _) = read_jpeg(&base.with_extension("jpg"));
        assert_eq!((png.width, png.height), (u32::from(w), u32::from(h)));
        let report = json(&base.with_extension("json"));
        assert_eq!(report["range"]["white_level_bits"], 14.0);
        assert_eq!(report["output"]["png_min"], 0);
        assert_eq!(report["output"]["png_max"], 65535);
        assert!(report["output"]["png_occupied_codes"].as_u64().unwrap() > 4096);
    }
}
