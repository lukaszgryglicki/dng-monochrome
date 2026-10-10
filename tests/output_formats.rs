mod common;

use dng_monochrome::{
    cli::Cli,
    expression::FunctionPolicy,
    formats::{Compression, Encoding, Format},
    output::{self, OutputPaths},
    parameters::Parameters,
    range::{self, Histogram, RangeOptions},
    raw::{self, MonoImage},
    tone::{self, Rendered, Transfer, Transfers},
};
use libheif_rs::{ColorSpace, DecodingOptions, HeifContext, LibHeif};
use std::{
    ffi::OsString,
    fs,
    path::Path,
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_dng-monochrome");

fn parse(flags: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_compat(
        ["dng-monochrome", "photo.DNG"]
            .into_iter()
            .chain(flags.iter().copied())
            .map(OsString::from),
    )
}

fn run(input: &Path, output: &Path, flags: &[&str]) -> Output {
    Command::new(BIN)
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(["--threads", "1"])
        .args(flags)
        .output()
        .unwrap()
}

fn success(result: Output) -> Output {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result
}

fn fixture(root: &Path, width: usize, height: usize) -> (MonoImage, Rendered) {
    let input = root.join("fixture.DNG");
    common::Dng::ramp(32, 32).write(&input);
    let mut image = raw::decode(&input, false).unwrap();
    let histogram = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &histogram, RangeOptions::default()).unwrap();
    let mut rendered = tone::render(
        &image,
        &histogram,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        Transfers::default(),
    )
    .unwrap();
    image.metadata.width = width;
    image.metadata.height = height;
    image.pixels.resize(width * height, 1023);
    let count = width * height;
    rendered.png = (0..count)
        .map(|i| {
            if count == 1 {
                32768
            } else {
                (i as u64 * 65535 / (count - 1) as u64) as u16
            }
        })
        .collect();
    rendered.jpeg = rendered
        .png
        .iter()
        .map(|&v| ((u32::from(v) + 128) / 257) as u8)
        .collect();
    (image, rendered)
}

fn save(
    root: &Path,
    image: &MonoImage,
    rendered: &Rendered,
    encoding: &Encoding,
    transfer: Transfer,
) -> std::path::PathBuf {
    let paths = OutputPaths::with_formats(root, false, &[encoding.format]);
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| {
            output::save_with_encodings(
                &paths,
                image,
                rendered,
                &serde_json::json!({}),
                Transfers {
                    png: transfer,
                    jpeg: Transfer::Linear,
                },
                90,
                false,
                &Parameters::default(),
                std::slice::from_ref(encoding),
            )
            .unwrap()
        });
    paths.path(encoding.format).to_owned()
}

struct Decoded {
    width: u32,
    height: u32,
    bits: u8,
    pixels: Vec<u16>,
    exif: Vec<u8>,
    transfer: Option<u16>,
}

fn decode(path: &Path, format: Format) -> Decoded {
    let bytes = fs::read(path).unwrap();
    if format == Format::J2k {
        let image =
            jpeg2k::Image::from_bytes_with(&bytes, jpeg2k::DecodeParameters::new().strict(true))
                .unwrap();
        assert_eq!(image.num_components(), 1);
        let component = &image.components()[0];
        assert!(!component.is_signed());
        let mut offset = 0usize;
        let exif = loop {
            let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            assert!(size >= 8);
            let end = offset + size;
            if &bytes[offset + 4..offset + 8] == b"uuid"
                && bytes[offset + 8..end].starts_with(b"JpgTiffExif->JP2")
            {
                break bytes[offset + 24..end].to_vec();
            }
            offset = end;
            assert!(offset < bytes.len(), "missing JP2 EXIF UUID");
        };
        Decoded {
            width: image.width(),
            height: image.height(),
            bits: component.precision() as u8,
            pixels: component
                .data()
                .iter()
                .map(|&v| u16::try_from(v).unwrap())
                .collect(),
            exif,
            transfer: None,
        }
    } else {
        let library = LibHeif::new_checked().unwrap();
        let mut context = HeifContext::read_from_bytes(&bytes).unwrap();
        context.set_max_decoding_threads(1);
        let handle = context.primary_image_handle().unwrap();
        assert!(!handle.has_alpha_channel());
        assert_eq!(
            handle.preferred_decoding_colorspace().unwrap(),
            ColorSpace::Monochrome
        );
        let bits = handle.luma_bits_per_pixel();
        let profile = handle
            .color_profile_nclx()
            .expect("explicit NCLX transfer is required");
        assert_eq!(profile.full_range_flag(), 1);
        let transfer = profile.transfer_characteristics() as u16;
        let mut ids = [0; 1];
        assert_eq!(handle.metadata_block_ids(&mut ids, b"Exif"), 1);
        let metadata = handle.metadata(ids[0]).unwrap();
        let tiff_offset = u32::from_be_bytes(metadata[..4].try_into().unwrap()) as usize + 4;
        let exif = metadata[tiff_offset..].to_vec();
        let mut options = DecodingOptions::new().unwrap();
        options.set_strict_decoding(true);
        options.set_convert_hdr_to_8bit(false);
        let decoded = library
            .decode(&handle, ColorSpace::Monochrome, Some(options))
            .unwrap();
        let plane = decoded.planes().y.unwrap();
        let sample_bytes = if bits > 8 { 2 } else { 1 };
        let pixels = plane
            .data
            .chunks_exact(plane.stride)
            .take(plane.height as usize)
            .flat_map(|row| row[..plane.width as usize * sample_bytes].chunks_exact(sample_bytes))
            .map(|sample| {
                if sample_bytes == 1 {
                    u16::from(sample[0])
                } else {
                    u16::from_ne_bytes([sample[0], sample[1]])
                }
            })
            .collect();
        Decoded {
            width: plane.width,
            height: plane.height,
            bits,
            pixels,
            exif,
            transfer: Some(transfer),
        }
    }
}

