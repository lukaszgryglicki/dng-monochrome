mod common;

use dng_monochrome::{
    cli::Cli,
    color::{self, Channel, Green},
    formats::Format,
};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::BufReader,
    path::Path,
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_dng-monochrome");

fn run(input: &Path, output: &Path, flags: &[&str]) -> Output {
    Command::new(BIN)
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(["--threads", "2"])
        .args(flags)
        .output()
        .unwrap()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn png(path: &Path) -> (u32, u32, png::ColorType, Vec<u16>) {
    let mut reader = png::Decoder::new(BufReader::new(File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut bytes).unwrap();
    assert_eq!(frame.bit_depth, png::BitDepth::Sixteen);
    let samples = bytes[..frame.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    (frame.width, frame.height, frame.color_type, samples)
}

fn fixture() -> common::Dng {
    let mut dng = common::Dng::ramp(64, 48);
    dng.cfa = true;
    for y in 0..dng.height {
        for x in 0..dng.width {
            let cell = (y / 2) * (dng.width / 2) + x / 2;
            let phase = (y % 2) * 2 + x % 2;
            dng.pixels[(y * dng.width + x) as usize] =
                (1500 + [3, 5, 7, 11][phase as usize] * cell + phase * 101) as u16;
        }
    }
    dng
}

#[test]
fn color_flags_accept_both_dash_forms_and_require_explicit_color_mode() {
    let parse = |flags: &[&str]| {
        Cli::try_parse_compat(
            ["dng-monochrome", "photo.ARW"]
                .into_iter()
                .chain(flags.iter().copied())
                .map(OsString::from),
        )
    };
    for color in ["--color", "-color", "-c"] {
        for green in ["--g1", "-g1", "--g2", "-g2", "--ga", "-ga"] {
            for channels in ["--channels", "-channels", "-4"] {
                assert!(parse(&[color, green, channels]).is_ok());
            }
        }
        assert!(parse(&[color]).is_ok());
        assert!(parse(&[color, "-none", "-jxl", "-j2k", "-avif"]).is_ok());
    }
    for flag in ["--g1", "--g2", "--ga", "--channels", "-4"] {
        assert!(parse(&[flag]).is_err(), "{flag}");
    }
    assert!(parse(&["--color=true"]).is_err());
    assert!(parse(&["--color", "--channels=true"]).is_err());
    assert!(parse(&["--", "-c", "-4"]).is_ok());
    assert!(parse(&["-func", "-color"]).is_ok());
    assert_eq!(parse(&["-c"]).unwrap().green(), Green::G1);
    for (flags, expected) in [
        (["-c", "-g1", "-g2", "-ga"], Green::Average),
        (["-c", "-ga", "-g1", "-g2"], Green::G2),
        (["-c", "-g2", "-ga", "-g1"], Green::G1),
    ] {
        assert_eq!(parse(&flags).unwrap().green(), expected);
    }
}

#[test]
fn color_planes_match_independent_monochrome_processing_and_selected_rgb() {
    let dir = tempfile::tempdir().unwrap();
    let dng = fixture();
    let input = dir.path().join("photo.DNG");
    dng.write(&input);
    let before = fs::read(&input).unwrap();
    let flags = [
        "-dark",
        "0.5",
        "-light",
        "0.7",
        "-func",
        "sin(pi*x/2)^1.2",
        "-func-scale",
        "-both",
        "-report",
        "-silent",
    ];
    let output = dir.path().join("color");
    let mut color_flags = vec!["-c", "-4", "-g1"];
    color_flags.extend(flags);
    success(run(&input, &output, &color_flags));
    for (phase, channel) in ["R", "G1", "G2", "B"].into_iter().enumerate() {
        let mut mono = common::Dng::ramp(dng.width / 2, dng.height / 2);
        mono.pixels = (0..mono.height)
            .flat_map(|y| {
                let dng = &dng;
                (0..mono.width).map(move |x| {
                    dng.pixels[(2 * y as usize + phase / 2) * dng.width as usize
                        + 2 * x as usize
                        + phase % 2]
                })
            })
            .collect();
        let source = dir.path().join(format!("reference{channel}.DNG"));
        mono.write(&source);
        let reference = dir.path().join(format!("reference{channel}"));
        success(run(&source, &reference, &flags));
        for mode in ["auto", "best"] {
            let saved = png(&output.join(mode).join(format!("photo{channel}.png")));
            let expected = png(&reference.join(mode).join(format!("reference{channel}.png")));
            assert_eq!(saved, expected, "{channel} {mode}");
        }
    }
    for mode in ["auto", "best"] {
        let root = output.join(mode);
        let (width, height, color, rgb) = png(&root.join("photo.png"));
        assert_eq!((width, height, color), (32, 24, png::ColorType::Rgb));
        for (index, channel) in ["R", "G1", "B"].into_iter().enumerate() {
            let plane = png(&root.join(format!("photo{channel}.png"))).3;
            assert_eq!(
                rgb.iter()
                    .skip(index)
                    .step_by(3)
                    .copied()
                    .collect::<Vec<_>>(),
                plane
            );
        }
        assert_eq!(fs::read_dir(root).unwrap().count(), 10);
    }
    for green in ["-g2", "-ga"] {
        let selected = dir.path().join(green);
        let mut selected_flags = vec!["-c", green];
        selected_flags.extend(flags);
        success(run(&input, &selected, &selected_flags));
        for mode in ["auto", "best"] {
            let rgb = png(&selected.join(mode).join("photo.png")).3;
            let reference = output.join(mode);
            let g1 = png(&reference.join("photoG1.png")).3;
            let g2 = png(&reference.join("photoG2.png")).3;
            for (index, (&g1, &g2)) in g1.iter().zip(&g2).enumerate() {
                let expected = if green == "-g2" {
                    g2
                } else {
                    (u32::from(g1) + u32::from(g2)).div_ceil(2) as u16
                };
                assert_eq!(rgb[3 * index + 1], expected, "{green} {mode} pixel {index}");
            }
            assert_eq!(fs::read_dir(selected.join(mode)).unwrap().count(), 2);
        }
    }
    assert_eq!(fs::read(input).unwrap(), before);
}

#[test]
fn all_bayer_layouts_crop_phases_calibration_and_orientations_preserve_samples() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("bayer.DNG");
    let orientations = [
        [0, 1, 2, 3, 4, 5],
        [2, 1, 0, 5, 4, 3],
        [5, 4, 3, 2, 1, 0],
        [3, 4, 5, 0, 1, 2],
        [0, 3, 1, 4, 2, 5],
        [3, 0, 4, 1, 5, 2],
        [5, 2, 4, 1, 3, 0],
        [2, 5, 1, 4, 0, 3],
    ];
    for pattern in [[0, 1, 1, 2], [2, 1, 1, 0], [1, 0, 2, 1], [1, 2, 0, 1]] {
        for orientation in 1..=8u16 {
            let mut dng = common::Dng::ramp(12, 10);
            dng.cfa = true;
            dng.cfa_pattern = pattern;
            dng.orientation = orientation;
            dng.active_area = Some([1, 1, 9, 11]);
            let (dx, dy) = (
                u32::from(orientation - 1) % 2,
                u32::from(orientation - 1) / 2 % 2,
            );
            dng.crop = Some([dx, dy, 7, 5]);
            dng.black_pattern = Some([101, 203, 307, 409]);
            dng.color_noise_profile =
                Some([[0.0001, 0.000002], [0.0003, 0.000004], [0.0005, 0.000006]]);
            dng.pixels = (1000..1120).collect();
            dng.write(&input);
            let image = color::decode_with_ratios(&input, false, Green::G1, true, true).unwrap();
            let mut greens = 0;
            let mut raw_means = [0.0; 4];
            for y in 0..2u32 {
                for x in 0..2u32 {
                    let phase = ((dy + y) % 2 * 2 + (dx + x) % 2) as usize;
                    let channel = match pattern[phase] {
                        0 => Channel::R,
                        2 => Channel::B,
                        1 => {
                            greens += 1;
                            if greens == 1 {
                                Channel::G1
                            } else {
                                Channel::G2
                            }
                        }
                        _ => unreachable!(),
                    };
                    let plane = &image
                        .channels
                        .iter()
                        .find(|(name, _)| *name == channel)
                        .unwrap()
                        .1;
                    let expected: Vec<_> = (0..2u32)
                        .flat_map(|cy| {
                            let dng = &dng;
                            (0..3u32).map(move |cx| {
                                dng.pixels[((1 + dy + 2 * cy + y) * dng.width + 1 + dx + 2 * cx + x)
                                    as usize]
                            })
                        })
                        .collect();
                    let expected: Vec<_> = orientations[orientation as usize - 1]
                        .iter()
                        .map(|&index| expected[index])
                        .collect();
                    let slot = match channel {
                        Channel::R => 0,
                        Channel::G1 => 1,
                        Channel::G2 => 2,
                        Channel::B => 3,
                    };
                    raw_means[slot] = expected.iter().map(|&v| f64::from(v)).sum::<f64>()
                        / expected.len() as f64
                        - f64::from(dng.black_pattern.unwrap()[phase]);
                    assert_eq!(
                        plane.pixels, expected,
                        "{pattern:?} orientation {orientation} {channel:?}"
                    );
                    assert_eq!(
                        plane.metadata.crop,
                        [1 + dx as usize, 1 + dy as usize, 6, 4]
                    );
                    assert_eq!(
                        (plane.metadata.width, plane.metadata.height),
                        if orientation < 5 { (3, 2) } else { (2, 3) }
                    );
                    assert_eq!(plane.metadata.orientation, orientation);
                    assert_eq!(
                        plane.metadata.black_level,
                        f64::from(dng.black_pattern.unwrap()[phase])
                    );
                    assert_eq!(
                        plane.metadata.noise_profile,
                        Some(dng.color_noise_profile.unwrap()[pattern[phase] as usize])
                    );
                }
            }
            let ratios = image.color_ratios.unwrap();
            let expected = [
                raw_means[0],
                (raw_means[1] + raw_means[2]) / 2.0,
                raw_means[3],
            ];
            assert_eq!(ratios.reference_means, expected);
            let maximum = expected.into_iter().fold(0.0, f64::max);
            assert_eq!(ratios.scale_factors, expected.map(|mean| mean / maximum));
            let full = color::decode(&input, true, Green::G2, false).unwrap();
            assert_eq!(
                full.channels
                    .iter()
                    .map(|(channel, _)| *channel)
                    .collect::<Vec<_>>(),
                [Channel::R, Channel::G2, Channel::B]
            );
            assert!(
                full.channels
                    .iter()
                    .all(|(_, plane)| plane.pixels.len() == 30)
            );
        }
    }
}

#[test]
fn packed_bayer_samples_and_endianness_are_not_developed_by_the_decoder() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("packed.DNG");
    for bits in [12, 14, 16] {
        for big_endian in [false, true] {
            let mut dng = common::Dng::ramp(16, 12);
            dng.cfa = true;
            dng.bits = bits;
            dng.big_endian = big_endian;
            dng.black = 0;
            dng.white = (1 << bits) - 1;
            dng.pixels = (0..192).map(|i| (i * dng.white / 191) as u16).collect();
            dng.write(&input);
            let image = color::decode(&input, false, Green::Average, false).unwrap();
            for (phase, (_, plane)) in image.channels.iter().enumerate() {
                let expected: Vec<_> = (0..6)
                    .flat_map(|y| {
                        let dng = &dng;
                        (0..8)
                            .map(move |x| dng.pixels[(2 * y + phase / 2) * 16 + 2 * x + phase % 2])
                    })
                    .collect();
                assert_eq!(
                    plane.pixels, expected,
                    "{bits} big_endian={big_endian} phase={phase}"
                );
                assert_eq!(plane.metadata.storage_bits, usize::from(bits));
                assert_eq!(plane.metadata.white_level, dng.white as u16);
            }
        }
    }
}

#[test]
fn only_requested_planes_are_processed_and_invalid_color_inputs_fail() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("invalid.DNG");
    let mut dng = fixture();
    dng.black_pattern = Some([1023, 1023, dng.white, 1023]);
    dng.write(&input);
    success(run(&input, &dir.path().join("g1"), &["-c", "-silent"]));
    for flag in ["-g2", "-ga", "-4"] {
        let out = dir.path().join(flag);
        let result = run(&input, &out, &["-c", flag, "-silent"]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("calibration"));
        assert!(!out.exists());
    }
    for invalid in 1..6 {
        let mut dng = fixture();
        match invalid {
            1 => dng.cfa_pattern = [0, 0, 1, 2],
            2 => dng.crop = Some([0, 0, 1, 4]),
            3 => dng.color_noise_profile = Some([[0.0, 0.0]; 3]),
            4 => dng.color_noise_profile = Some([[f64::NAN, 1.0]; 3]),
            5 => dng.color_noise_profile = Some([[f64::MAX, 1.0]; 3]),
            _ => unreachable!(),
        }
        dng.write(&input);
        assert!(
            color::decode(&input, false, Green::G1, true).is_err(),
            "case {invalid}"
        );
    }
    fs::write(&input, b"not raw data").unwrap();
    assert!(color::decode(&input, false, Green::G1, true).is_err());
}

