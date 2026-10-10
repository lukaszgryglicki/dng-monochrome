mod common;

use std::{collections::BTreeMap, fs::File, io::BufReader, path::Path, process::Command};

type Tags = BTreeMap<u16, (u16, Vec<u8>)>;

fn number(bytes: &[u8], little: bool) -> u32 {
    match bytes.len() {
        2 => u32::from(if little {
            u16::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u16::from_be_bytes(bytes.try_into().unwrap())
        }),
        4 => {
            if little {
                u32::from_le_bytes(bytes.try_into().unwrap())
            } else {
                u32::from_be_bytes(bytes.try_into().unwrap())
            }
        }
        _ => panic!("unexpected integer width"),
    }
}

fn directory(bytes: &[u8], offset: usize, little: bool) -> Tags {
    let count = number(&bytes[offset..offset + 2], little) as usize;
    (0..count)
        .map(|i| {
            let entry = offset + 2 + 12 * i;
            let tag = number(&bytes[entry..entry + 2], little) as u16;
            let typ = number(&bytes[entry + 2..entry + 4], little) as u16;
            let n = number(&bytes[entry + 4..entry + 8], little) as usize;
            let width = match typ {
                1 | 2 | 7 => 1,
                3 => 2,
                4 | 9 => 4,
                5 | 10 | 12 => 8,
                _ => panic!("unexpected TIFF type {typ}"),
            };
            let length = n * width;
            let start = if length <= 4 {
                entry + 8
            } else {
                number(&bytes[entry + 8..entry + 12], little) as usize
            };
            (tag, (typ, bytes[start..start + length].to_vec()))
        })
        .collect()
}

fn embedded(path: &Path, png: bool) -> Vec<u8> {
    let file = BufReader::new(File::open(path).unwrap());
    if png {
        let reader = png::Decoder::new(file).read_info().unwrap();
        reader
            .info()
            .exif_metadata
            .as_ref()
            .expect("PNG eXIf must exist")
            .to_vec()
    } else {
        let mut reader = jpeg_decoder::Decoder::new(file);
        reader.read_info().unwrap();
        reader
            .exif_data()
            .expect("JPEG EXIF APP1 must exist")
            .to_vec()
    }
}

#[test]
fn both_formats_preserve_photographic_exif_and_normalize_output_geometry() {
    let tmp = tempfile::tempdir().unwrap();
    for big_endian in [false, true] {
        for orientation in [1, 6] {
            for transfer in ["srgb", "linear"] {
                let mut dng = common::Dng::ramp(24, 16);
                dng.big_endian = big_endian;
                dng.orientation = orientation;
                dng.crop = Some([2, 3, 20, 10]);
                dng.iso = Some(160000);
                let input = tmp.path().join("metadata.DNG");
                dng.write(&input);
                let out = tmp
                    .path()
                    .join(format!("{big_endian}-{orientation}-{transfer}"));
                let result = Command::new(env!("CARGO_BIN_EXE_dng-monochrome"))
                    .args([
                        "--jpg",
                        "--threads",
                        "2",
                        "--silent",
                        "--report",
                        "--transfer",
                        transfer,
                        "-o",
                    ])
                    .arg(&out)
                    .arg(&input)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let (width, height) = if orientation == 1 { (20, 10) } else { (10, 20) };
                for (extension, is_png) in [("png", true), ("jpg", false)] {
                    let bytes = embedded(&out.join(format!("metadata.{extension}")), is_png);
                    assert!(&bytes[..2] == b"II" || &bytes[..2] == b"MM");
                    let little = &bytes[..2] == b"II";
                    assert_eq!(number(&bytes[2..4], little), 42);
                    let root = directory(&bytes, number(&bytes[4..8], little) as usize, little);
                    assert_eq!(number(&root[&0x0112].1, little), 1);
                    assert_eq!(number(&root[&0x0100].1, little), width);
                    assert_eq!(number(&root[&0x0101].1, little), height);
                    assert_eq!(
                        number(&root[&0x0102].1, little),
                        if is_png { 16 } else { 8 }
                    );
                    assert_eq!(number(&root[&0x0115].1, little), 1);
                    assert!(root[&0x0131].1.starts_with(b"dng-monochrome "));
                    let exif = directory(&bytes, number(&root[&0x8769].1, little) as usize, little);
                    assert_eq!(number(&exif[&0x8827].1, little), 65535);
                    for tag in [0x8831, 0x8833] {
                        assert_eq!(exif[&tag].0, 4);
                        assert_eq!(number(&exif[&tag].1, little), 160000);
                    }
                    for (tag, expected) in
                        [(0x829a, [1, 125]), (0x829d, [28, 10]), (0x920a, [35, 1])]
                    {
                        assert_eq!(exif[&tag].0, 5);
                        assert_eq!(number(&exif[&tag].1[..4], little), expected[0]);
                        assert_eq!(number(&exif[&tag].1[4..], little), expected[1]);
                    }
                    assert_eq!(&exif[&0xa434].1, b"Recorded manual M lens\0");
                    assert_eq!(&exif[&0x9003].1, b"2026:10:03 12:34:56\0");
                    assert_eq!(number(&exif[&0xa002].1, little), width);
                    assert_eq!(number(&exif[&0xa003].1, little), height);
                    assert_eq!(
                        number(&exif[&0xa001].1, little),
                        if transfer == "linear" { 65535 } else { 1 }
                    );
                    if !is_png && transfer == "linear" {
                        assert_eq!(exif[&0xa500].0, 5);
                        assert_eq!(number(&exif[&0xa500].1[..4], little), 1);
                        assert_eq!(number(&exif[&0xa500].1[4..], little), 1);
                    }
                    assert!(
                        !exif.contains_key(&0x927c),
                        "do not retain invalid MakerNote offsets"
                    );
                }
                let report: serde_json::Value =
                    serde_json::from_reader(File::open(out.join("metadata.json")).unwrap())
                        .unwrap();
                assert_eq!(report["metadata"]["iso"], 160000);
                assert_eq!(report["metadata"]["exposure_seconds"], 0.008);
                assert_eq!(report["metadata"]["aperture_f_number"], 2.8);
                assert_eq!(report["metadata"]["focal_length_mm"], 35.0);
                assert_eq!(report["metadata"]["lens_model"], "Recorded manual M lens");
            }
        }
    }
}