fn quantized(pixels: &[u16], bits: u8) -> Vec<u16> {
    let maximum = (1u64 << bits) - 1;
    pixels
        .iter()
        .map(|&v| ((u64::from(v) * maximum + 65535 / 2) / 65535) as u16)
        .collect()
}

#[test]
fn flags_are_independent_support_both_dash_styles_and_last_switch_wins() {
    assert_eq!(parse(&[]).unwrap().formats.enabled(), [Format::Png]);
    assert_eq!(
        parse(&["-jpg"]).unwrap().formats.enabled(),
        [Format::Png, Format::Jpeg]
    );
    let all = parse(&["-no-png", "-heic", "--avif", "-j2k"]).unwrap();
    assert_eq!(
        all.formats.enabled(),
        [Format::Heic, Format::Avif, Format::J2k]
    );
    let encodings = all.formats.encodings(Some(9.25)).unwrap();
    assert!(
        encodings
            .iter()
            .all(|e| e.compression == Compression::Lossless && e.quality.is_none())
    );
    assert_eq!(
        encodings.iter().map(|e| e.bits).collect::<Vec<_>>(),
        [10, 10, 10]
    );
    for (enable, disable, format) in [
        ("-png", "--no-png", Format::Png),
        ("--jpg", "-no-jpg", Format::Jpeg),
        ("-heic", "--no-heic", Format::Heic),
        ("--avif", "-no-avif", Format::Avif),
        ("-j2k", "--no-j2k", Format::J2k),
    ] {
        let disabled = parse(&["--analyze", enable, disable]).unwrap();
        assert!(!disabled.formats.enabled().contains(&format));
        let enabled = parse(&["--analyze", disable, enable]).unwrap();
        assert!(enabled.formats.enabled().contains(&format));
    }
    assert!(parse(&["-no-png"]).is_err());
    assert!(parse(&["-analyze", "-no-png"]).is_ok());
}

#[test]
fn codec_modes_and_qualities_override_only_the_requested_format() {
    let cli = parse(&[
        "-heic",
        "-avif",
        "-j2k",
        "-lossy",
        "-avif-mode",
        "lossless",
        "-heic-quality=97",
        "--j2k-quality",
        "42",
    ])
    .unwrap();
    let encodings = cli.formats.encodings(Some(11.1)).unwrap();
    assert_eq!(encodings[0].quality, Some(97));
    assert_eq!(encodings[1].compression, Compression::Lossless);
    assert_eq!(encodings[1].quality, None);
    assert_eq!(encodings[2].quality, Some(42));
    for (first, last, expected) in [
        ("-lossy", "--lossless", Compression::Lossless),
        ("-lossless", "--lossy", Compression::Lossy),
    ] {
        let cli = parse(&["-heic", first, last]).unwrap();
        let encoding = &cli.formats.encodings(Some(10.0)).unwrap()[0];
        assert_eq!(encoding.compression, expected);
        if expected == Compression::Lossy {
            assert_eq!(encoding.quality, Some(90));
        }
    }
    for option in [
        "-heic-quality",
        "--avif-quality",
        "-j2k-quality",
        "-jpg-quality",
    ] {
        for invalid in ["0", "101", "-1", "invalid"] {
            assert!(parse(&[option, invalid]).is_err());
        }
    }
}