#[test]
fn unequal_precision_uses_exact_shifts_before_averaging_greens() {
    let r = [0, 32768, 65535];
    let g1 = [65535, 32768, 0];
    let b = [32768, 65535, 0];
    let (rgb, bits) = color::combine([(&r, 9), (&g1, 11), (&b, 10)], None).unwrap();
    assert_eq!(bits, 11);
    assert_eq!(rgb, [0, 2047, 1024, 1024, 1024, 2046, 2044, 0, 0]);
    let g2 = [0, 65535, 65535];
    let (rgb, bits) = color::combine([(&r, 12), (&g1, 9), (&b, 10)], Some((&g2, 11))).unwrap();
    assert_eq!(bits, 12);
    assert_eq!(rgb, [0, 2044, 2048, 2048, 3072, 4092, 4095, 2048, 0]);
    let ratios = color::ColorRatios {
        reference_means: [80.0, 120.0, 160.0],
        scale_factors: [0.5, 0.75, 1.0],
    };
    let (rgb, bits) = color::combine_with_ratios(
        [(&r, 12), (&g1, 9), (&b, 10)],
        Some((&g2, 11)),
        Some(&ratios),
    )
    .unwrap();
    assert_eq!(bits, 12);
    assert_eq!(rgb, [0, 1533, 2048, 1024, 2304, 4092, 2048, 1536, 0]);
    for depth in [0, 17] {
        assert!(color::combine([(&r, depth), (&g1, 11), (&b, 10)], None).is_err());
    }
    assert!(color::combine([(&r[..2], 9), (&g1, 11), (&b, 10)], None).is_err());
}