#[test]
fn oversized_jpeg_metadata_is_an_error_without_partial_images() {
    let tmp = tempfile::tempdir().unwrap();
    let mut dng = common::Dng::ramp(16, 16);
    dng.lens_model = "x".repeat(70000);
    let input = tmp.path().join("large.DNG");
    dng.write(&input);
    let out = tmp.path().join("out");
    let result = Command::new(env!("CARGO_BIN_EXE_dng-monochrome"))
        .args(["--jpg", "--threads", "2"])
        .arg("-o")
        .arg(&out)
        .arg(&input)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("65527-byte"));
    assert!(!out.join("large.png").exists());
    assert!(!out.join("large.jpg").exists());
}

#[test]
fn gps_photographer_and_apex_aperture_are_preserved_without_inventing_fnumber() {
    use dng_monochrome::{
        expression::FunctionPolicy,
        output::{self, OutputPaths},
        range::{self, Histogram, RangeOptions},
        raw, tone,
    };
    use rawler::{exif::ExifGPS, formats::tiff::Rational};
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("photo.DNG");
    common::Dng::ramp(32, 16).write(&input);
    let mut image = raw::decode(&input, false).unwrap();
    image.metadata.aperture_f_number = None;
    let source = &mut image.source_metadata.exif;
    source.fnumber = None;
    source.aperture_value = Some(Rational { n: 740, d: 100 });
    source.artist = Some("Fixture artist".into());
    source.copyright = Some("Fixture copyright".into());
    source.gps = Some(ExifGPS {
        gps_version_id: Some([2, 3, 0, 0]),
        gps_latitude_ref: Some("N".into()),
        gps_latitude: Some([
            Rational { n: 12, d: 1 },
            Rational { n: 3, d: 1 },
            Rational { n: 4, d: 1 },
        ]),
        gps_longitude_ref: Some("E".into()),
        gps_longitude: Some([
            Rational { n: 34, d: 1 },
            Rational { n: 5, d: 1 },
            Rational { n: 6, d: 1 },
        ]),
        ..Default::default()
    });
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
    let rendered = tone::render(
        &image,
        &hist,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        tone::Transfers::default(),
    )
    .unwrap();
    let paths = OutputPaths::new(&tmp.path().join("out/photo"), false);
    output::save(
        &paths,
        &image,
        &rendered,
        &serde_json::json!({}),
        tone::Transfers::default(),
        90,
        false,
    )
    .unwrap();
    for (path, png) in [(&paths.png, true), (&paths.jpeg, false)] {
        let bytes = embedded(path, png);
        let little = &bytes[..2] == b"II";
        let root = directory(&bytes, number(&bytes[4..8], little) as usize, little);
        assert_eq!(&root[&0x013b].1, b"Fixture artist\0");
        assert_eq!(&root[&0x8298].1, b"Fixture copyright\0");
        let gps = directory(&bytes, number(&root[&0x8825].1, little) as usize, little);
        assert_eq!(&gps[&1].1, b"N\0");
        assert_eq!(&gps[&3].1, b"E\0");
        assert_eq!(number(&gps[&2].1[..4], little), 12);
        assert_eq!(number(&gps[&4].1[..4], little), 34);
        let exif = directory(&bytes, number(&root[&0x8769].1, little) as usize, little);
        assert!(!exif.contains_key(&0x829d));
        assert_eq!(number(&exif[&0x9202].1[..4], little), 740);
        assert_eq!(number(&exif[&0x9202].1[4..], little), 100);
    }
}