#[test]
fn lossless_codecs_preserve_every_selected_depth_sample_and_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let (image, rendered) = fixture(tmp.path(), 256, 256);
    let cases = [Format::Heic, Format::Avif]
        .into_iter()
        .flat_map(|format| [8, 10, 12].map(|bits| (format, bits)))
        .chain((1..=16).map(|bits| (Format::J2k, bits)));
    for (format, bits) in cases {
        let encoding =
            Encoding::new(format, Some(f64::from(bits)), Compression::Lossless, 90).unwrap();
        let path = save(
            &tmp.path().join(format!("{format}-{bits}")),
            &image,
            &rendered,
            &encoding,
            Transfer::Linear,
        );
        let decoded = decode(&path, format);
        assert_eq!(
            (decoded.width, decoded.height, decoded.bits),
            (256, 256, bits),
            "{format}"
        );
        assert_eq!(
            decoded.pixels,
            quantized(&rendered.png, bits),
            "{format} {bits}-bit"
        );
        assert!(decoded.exif.starts_with(b"II\x2a\0") || decoded.exif.starts_with(b"MM\0\x2a"));
        assert!(
            decoded
                .exif
                .windows(b"Recorded manual M lens".len())
                .any(|v| v == b"Recorded manual M lens")
        );
        if format != Format::J2k {
            assert_eq!(decoded.transfer, Some(8));
        }
        assert!(!path.with_extension("png").exists());
        assert!(!path.with_extension("jpg").exists());
    }
}

#[test]
fn odd_and_tiny_geometry_and_srgb_transfer_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    for (width, height) in [
        (1, 1),
        (19, 13),
        (31, 31),
        (32, 32),
        (63, 63),
        (64, 64),
        (1, 129),
        (129, 1),
    ] {
        let (image, rendered) = fixture(tmp.path(), width, height);
        for format in [Format::Heic, Format::Avif, Format::J2k] {
            let encoding = Encoding::new(format, Some(11.5), Compression::Lossless, 90).unwrap();
            let path = save(
                &tmp.path().join(format!("{format}-{width}x{height}")),
                &image,
                &rendered,
                &encoding,
                Transfer::Srgb,
            );
            let decoded = decode(&path, format);
            assert_eq!(
                (decoded.width, decoded.height),
                (width as u32, height as u32)
            );
            assert_eq!(decoded.pixels, quantized(&rendered.png, 12), "{format}");
            if format != Format::J2k {
                assert_eq!(decoded.transfer, Some(13));
            }
        }
    }
}

#[test]
fn lossy_quality_changes_compression_and_reconstruction_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (image, mut rendered) = fixture(tmp.path(), 64, 64);
    let mut state = 0x51f15eadu32;
    for value in &mut rendered.png {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *value = state as u16;
    }
    for format in [Format::Heic, Format::Avif, Format::J2k] {
        let mut errors = Vec::new();
        let mut sizes = Vec::new();
        for quality in [20, 95] {
            let encoding = Encoding::new(format, Some(11.5), Compression::Lossy, quality).unwrap();
            let path = save(
                &tmp.path().join(format!("{format}-q{quality}")),
                &image,
                &rendered,
                &encoding,
                Transfer::Linear,
            );
            let decoded = decode(&path, format);
            assert_eq!((decoded.width, decoded.height, decoded.bits), (64, 64, 12));
            let error: u64 = decoded
                .pixels
                .iter()
                .zip(quantized(&rendered.png, 12))
                .map(|(&actual, expected)| i64::from(actual).abs_diff(i64::from(expected)).pow(2))
                .sum();
            errors.push(error);
            sizes.push(fs::metadata(path).unwrap().len());
        }
        assert!(
            errors[1] < errors[0],
            "{format} reconstruction errors: {errors:?}"
        );
        assert!(sizes[1] > sizes[0], "{format} encoded sizes: {sizes:?}");
    }
}