#[test]
fn channel_destinations_are_preflighted_and_render_errors_publish_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.DNG");
    let collision = dir.path().join("photoR.DNG");
    fixture().write(&input);
    fixture().write(&collision);
    let out = dir.path().join("collision");
    let result = Command::new(BIN)
        .args(["-c", "-4", "-both", "-o"])
        .arg(&out)
        .arg(&input)
        .arg(&collision)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("same output"));
    assert!(!out.exists());
    let stale = dir.path().join("stale");
    fs::create_dir(&stale).unwrap();
    fs::write(stale.join("photoG2.json"), b"keep").unwrap();
    let result = run(&input, &stale, &["-c", "-4", "--overwrite", "-silent"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("stale"));
    assert_eq!(fs::read_dir(&stale).unwrap().count(), 1);
    assert_eq!(fs::read(stale.join("photoG2.json")).unwrap(), b"keep");
    let bad_function = dir.path().join("function");
    let result = run(
        &input,
        &bad_function,
        &["-c", "-4", "-func", "0/0", "-silent"],
    );
    assert!(!result.status.success());
    assert!(!bad_function.exists());
}

#[test]
fn raw_discovery_is_recursive_deduplicated_and_continues_after_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    fs::create_dir_all(input.join("nested")).unwrap();
    fixture().write(&input.join("a.ArW"));
    fixture().write(&input.join("nested/b.dng"));
    fs::write(input.join("preview.JPG"), b"ignored").unwrap();
    let discovered = dng_monochrome::cli::discover(std::slice::from_ref(&input)).unwrap();
    assert_eq!(discovered.len(), 2);
    assert_eq!(discovered[0].relative, Path::new("a.ArW"));
    assert_eq!(discovered[1].relative, Path::new("nested/b.dng"));
    let out = dir.path().join("analysis");
    let result = success(run(&input, &out, &["-c", "-analyze", "-none", "-silent"]));
    let analyses: Vec<serde_json::Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(analyses.len(), 2);
    assert!(analyses[0]["source"].as_str().unwrap().ends_with("a.ArW"));
    assert!(
        analyses[1]["source"]
            .as_str()
            .unwrap()
            .ends_with("nested/b.dng")
    );
    assert!(
        analyses
            .iter()
            .all(|a| a["channels"].as_object().unwrap().len() == 3 && a["file_threads"] == 1)
    );
    assert!(!out.exists());
    let result = Command::new(BIN)
        .args(["-c", "-analyze", "-none", "-silent", "--threads", "2"])
        .arg(&input)
        .arg(input.join("a.ArW"))
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap().lines().count(), 2);
    fs::write(input.join("0-broken.ARW"), b"bad").unwrap();
    let output = dir.path().join("batch");
    let result = run(&input, &output, &["-c", "-silent"]);
    assert!(!result.status.success());
    assert!(output.join("a.png").exists());
    assert!(output.join("nested/b.png").exists());
    assert!(!output.join("0-broken.png").exists());
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn companion_names_preserve_non_utf8_bytes_and_multiple_dots() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let input = dir
        .path()
        .join(OsString::from_vec(b"photo.v1.\xff.DNG".to_vec()));
    fixture().write(&input);
    let out = dir.path().join("out");
    success(run(&input, &out, &["-c", "-4", "-silent"]));
    for suffix in ["", "R", "G1", "G2", "B"] {
        let mut name = b"photo.v1.\xff".to_vec();
        name.extend_from_slice(suffix.as_bytes());
        name.extend_from_slice(b".png");
        assert!(out.join(OsString::from_vec(name)).is_file());
    }
    assert_eq!(fs::read_dir(out).unwrap().count(), 5);
}

