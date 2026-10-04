mod common;

use common::Dng;
use dng_monochrome::raw::decode;

#[test]
fn integer_samples_and_metadata_survive_decoding() {
    let tmp = tempfile::tempdir().unwrap();
    for bits in [12, 14, 16] {
        for big_endian in [false, true] {
            let mut dng = Dng::ramp(32, 16);
            dng.bits = bits;
            dng.big_endian = big_endian;
            dng.white = (1u32 << bits) - 1;
            dng.black = 0;
            dng.pixels = (0..512).map(|i| (i * dng.white / 511) as u16).collect();
            let path = tmp.path().join(format!("{bits}-{big_endian}.DNG"));
            dng.write(&path);
            let image = decode(&path, false).unwrap();
            assert_eq!(
                image.pixels, dng.pixels,
                "{bits}-bit, big endian={big_endian}"
            );
            assert_eq!(image.metadata.storage_bits, bits as usize);
            assert_eq!(image.metadata.white_level, dng.white as u16);
            assert_eq!((image.metadata.width, image.metadata.height), (32, 16));
        }
    }
}

#[test]
fn crop_and_all_eight_orientations_are_applied_once() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = [
        vec![6, 7, 8, 11, 12, 13],
        vec![8, 7, 6, 13, 12, 11],
        vec![13, 12, 11, 8, 7, 6],
        vec![11, 12, 13, 6, 7, 8],
        vec![6, 11, 7, 12, 8, 13],
        vec![11, 6, 12, 7, 13, 8],
        vec![13, 8, 12, 7, 11, 6],
        vec![8, 13, 7, 12, 6, 11],
    ];
    for orientation in 1..=8 {
        let mut dng = Dng::ramp(5, 4);
        dng.black = 0;
        dng.pixels = (0..20).collect();
        dng.crop = Some([1, 1, 3, 2]);
        dng.orientation = orientation;
        let path = tmp.path().join("oriented.dng");
        dng.write(&path);
        let image = decode(&path, false).unwrap();
        assert_eq!(image.pixels, expected[orientation as usize - 1]);
        assert_eq!(image.metadata.width, if orientation < 5 { 3 } else { 2 });
        let uncropped = decode(&path, true).unwrap();
        assert_eq!(uncropped.pixels.len(), 20);
    }
}

#[test]
fn cfa_invalid_calibration_and_corrupt_files_are_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.DNG");
    let mut dng = Dng::ramp(8, 8);
    dng.cfa = true;
    dng.write(&path);
    assert!(
        decode(&path, false)
            .unwrap_err()
            .to_string()
            .contains("monochrome")
    );
    dng.cfa = false;
    dng.black = dng.white;
    dng.write(&path);
    assert!(
        decode(&path, false)
            .unwrap_err()
            .to_string()
            .contains("calibration")
    );
    std::fs::write(&path, b"not a DNG image").unwrap();
    assert!(decode(&path, false).is_err());
    std::fs::write(&path, b"II\x2a\0").unwrap();
    assert!(decode(&path, false).is_err());
    assert!(decode(&tmp.path().join("missing.DNG"), false).is_err());
}

#[test]
fn extended_iso_and_noise_profile_preserve_their_full_precision() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("metadata.DNG");
    for big_endian in [false, true] {
        for iso in [None, Some(125), Some(65535), Some(160000)] {
            let mut dng = Dng::ramp(32, 16);
            dng.big_endian = big_endian;
            dng.iso = iso;
            dng.noise_profile = Some([0.0001, 0.00000002]);
            dng.write(&path);
            let image = decode(&path, false).unwrap();
            assert_eq!(image.metadata.iso, iso);
            assert_eq!(image.metadata.noise_profile, dng.noise_profile);
            assert_eq!(image.pixels, dng.pixels);
        }
    }
    for profile in [[-0.1, 0.1], [f64::NAN, 1.0], [0.0, 0.0], [f64::MAX, 1.0]] {
        let mut dng = Dng::ramp(32, 16);
        dng.noise_profile = Some(profile);
        dng.write(&path);
        assert!(decode(&path, false).is_err());
    }
}

#[test]
#[ignore = "set DNG_MONO_SAMPLE to an original Leica DNG; no private photos are bundled"]
fn original_leica_sample_preserves_fourteen_bit_data() {
    let path = std::env::var_os("DNG_MONO_SAMPLE").expect("set DNG_MONO_SAMPLE");
    let image = decode(std::path::Path::new(&path), false).unwrap();
    let min = *image.pixels.iter().min().unwrap();
    let max = *image.pixels.iter().max().unwrap();
    let mut occupied = vec![false; 65536];
    for &v in &image.pixels {
        occupied[v as usize] = true;
    }
    let unique = occupied.iter().filter(|v| **v).count();
    eprintln!(
        "{:?}; observed={min}..{max}; occupied_codes={unique}",
        image.metadata
    );
    assert!(image.metadata.model.to_lowercase().contains("monochrom"));
    assert_eq!(image.metadata.white_level, 16383);
    assert!(max > 4095, "14-bit samples must not be decoded as 12-bit");
    assert!(
        unique > 4096,
        "sample must retain more than 4096 distinct raw codes"
    );
    assert_eq!(
        image.pixels.len(),
        image.metadata.width * image.metadata.height
    );
}