#[test]
fn cli_can_save_all_new_formats_without_png_and_reports_unknown_dr() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(32, 16).write(&input);
    let out = tmp.path().join("out");
    let result = success(run(
        &input,
        &out,
        &["-no-png", "-heic", "-avif", "-j2k", "-report"],
    ));
    assert!(!out.join("photo.png").exists());
    assert!(!out.join("photo.jpg").exists());
    assert!(String::from_utf8_lossy(&result.stderr).contains("noise-based DR unavailable"));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(out.join("photo.json")).unwrap()).unwrap();
    assert!(report["range"]["dynamic_range"]["snr1_stops"].is_null());
    for (index, format, bits) in [
        (0, Format::Heic, 12),
        (1, Format::Avif, 12),
        (2, Format::J2k, 16),
    ] {
        assert_eq!(
            decode(&out.join(format!("photo.{}", format.extension())), format).bits,
            bits
        );
        assert_eq!(report["additional_formats"][index]["bits"], bits);
        assert_eq!(
            report["additional_formats"][index]["compression"],
            "lossless"
        );
        assert!(report["additional_formats"][index]["measured_dr"].is_null());
    }
}

#[test]
fn automatic_precision_uses_the_existing_noise_estimate_not_raw_span() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("profile.DNG");
    let mut dng = common::Dng::ramp(128, 64);
    dng.noise_profile = Some([0.0, 1e-6]);
    dng.write(&input);
    let out = tmp.path().join("out");
    success(run(&input, &out, &["-heic", "-report", "-silent"]));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(out.join("profile.json")).unwrap()).unwrap();
    let dr = report["range"]["dynamic_range"]["snr1_stops"]
        .as_f64()
        .unwrap();
    assert!(dr < 10.0, "{dr}");
    assert!(report["range"]["retained_span_bits"].as_f64().unwrap() > 12.0);
    let expected = Encoding::new(Format::Heic, Some(dr), Compression::Lossless, 90).unwrap();
    assert_eq!(
        report["additional_formats"][0]["measured_dr"].as_f64(),
        Some(dr)
    );
    assert_eq!(
        decode(&out.join("profile.heic"), Format::Heic).bits,
        expected.bits
    );
}

#[test]
fn disabled_files_do_not_block_conversion_and_are_never_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(32, 16).write(&input);
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    for extension in ["jpg", "heic", "avif", "jp2"] {
        fs::write(
            out.join(format!("photo.{extension}")),
            b"keep disabled output",
        )
        .unwrap();
    }
    success(run(
        &input,
        &out,
        &["-no-jpg", "-no-heic", "-no-avif", "-no-j2k", "-silent"],
    ));
    for extension in ["jpg", "heic", "avif", "jp2"] {
        assert_eq!(
            fs::read(out.join(format!("photo.{extension}"))).unwrap(),
            b"keep disabled output"
        );
    }
    let result = run(&input, &tmp.path().join("disabled"), &["-no-png"]);
    assert_eq!(result.status.code(), Some(2));
    assert!(!tmp.path().join("disabled").exists());
}

#[test]
fn new_format_collisions_are_preflighted_before_any_companion_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(32, 16).write(&input);
    for format in [Format::Heic, Format::Avif, Format::J2k] {
        let out = tmp.path().join(format.extension());
        fs::create_dir(&out).unwrap();
        let existing = out.join(format!("photo.{}", format.extension()));
        fs::write(&existing, b"existing image").unwrap();
        let flag = format!(
            "-{}",
            if format == Format::J2k {
                "j2k"
            } else {
                format.extension()
            }
        );
        let result = run(&input, &out, &[&flag, "-report"]);
        assert!(!result.status.success());
        assert!(!out.join("photo.png").exists());
        assert!(!out.join("photo.json").exists());
        assert_eq!(fs::read(existing).unwrap(), b"existing image");
    }
}

#[test]
fn jpeg_limits_do_not_reject_png_or_jpeg2000_only_output() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("wide.DNG");
    common::Dng::ramp(65536, 1).write(&input);
    let out = tmp.path().join("wide");
    success(run(&input, &out, &["-j2k", "-silent"]));
    assert_eq!(decode(&out.join("wide.jp2"), Format::J2k).width, 65536);
    assert!(out.join("wide.png").exists());
    let rejected = tmp.path().join("jpeg");
    let result = run(&input, &rejected, &["-jpg"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("JPEG limit"));
    assert!(!rejected.join("wide.png").exists());
    let input = tmp.path().join("large.DNG");
    let mut dng = common::Dng::ramp(32, 16);
    dng.lens_model = "x".repeat(70000);
    dng.write(&input);
    let out = tmp.path().join("metadata");
    success(run(&input, &out, &["-heic", "-avif", "-j2k", "-silent"]));
    assert!(out.join("large.png").exists());
    for format in [Format::Heic, Format::Avif, Format::J2k] {
        assert!(
            decode(&out.join(format!("large.{}", format.extension())), format)
                .exif
                .len()
                > 70000
        );
    }
}