#[test]
fn companion_names_preserve_unicode_on_all_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("photo.\u{017c}\u{4e2d}.DNG");
    fixture().write(&input);
    let out = dir.path().join("out");
    success(run(&input, &out, &["-c", "-4", "-silent"]));
    for suffix in ["", "R", "G1", "G2", "B"] {
        assert!(
            out.join(format!("photo.\u{017c}\u{4e2d}{suffix}.png"))
                .is_file()
        );
    }
    assert_eq!(fs::read_dir(out).unwrap().count(), 5);
}

fn codec_fixture(width: u32, height: u32, dr: [f64; 3]) -> common::Dng {
    let mut dng = common::Dng::ramp(width, height);
    dng.cfa = true;
    dng.black = 0;
    dng.white = 65535;
    dng.color_noise_profile = Some(dr.map(|stops| [0.0, (-2.0 * stops).exp2()]));
    let count = width / 2 * (height / 2);
    for y in 0..height {
        for x in 0..width {
            let phase = (y % 2 * 2 + x % 2) as usize;
            let cell = y / 2 * (width / 2) + x / 2;
            let code = (cell + [0, count / 4, count / 2, count / 3][phase]) % count;
            dng.pixels[(y * width + x) as usize] =
                (u64::from(code) * 65535 / u64::from(count - 1)) as u16;
        }
    }
    dng
}

struct ColorDecoded {
    width: u32,
    height: u32,
    bits: u8,
    pixels: Vec<u16>,
    exif: Vec<u8>,
    transfer: Option<u16>,
}

fn quantized(value: u16, bits: u8) -> u16 {
    ((u64::from(value) * ((1u64 << bits) - 1) + 32767) / 65535) as u16
}

fn decode_color(path: &Path, format: Format) -> ColorDecoded {
    let bytes = fs::read(path).unwrap();
    match format {
        Format::Png => {
            let (width, height, color, pixels) = png(path);
            assert_eq!(color, png::ColorType::Rgb);
            let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                .read_info()
                .unwrap();
            ColorDecoded {
                width,
                height,
                bits: 16,
                pixels,
                exif: reader.info().exif_metadata.as_ref().unwrap().to_vec(),
                transfer: None,
            }
        }
        Format::Jpeg => {
            let mut reader = jpeg_decoder::Decoder::new(bytes.as_slice());
            let pixels = reader
                .decode()
                .unwrap()
                .into_iter()
                .map(u16::from)
                .collect();
            let info = reader.info().unwrap();
            assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
            ColorDecoded {
                width: u32::from(info.width),
                height: u32::from(info.height),
                bits: 8,
                pixels,
                exif: reader.exif_data().unwrap().to_vec(),
                transfer: None,
            }
        }
        Format::Jxl => {
            let image = jxl_oxide::JxlImage::builder()
                .pool(jxl_oxide::JxlThreadPool::none())
                .read(bytes.as_slice())
                .unwrap();
            assert!(!image.pixel_format().is_grayscale());
            assert!(!image.pixel_format().has_alpha());
            let bits = image.image_header().metadata.bit_depth.bits_per_sample() as u8;
            let render = image.render_frame(0).unwrap();
            let mut stream = render.stream();
            let mut pixels = vec![0u16; (image.width() * image.height() * 3) as usize];
            assert_eq!(stream.write_to_buffer(&mut pixels), pixels.len());
            let pixels = pixels
                .into_iter()
                .map(|value| quantized(value, bits))
                .collect();
            ColorDecoded {
                width: image.width(),
                height: image.height(),
                bits,
                pixels,
                exif: image
                    .aux_boxes()
                    .first_exif()
                    .unwrap()
                    .unwrap()
                    .payload()
                    .to_vec(),
                transfer: image.rendered_cicp().map(|cicp| u16::from(cicp[1])),
            }
        }
        Format::J2k => {
            let image = jpeg2k::Image::from_bytes_with(
                &bytes,
                jpeg2k::DecodeParameters::new().strict(true),
            )
            .unwrap();
            assert_eq!(image.num_components(), 3);
            let bits = image.components()[0].precision() as u8;
            assert!(
                image
                    .components()
                    .iter()
                    .all(|c| !c.is_signed() && c.precision() == u32::from(bits))
            );
            let pixels = (0..(image.width() * image.height()) as usize)
                .flat_map(|i| {
                    image
                        .components()
                        .iter()
                        .map(move |c| u16::try_from(c.data()[i]).unwrap())
                })
                .collect();
            let mut offset = 0;
            let exif = loop {
                let size =
                    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                let end = offset + size;
                assert!(size >= 8 && end <= bytes.len());
                if &bytes[offset + 4..offset + 8] == b"uuid"
                    && bytes[offset + 8..end].starts_with(b"JpgTiffExif->JP2")
                {
                    break bytes[offset + 24..end].to_vec();
                }
                offset = end;
                assert!(offset < bytes.len(), "missing photographic EXIF");
            };
            ColorDecoded {
                width: image.width(),
                height: image.height(),
                bits,
                pixels,
                exif,
                transfer: None,
            }
        }
        Format::Heic | Format::Avif => {
            use libheif_rs::{ColorSpace, DecodingOptions, HeifContext, LibHeif, RgbChroma};
            let library = LibHeif::new_checked().unwrap();
            let mut context = HeifContext::read_from_bytes(&bytes).unwrap();
            context.set_max_decoding_threads(1);
            let handle = context.primary_image_handle().unwrap();
            let profile = handle.color_profile_nclx().unwrap();
            assert_eq!(profile.full_range_flag(), 1);
            assert_eq!(profile.matrix_coefficients() as u16, 0);
            let bits = handle.luma_bits_per_pixel();
            let mut options = DecodingOptions::new().unwrap();
            options.set_strict_decoding(true);
            options.set_convert_hdr_to_8bit(false);
            let image = library
                .decode(&handle, ColorSpace::Rgb(RgbChroma::C444), Some(options))
                .unwrap();
            let planes = image.planes();
            let planes = [planes.r.unwrap(), planes.g.unwrap(), planes.b.unwrap()];
            assert!(planes.iter().all(|plane| plane.width == handle.width()
                && plane.height == handle.height()
                && plane.bits_per_pixel == bits));
            let mut pixels = Vec::new();
            for y in 0..handle.height() as usize {
                for x in 0..handle.width() as usize {
                    for plane in &planes {
                        let index = y * plane.stride + x * if bits > 8 { 2 } else { 1 };
                        pixels.push(if bits > 8 {
                            u16::from_ne_bytes(plane.data[index..index + 2].try_into().unwrap())
                        } else {
                            u16::from(plane.data[index])
                        });
                    }
                }
            }
            let mut ids = [0];
            assert_eq!(handle.metadata_block_ids(&mut ids, b"Exif"), 1);
            let metadata = handle.metadata(ids[0]).unwrap();
            let offset = u32::from_be_bytes(metadata[..4].try_into().unwrap()) as usize + 4;
            ColorDecoded {
                width: handle.width(),
                height: handle.height(),
                bits,
                pixels,
                exif: metadata[offset..].to_vec(),
                transfer: Some(profile.transfer_characteristics() as u16),
            }
        }
    }
}

fn assert_color_exif(decoded: &ColorDecoded) {
    use rawler::{
        formats::tiff::{
            Value,
            reader::{GenericTiffReader, TiffReader},
        },
        tags::{ExifTag, TiffCommonTag},
    };
    let tiff = GenericTiffReader::new_with_buffer(&decoded.exif, 0, 0, None).unwrap();
    let root = tiff.root_ifd();
    for (tag, expected) in [
        (TiffCommonTag::ImageWidth, decoded.width),
        (TiffCommonTag::ImageLength, decoded.height),
        (TiffCommonTag::SamplesPerPixel, 3),
        (TiffCommonTag::PhotometricInt, 2),
    ] {
        assert_eq!(root.get_entry(tag).unwrap().force_u32(0), expected);
    }
    assert_eq!(
        root.get_entry(ExifTag::Orientation).unwrap().force_u32(0),
        1
    );
    let Value::Short(bits) = &root.get_entry(TiffCommonTag::BitsPerSample).unwrap().value else {
        panic!("missing RGB depth");
    };
    assert_eq!(bits, &[u16::from(decoded.bits); 3]);
    assert_eq!(
        root.get_entry_recursive(ExifTag::StandardOutputSensitivity)
            .unwrap()
            .force_u32(0),
        125
    );
    assert_eq!(
        root.get_entry_recursive(ExifTag::ExifImageWidth)
            .unwrap()
            .force_u32(0),
        decoded.width
    );
    assert_eq!(
        root.get_entry_recursive(ExifTag::ExifImageHeight)
            .unwrap()
            .force_u32(0),
        decoded.height
    );
}

fn report_depth(report: &serde_json::Value, format: Format) -> u8 {
    let format = serde_json::to_value(format).unwrap();
    report["additional_formats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["format"] == format)
        .unwrap()["bits"]
        .as_u64()
        .unwrap() as u8
}

#[test]
fn all_color_formats_round_trip_channel_precision_transfers_and_exif() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("rgb.DNG");
    for (case, dr, green, transfer, low_bits) in [
        ("eight", [5.2, 6.2, 7.2], "-g1", "linear", false),
        ("ten", [8.2, 9.2, 7.2], "-g2", "srgb", false),
        ("twelve", [10.2, 11.2, 9.2], "-ga", "linear", false),
        ("sixteen", [13.2, 15.2, 14.2], "-ga", "srgb", false),
        ("low", [1.2, 2.2, 3.2], "-ga", "linear", true),
    ] {
        let mut dng = codec_fixture(70, 54, dr);
        dng.orientation = 6;
        dng.write(&input);
        let out = dir.path().join(case);
        let mut flags = vec![
            "-c",
            "-4",
            "-all",
            "-report",
            "-silent",
            "-no-optimize",
            "-dark",
            "0",
            "-light",
            "0",
            "-jpeg-quality",
            "100",
            "-transfer",
            transfer,
            green,
        ];
        if low_bits {
            flags.extend(["-j2k-allow-low-bits", "-jxl-allow-low-bits"]);
        }
        success(run(&input, &out, &flags));
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(out.join("rgb.json")).unwrap()).unwrap();
        let planes =
            ["R", "G1", "G2", "B"].map(|channel| png(&out.join(format!("rgb{channel}.png"))).3);
        let selected = if green == "-g2" { [0, 2, 3] } else { [0, 1, 3] };
        for format in [
            Format::Png,
            Format::Jpeg,
            Format::Heic,
            Format::Avif,
            Format::J2k,
            Format::Jxl,
        ] {
            let decoded = decode_color(&out.join(format!("rgb.{}", format.extension())), format);
            assert_eq!((decoded.width, decoded.height), (27, 35));
            assert_color_exif(&decoded);
            if let Some(transfer_code) = decoded.transfer {
                assert_eq!(transfer_code, if transfer == "srgb" { 13 } else { 8 });
            }
            let depths = if format == Format::Png {
                [16; 4]
            } else if format == Format::Jpeg {
                [8; 4]
            } else {
                ["R", "G1", "G2", "B"].map(|name| report_depth(&report["channels"][name], format))
            };
            let green_depth = if green == "-ga" {
                depths[1].max(depths[2])
            } else {
                depths[selected[1]]
            };
            let bits = depths[0].max(green_depth).max(depths[3]);
            assert_eq!(decoded.bits, bits, "{case} {format}");
            let expected: Vec<_> = (0..planes[0].len())
                .flat_map(|index| {
                    let code = |channel: usize, depth| {
                        u32::from(quantized(planes[channel][index], depths[channel]))
                            << (depth - depths[channel])
                    };
                    let green_code = if green == "-ga" {
                        (code(1, green_depth) + code(2, green_depth)).div_ceil(2)
                            << (bits - green_depth)
                    } else {
                        code(selected[1], bits)
                    };
                    [
                        code(0, bits) as u16,
                        green_code as u16,
                        code(3, bits) as u16,
                    ]
                })
                .collect();
            if format == Format::Jpeg {
                let error: u64 = decoded
                    .pixels
                    .iter()
                    .zip(&expected)
                    .map(|(&a, &b)| u64::from(a.abs_diff(b)))
                    .sum();
                assert!(
                    error < expected.len() as u64 * 2,
                    "RGB JPEG error {case}: {error}"
                );
            } else {
                assert_eq!(decoded.pixels, expected, "{case} {format}");
            }
            if !matches!(format, Format::Png | Format::Jpeg) {
                assert_eq!(report_depth(&report, format), bits);
            }
        }
        assert_eq!(fs::read_dir(out).unwrap().count(), 35);
    }
}

#[test]
fn unused_green_does_not_inflate_rgb_precision_but_average_includes_it() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("precision.DNG");
    let mut dng = codec_fixture(64, 48, [8.2, 9.2, 7.2]);
    for y in (1..dng.height).step_by(2) {
        for x in (0..dng.width).step_by(2) {
            dng.pixels[(y * dng.width + x) as usize] = 12345;
        }
    }
    dng.write(&input);
    for green in ["-g1", "-ga"] {
        let out = dir.path().join(green);
        success(run(
            &input,
            &out,
            &[
                "-c", "-4", "-none", "-jxl", "-j2k", "-report", "-silent", "-dark", "0", "-light",
                "0", green,
            ],
        ));
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(out.join("precision.json")).unwrap()).unwrap();
        assert!(report["channels"]["G2"]["range"]["dynamic_range"]["snr1_stops"].is_null());
        for format in [Format::J2k, Format::Jxl] {
            assert_eq!(report_depth(&report["channels"]["G2"], format), 16);
            let expected = if green == "-ga" { 16 } else { 10 };
            assert_eq!(report_depth(&report, format), expected);
            assert_eq!(
                decode_color(
                    &out.join(format!("precision.{}", format.extension())),
                    format
                )
                .bits,
                expected
            );
        }
    }
}

#[test]
fn rgb_heic_partial_grid_tiles_preserve_odd_dimensions_and_edges() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("grid.DNG");
    codec_fixture(2054, 6, [10.2, 11.2, 9.2]).write(&input);
    let out = dir.path().join("out");
    success(run(
        &input,
        &out,
        &[
            "-c",
            "-4",
            "-heic",
            "-report",
            "-silent",
            "-no-optimize",
            "-dark",
            "0",
            "-light",
            "0",
        ],
    ));
    let decoded = decode_color(&out.join("grid.heic"), Format::Heic);
    assert_eq!((decoded.width, decoded.height, decoded.bits), (1027, 3, 12));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(out.join("grid.json")).unwrap()).unwrap();
    for (component, channel) in ["R", "G1", "B"].into_iter().enumerate() {
        let plane = png(&out.join(format!("grid{channel}.png"))).3;
        let depth = report_depth(&report["channels"][channel], Format::Heic);
        let expected: Vec<_> = plane
            .into_iter()
            .map(|p| quantized(p, depth) << (12 - depth))
            .collect();
        assert_eq!(
            decoded
                .pixels
                .iter()
                .skip(component)
                .step_by(3)
                .copied()
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn lossy_color_formats_keep_three_channels_and_the_requested_depth() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("lossy.DNG");
    codec_fixture(64, 48, [9.2; 3]).write(&input);
    let out = dir.path().join("out");
    success(run(
        &input,
        &out,
        &[
            "-c",
            "-ga",
            "-all",
            "-lossy",
            "-report",
            "-silent",
            "-no-optimize",
            "-dark",
            "0",
            "-light",
            "0",
            "-heic-quality",
            "95",
            "-avif-quality",
            "95",
            "-j2k-quality",
            "98",
            "-jxl-quality",
            "95",
        ],
    ));
    let master = png(&out.join("lossy.png")).3;
    for format in [Format::Heic, Format::Avif, Format::J2k, Format::Jxl] {
        let decoded = decode_color(&out.join(format!("lossy.{}", format.extension())), format);
        assert_eq!((decoded.width, decoded.height, decoded.bits), (32, 24, 10));
        let error: u64 = decoded
            .pixels
            .iter()
            .zip(&master)
            .map(|(&a, &b)| u64::from(a.abs_diff(quantized(b, 10))))
            .sum();
        assert!(
            error < master.len() as u64 * 100,
            "{format}: mean error exceeds 10% of the range"
        );
        assert_color_exif(&decoded);
    }
    assert_eq!(fs::read_dir(out).unwrap().count(), 7);
}

fn ratio_fixture() -> common::Dng {
    let mut dng = fixture();
    for y in 0..dng.height {
        for x in 0..dng.width {
            let phase = (y % 2 * 2 + x % 2) as usize;
            let cell = y / 2 * (dng.width / 2) + x / 2;
            let variation = (cell % 8) as i32 * 10 - 35;
            dng.pixels[(y * dng.width + x) as usize] =
                (dng.black as i32 + [80, 100, 140, 160][phase] + variation) as u16;
        }
    }
    dng
}

#[test]
fn keep_color_ratios_scales_only_rgb_from_black_corrected_raw_means() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("ratios.DNG");
    ratio_fixture().write(&input);
    let before = fs::read(&input).unwrap();
    let baseline = dir.path().join("independent");
    let corrected = dir.path().join("ratios");
    let flags = [
        "-c",
        "-4",
        "-all",
        "-report",
        "-silent",
        "-no-optimize",
        "-dark",
        "0",
        "-light",
        "0",
        "-transfer",
        "srgb",
        "-func",
        "x^1.2",
        "-jpeg-quality",
        "100",
    ];
    success(run(&input, &baseline, &flags));
    let mut corrected_flags = flags.to_vec();
    corrected_flags.push("-kc");
    success(run(&input, &corrected, &corrected_flags));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(corrected.join("ratios.json")).unwrap()).unwrap();
    assert_eq!(report["keep_color_ratios"], true);
    assert_eq!(
        report["color_ratios"]["reference_means"],
        serde_json::json!([80.0, 120.0, 160.0])
    );
    assert_eq!(
        report["color_ratios"]["scale_factors"],
        serde_json::json!([0.5, 0.75, 1.0])
    );
    for format in [
        Format::Png,
        Format::Jpeg,
        Format::Heic,
        Format::Avif,
        Format::J2k,
        Format::Jxl,
    ] {
        let name = format!("ratios.{}", format.extension());
        let original = decode_color(&baseline.join(&name), format);
        let output = decode_color(&corrected.join(&name), format);
        assert_eq!(
            (original.width, original.height, original.bits),
            (output.width, output.height, output.bits)
        );
        let expected: Vec<_> = original
            .pixels
            .iter()
            .enumerate()
            .map(|(index, &value)| (f64::from(value) * [0.5, 0.75, 1.0][index % 3]).round() as u16)
            .collect();
        if format == Format::Jpeg {
            let error: u64 = output
                .pixels
                .iter()
                .zip(&expected)
                .map(|(&a, &b)| u64::from(a.abs_diff(b)))
                .sum();
            assert!(error < expected.len() as u64 * 2);
        } else {
            assert_eq!(output.pixels, expected, "{format}");
            assert_eq!(
                output
                    .pixels
                    .iter()
                    .skip(2)
                    .step_by(3)
                    .copied()
                    .collect::<Vec<_>>(),
                original
                    .pixels
                    .iter()
                    .skip(2)
                    .step_by(3)
                    .copied()
                    .collect::<Vec<_>>(),
                "dominant blue changed"
            );
        }
        for channel in ["R", "G1", "G2", "B"] {
            let name = format!("ratios{channel}.{}", format.extension());
            assert_eq!(
                fs::read(baseline.join(&name)).unwrap(),
                fs::read(corrected.join(name)).unwrap(),
                "{channel} {format}"
            );
        }
    }
    for channel in ["R", "G1", "G2", "B"] {
        let name = format!("ratios{channel}.json");
        assert_eq!(
            fs::read(baseline.join(&name)).unwrap(),
            fs::read(corrected.join(name)).unwrap()
        );
    }
    assert_eq!(fs::read(input).unwrap(), before);
}

#[test]
fn keep_color_ratio_aliases_require_color_and_keep_option_boundaries() {
    let parse = |flags: &[&str]| {
        Cli::try_parse_compat(
            ["dng-monochrome", "photo.ARW"]
                .into_iter()
                .chain(flags.iter().copied())
                .map(OsString::from),
        )
    };
    for flag in [
        "--keep-color-ratios",
        "-keep-color-ratios",
        "-kc",
        "-kcr",
        "--kc",
        "--kcr",
    ] {
        assert!(parse(&["-c", flag]).is_ok(), "{flag}");
        assert!(parse(&[flag]).is_err(), "{flag} without --color");
        assert!(parse(&["--", flag]).is_ok());
        assert_eq!(
            parse(&["-func", flag]).unwrap().function.as_deref(),
            Some(flag)
        );
    }
    assert!(parse(&["-c", "--keep-color-ratios=true"]).is_err());
}

#[test]
fn raw_ratio_reference_uses_both_greens_without_developing_unused_green() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("reference.DNG");
    ratio_fixture().write(&input);
    for (green, threads) in ["-g1", "-g2", "-ga"]
        .into_iter()
        .flat_map(|green| ["1", "2"].map(|threads| (green, threads)))
    {
        let out = dir.path().join(green);
        let result = success(
            Command::new(BIN)
                .arg(&input)
                .arg("-o")
                .arg(&out)
                .args([
                    "-c",
                    "-kcr",
                    green,
                    "-analyze",
                    "-none",
                    "-silent",
                    "--threads",
                    threads,
                ])
                .output()
                .unwrap(),
        );
        let analysis: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(analysis["file_threads"], threads.parse::<u64>().unwrap());
        assert_eq!(
            analysis["color_ratios"]["reference_means"],
            serde_json::json!([80.0, 120.0, 160.0])
        );
        assert_eq!(
            analysis["color_ratios"]["scale_factors"],
            serde_json::json!([0.5, 0.75, 1.0])
        );
        assert_eq!(
            analysis["channels"].as_object().unwrap().len(),
            if green == "-ga" { 4 } else { 3 }
        );
        assert!(!out.exists());
    }
}

#[test]
fn raw_ratio_reference_clamps_negative_signals_and_handles_zero_or_tied_means() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("black.DNG");
    let mut dng = ratio_fixture();
    dng.black_pattern = Some([101, 203, 307, 409]);
    for (signals, expected) in [
        ([-10, 0, 0, 0], None),
        ([-10, 20, 20, 20], Some([0.0, 1.0, 1.0])),
        ([50, 20, 20, 10], Some([1.0, 0.4, 0.2])),
        ([10, 30, 50, 20], Some([0.25, 1.0, 0.5])),
    ] {
        for y in 0..dng.height {
            for x in 0..dng.width {
                let phase = (y % 2 * 2 + x % 2) as usize;
                dng.pixels[(y * dng.width + x) as usize] =
                    (dng.black_pattern.unwrap()[phase] as i32 + signals[phase]) as u16;
            }
        }
        dng.write(&input);
        let result = success(run(
            &input,
            &dir.path().join("unused"),
            &["-c", "-kc", "-analyze", "-none"],
        ));
        let analysis: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        if let Some(expected) = expected {
            assert_eq!(
                analysis["color_ratios"]["scale_factors"],
                serde_json::json!(expected)
            );
        } else {
            assert!(analysis["color_ratios"].is_null());
            assert!(String::from_utf8_lossy(&result.stderr).contains("color ratios unavailable"));
        }
    }
}

#[test]
#[ignore = "set DNG_COLOR_SAMPLE to an original raw; runs full-resolution color through all six formats"]
fn original_color_sample_preserves_independent_channel_codes_in_all_formats() {
    let input = std::path::PathBuf::from(
        std::env::var_os("DNG_COLOR_SAMPLE").expect("set DNG_COLOR_SAMPLE"),
    );
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join("channels");
    let output = dir.path().join("color");
    let binary = std::env::var_os("DNG_COLOR_BINARY").unwrap_or_else(|| OsString::from(BIN));
    let convert = |output: &Path, flags: &[&str]| {
        success(
            Command::new(&binary)
                .arg(&input)
                .arg("-o")
                .arg(output)
                .args(["--threads", "2"])
                .args(flags)
                .output()
                .unwrap(),
        )
    };
    let filename = |channel: &str, extension: &str| {
        let mut name = input.file_stem().unwrap().to_os_string();
        name.push(channel);
        name.push(".");
        name.push(extension);
        name
    };
    convert(
        &reference,
        &["-c", "-4", "-kc", "-report", "-silent", "-no-optimize"],
    );
    convert(
        &output,
        &[
            "-c",
            "-kc",
            "-all",
            "-report",
            "-silent",
            "-no-optimize",
            "-jpeg-quality",
            "100",
        ],
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join(filename("", "json"))).unwrap()).unwrap();
    let reference_report: serde_json::Value =
        serde_json::from_slice(&fs::read(reference.join(filename("", "json"))).unwrap()).unwrap();
    assert_eq!(report["color_ratios"], reference_report["color_ratios"]);
    assert_eq!(report["channels"].as_object().unwrap().len(), 3);
    let planes = ["R", "G1", "B"].map(|channel| png(&reference.join(filename(channel, "png"))));
    let (width, height) = (planes[0].0, planes[0].1);
    assert!(
        planes
            .iter()
            .all(|plane| (plane.0, plane.1, plane.2) == (width, height, png::ColorType::Grayscale))
    );
    let ratios = [0, 1, 2].map(|c| report["color_ratios"]["scale_factors"][c].as_f64().unwrap());
    let depth = |report: &serde_json::Value, format: Format| -> u8 {
        match format {
            Format::Png => 16,
            Format::Jpeg => 8,
            _ => report["additional_formats"]
                .as_array()
                .unwrap()
                .iter()
                .find(|plan| plan["format"] == serde_json::to_value(format).unwrap())
                .unwrap()["bits"]
                .as_u64()
                .unwrap() as u8,
        }
    };
    for format in [
        Format::Png,
        Format::Jpeg,
        Format::Heic,
        Format::Avif,
        Format::J2k,
        Format::Jxl,
    ] {
        let decoded = decode_color(&output.join(filename("", format.extension())), format);
        let bits = depth(&report, format);
        let depths = ["R", "G1", "B"].map(|channel| depth(&report["channels"][channel], format));
        assert_eq!(
            (decoded.width, decoded.height, decoded.bits),
            (width, height, bits)
        );
        assert_eq!(decoded.pixels.len(), planes[0].3.len() * 3);
        assert!(!decoded.exif.is_empty());
        let mut error = 0u64;
        for (index, &actual) in decoded.pixels.iter().enumerate() {
            let c = index % 3;
            let code = quantized(planes[c].3[index / 3], depths[c]) << (bits - depths[c]);
            let expected = (f64::from(code) * ratios[c]).round() as u16;
            error += u64::from(actual.abs_diff(expected));
            if format != Format::Jpeg {
                assert_eq!(actual, expected, "{format} sample {index}");
            }
        }
        if format == Format::Jpeg {
            assert!(
                error < decoded.pixels.len() as u64 * 3,
                "JPEG mean error exceeded three codes"
            );
        }
        eprintln!(
            "{format}: {width}x{height}, {bits} bits, {} samples, mean error {:.6}",
            decoded.pixels.len(),
            error as f64 / decoded.pixels.len() as f64
        );
    }
    assert_eq!(fs::read_dir(reference).unwrap().count(), 10);
    assert_eq!(fs::read_dir(output).unwrap().count(), 7);
}

#[test]
fn forced_color_uses_raw_origin_for_cropped_oriented_monochrome_data() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("mono.DNG");
    let mut dng = common::Dng::ramp(10, 8);
    dng.crop = Some([1, 1, 7, 5]);
    dng.orientation = 6;
    dng.write(&input);
    let image = color::decode(&input, false, Green::G1, true).unwrap();
    assert!(image.cfa_assumed);
    assert_eq!(image.cfa, "BGGR");
    for (channel, plane) in &image.channels {
        let (ox, oy) = match channel {
            Channel::R => (2, 2),
            Channel::G1 => (2, 1),
            Channel::G2 => (1, 2),
            Channel::B => (1, 1),
        };
        let unrotated: Vec<_> = (0..2)
            .flat_map(|y| {
                let dng = &dng;
                (0..3).map(move |x| dng.pixels[(oy + 2 * y) * 10 + ox + 2 * x])
            })
            .collect();
        let expected = [3, 0, 4, 1, 5, 2].map(|i| unrotated[i]);
        assert_eq!(plane.pixels, expected, "{channel:?}");
        assert_eq!(plane.metadata.crop, [1, 1, 6, 4]);
        assert_eq!((plane.metadata.width, plane.metadata.height), (2, 3));
    }
}

#[test]
fn explicit_mode_overrides_color_label_for_both_raw_extensions() {
    let dir = tempfile::tempdir().unwrap();
    for cfa in [false, true] {
        for extension in ["DNG", "ARW"] {
            let mut dng = fixture();
            dng.cfa = cfa;
            let input = dir.path().join(format!("photo.{extension}"));
            dng.write(&input);
            let mono = dir.path().join(format!("mono-{cfa}-{extension}"));
            success(run(&input, &mono, &["-silent"]));
            let image = png(&mono.join("photo.png"));
            assert_eq!(
                (image.0, image.1, image.2),
                (64, 48, png::ColorType::Grayscale)
            );
            assert_eq!(
                dng_monochrome::raw::decode(&input, false).unwrap().pixels,
                dng.pixels
            );
            let rgb = dir.path().join(format!("rgb-{cfa}-{extension}"));
            success(run(&input, &rgb, &["-c", "-4", "-report", "-silent"]));
            let image = png(&rgb.join("photo.png"));
            assert_eq!((image.0, image.1, image.2), (32, 24, png::ColorType::Rgb));
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(rgb.join("photo.json")).unwrap()).unwrap();
            assert_eq!(report["cfa"], "RGGB");
            assert_eq!(report["cfa_assumed"], !cfa);
            for (component, channel) in ["R", "G1", "B"].into_iter().enumerate() {
                assert_eq!(
                    image
                        .3
                        .iter()
                        .skip(component)
                        .step_by(3)
                        .copied()
                        .collect::<Vec<_>>(),
                    png(&rgb.join(format!("photo{channel}.png"))).3
                );
            }
        }
    }
}
