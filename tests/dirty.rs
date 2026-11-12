mod common;

use common::Dng;
use dng_monochrome::{
    cli::Cli,
    color::{self, Green},
    dirty::{self, Calibration, Evidence},
    raw::{self, MonoImage},
};
use std::{
    ffi::OsString,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_dng-monochrome");
const CENTERS: [(f64, f64); 2] = [(180.5, 150.5), (440.5, 290.5)];

fn scene(index: usize, dust: bool) -> Dng {
    let mut image = Dng::ramp(640, 480);
    let gradients = [(0.4, 0.1), (-0.2, 0.4), (-0.3, -0.3), (0.2, -0.4)];
    let (a, b) = gradients[index % gradients.len()];
    for y in 0..image.height {
        for x in 0..image.width {
            let (px, py) = (f64::from(x / 2 * 2) + 0.5, f64::from(y / 2 * 2) + 0.5);
            let (u, v) = (px / 640.0 - 0.5, py / 480.0 - 0.5);
            let mut illumination = 0.35f64.ln() + a * u + b * v + 0.1 * u * v;
            let moving =
                ((px - (70.0 + index as f64 * 120.0)).powi(2) + (py - 400.0).powi(2)) / 100.0;
            illumination -= 0.3 * (-0.5 * moving).exp();
            if dust {
                for (i, (cx, cy)) in CENTERS.into_iter().enumerate() {
                    let sigma = [6.4, 11.0][i] + (index % 3) as f64 * 0.8;
                    let r2 = ((px - cx).powi(2) + (py - cy).powi(2)) / sigma.powi(2);
                    illumination -= (0.24 + index as f64 * 0.025) * (-0.5 * r2).exp();
                }
            }
            image.pixels[(y * image.width + x) as usize] = (f64::from(image.black)
                + f64::from(image.white - image.black) * illumination.exp())
            .round() as u16;
        }
    }
    image
}

fn curved_scene(index: usize, center: Option<(f64, f64)>) -> Dng {
    let mut image = scene(index, false);
    image.crop = Some([12, 8, 601, 451]);
    if let Some((cx, cy)) = center {
        let (sin, cos) = 0.55f64.sin_cos();
        let sigma = 2.8 + (index % 3) as f64 * 0.3;
        for y in 0..image.height {
            for x in 0..image.width {
                let dx = f64::from(x / 2 * 2) + 0.5 - cx;
                let dy = f64::from(y / 2 * 2) + 0.5 - cy;
                let (u, v) = (cos * dx + sin * dy, -sin * dx + cos * dy);
                let t = u.clamp(-28.0, 28.0);
                let r2 = ((u - t).powi(2) + (v - 0.018 * (t * t - 250.0)).powi(2)) / sigma.powi(2);
                let gain = (-(0.18 + index as f64 * 0.02) * (-0.5 * r2).exp()).exp();
                let pixel = &mut image.pixels[(y * image.width + x) as usize];
                *pixel = (f64::from(image.black)
                    + (f64::from(*pixel) - f64::from(image.black)) * gain)
                    .round() as u16;
            }
        }
    }
    image
}

// A clean scene attenuated by `shadow(x, y)`, the log gain at each sample center.
fn shaded(index: usize, shadow: impl Fn(f64, f64) -> f64) -> Dng {
    let mut image = scene(index, false);
    let black = f64::from(image.black);
    let width = image.width as usize;
    for (i, pixel) in image.pixels.iter_mut().enumerate() {
        let gain = shadow((i % width) as f64 + 0.5, (i / width) as f64 + 0.5).exp();
        *pixel = (black + (f64::from(*pixel) - black) * gain).round() as u16;
    }
    image
}

// Largest relative error against `clean` within `radius` of a point.
fn worst_error(image: &[u16], clean: &Dng, center: (f64, f64), radius: f64) -> f64 {
    let black = f64::from(clean.black);
    let width = clean.width as usize;
    image
        .iter()
        .zip(&clean.pixels)
        .enumerate()
        .filter(|(i, _)| {
            ((i % width) as f64 + 0.5 - center.0).hypot((i / width) as f64 + 0.5 - center.1)
                <= radius
        })
        .map(|(_, (&value, &clean))| {
            ((f64::from(value) - black) / (f64::from(clean) - black) - 1.0).abs()
        })
        .fold(0.0, f64::max)
}

fn decode(root: &Path, name: &str, image: &Dng) -> MonoImage {
    let path = root.join(name);
    image.write(&path);
    raw::decode(&path, false).unwrap()
}

fn references(root: &Path, crop: Option<[u32; 4]>) -> Vec<Evidence> {
    (0..4)
        .map(|index| {
            let mut image = scene(index, true);
            image.crop = crop;
            image.orientation = [1, 6, 8, 3][index];
            dirty::analyze(&decode(root, &format!("ref{index}.DNG"), &image)).unwrap()
        })
        .collect()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn command(input: &Path, output: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(["--threads", "2", "--jobs", "1"]);
    command
}

fn png(path: &Path) -> Vec<u16> {
    let mut reader = png::Decoder::new(BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut bytes).unwrap();
    assert_eq!(frame.bit_depth, png::BitDepth::Sixteen);
    bytes[..frame.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|value| u16::from_be_bytes(*value))
        .collect()
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn recurring_shadows_are_repaired_but_moving_scene_details_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let original = scene(0, true);
    let clean = scene(0, false);
    let mut image = decode(dir.path(), "target.DNG", &original);
    let plan = calibration.plan(&dirty::analyze(&image).unwrap());
    let report = plan.apply(&mut image).unwrap();
    assert_eq!(report.spots.len(), 2, "{report:?}");
    assert!(report.warning.is_none());
    assert!((1.1..=4.0).contains(&report.maximum_gain));
    assert_eq!(report.clipped_samples, 0);
    assert!(report.changed_samples > 500);
    assert!(report.changed_samples < image.pixels.len() / 10);
    for (cx, cy) in CENTERS {
        let (mut before, mut after, mut count) = (0.0, 0.0, 0);
        for y in cy as usize - 6..=cy as usize + 6 {
            for x in cx as usize - 6..=cx as usize + 6 {
                let i = y * 640 + x;
                before += f64::from(clean.pixels[i].abs_diff(original.pixels[i]));
                after += f64::from(clean.pixels[i].abs_diff(image.pixels[i]));
                count += 1;
            }
        }
        assert!(after < before * 0.15, "remaining error {after}/{before}");
        assert!(
            after / f64::from(count) < 70.0,
            "mean residual {}, original {}; report {report:?}",
            after / f64::from(count),
            before / f64::from(count)
        );
    }
    for (index, (&before, &after)) in original.pixels.iter().zip(&image.pixels).enumerate() {
        assert!(after >= before && after <= original.white as u16);
        let (x, y) = ((index % 640) as f64, (index / 640) as f64);
        if CENTERS
            .iter()
            .all(|&(cx, cy)| (x - cx).hypot(y - cy) > 65.0)
        {
            assert_eq!(after, before, "non-dust sample {index} changed");
        }
    }
}

#[test]
fn curved_recurring_shadows_are_repaired_in_mono_and_bayer_planes() {
    let dir = tempfile::tempdir().unwrap();
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let mut source = curved_scene(index, Some((320.5, 160.5)));
            source.orientation = [1, 6, 8, 3][index];
            dirty::analyze_with_extra(
                &decode(dir.path(), &format!("ref{index}.DNG"), &source),
                true,
            )
            .unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let check = |original: &[u16], corrected: &[u16], clean: &[u16], report: &dirty::Report| {
        let (mut before, mut after, mut changed) = (0u64, 0u64, 0usize);
        for ((&original, &corrected), &clean) in original.iter().zip(corrected).zip(clean) {
            if original.abs_diff(clean) > 20 {
                before += u64::from(original.abs_diff(clean));
                after += u64::from(corrected.abs_diff(clean));
            }
            // Full-resolution refinement may only trim its own overshoot.
            assert!(f64::from(corrected) >= f64::from(original) * 0.995);
            changed += usize::from(original != corrected);
        }
        assert!(before > 5_000);
        assert!(
            (after as f64) < before as f64 * 0.2,
            "curved shadow residual {after}/{before}; {report:?}"
        );
        assert!(changed < original.len() / 5);
    };
    for orientation in [1, 6] {
        let mut source = curved_scene(0, Some((320.5, 160.5)));
        let mut clean = curved_scene(0, None);
        source.orientation = orientation;
        clean.orientation = orientation;
        let mut image = decode(dir.path(), "target.DNG", &source);
        let expected = decode(dir.path(), "clean.DNG", &clean);
        let original = image.pixels.clone();
        let plan = calibration.plan(&dirty::analyze_with_extra(&image, true).unwrap());
        plan.apply_with_strength(&mut image, 0.0).unwrap();
        assert_eq!(image.pixels, original);
        let report = plan.apply(&mut image).unwrap();
        assert_eq!(report.clipped_samples, 0);
        check(&original, &image.pixels, &expected.pixels, &report);

        let channels = color::decode(&dir.path().join("target.DNG"), false, Green::Average, true)
            .unwrap()
            .channels;
        let clean_channels =
            color::decode(&dir.path().join("clean.DNG"), false, Green::Average, true)
                .unwrap()
                .channels;
        for ((_, mut channel), (_, clean)) in channels.into_iter().zip(clean_channels) {
            let original = channel.pixels.clone();
            let report = plan.apply(&mut channel).unwrap();
            assert_eq!(report.clipped_samples, 0);
            check(&original, &channel.pixels, &clean.pixels, &report);
        }
    }
}

#[test]
fn faint_curved_shadows_are_not_lost_when_large_proposals_dilute_their_contrast() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = Vec::new();
    let mut clean_images = Vec::new();
    for index in 0..4 {
        let clean = curved_scene(index, None);
        let mut source = curved_scene(index, Some((320.5, 160.5)));
        let black = f64::from(source.black);
        for (pixel, &clean) in source.pixels.iter_mut().zip(&clean.pixels) {
            let light = f64::from(clean) - black;
            let ratio = (f64::from(*pixel) - black) / light;
            *pixel = (black + light * ratio.powf(0.2)).round() as u16;
        }
        images.push(decode(dir.path(), &format!("faint{index}.DNG"), &source));
        clean_images.push(decode(dir.path(), &format!("clean{index}.DNG"), &clean));
    }
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze_with_extra(image, true).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for ((image, clean), evidence) in images.iter_mut().zip(clean_images).zip(&evidence) {
        let original = image.pixels.clone();
        let report = calibration.plan(evidence).apply(image).unwrap();
        let (mut before, mut after) = (0u64, 0u64);
        for ((&original, &corrected), &clean) in
            original.iter().zip(&image.pixels).zip(&clean.pixels)
        {
            if original.abs_diff(clean) > 20 {
                before += u64::from(original.abs_diff(clean));
                after += u64::from(corrected.abs_diff(clean));
            }
        }
        assert!(before > 5_000);
        assert!(
            (after as f64) < before as f64 * 0.2,
            "faint curved shadow residual {after}/{before}; {report:?}"
        );
    }
}

#[test]
fn curved_shadow_confirmation_does_not_repair_clean_targets_or_moving_details() {
    let dir = tempfile::tempdir().unwrap();
    let fixed: Vec<_> = (0..4)
        .map(|index| {
            dirty::analyze_with_extra(
                &decode(
                    dir.path(),
                    &format!("fixed{index}.DNG"),
                    &curved_scene(index, Some((320.5, 160.5))),
                ),
                true,
            )
            .unwrap()
        })
        .collect();
    let mut clean = decode(dir.path(), "clean.DNG", &curved_scene(0, None));
    let before = clean.pixels.clone();
    let report = Calibration::new(&fixed)
        .unwrap()
        .plan(&dirty::analyze_with_extra(&clean, true).unwrap())
        .apply(&mut clean)
        .unwrap();
    assert_eq!(report.changed_samples, 0);
    assert_eq!(clean.pixels, before);

    let moving: Vec<_> = (0..4)
        .map(|index| {
            dirty::analyze_with_extra(
                &decode(
                    dir.path(),
                    &format!("moving{index}.DNG"),
                    &curved_scene(index, Some((170.5 + index as f64 * 90.0, 160.5))),
                ),
                true,
            )
            .unwrap()
        })
        .collect();
    let mut target = decode(
        dir.path(),
        "target.DNG",
        &curved_scene(0, Some((170.5, 160.5))),
    );
    let before = target.pixels.clone();
    let report = Calibration::new(&moving)
        .unwrap()
        .plan(&moving[0])
        .apply(&mut target)
        .unwrap();
    assert_eq!(report.changed_samples, 0);
    assert_eq!(target.pixels, before);
}

#[test]
fn long_unenclosed_scene_lines_are_not_treated_as_curved_dirt() {
    let dir = tempfile::tempdir().unwrap();
    let mut images: Vec<_> = (0..4)
        .map(|index| {
            let mut source = curved_scene(index, None);
            for y in 0..source.height {
                for x in 0..source.width {
                    let px = f64::from(x / 2 * 2) + 0.5;
                    let py = f64::from(y / 2 * 2) + 0.5;
                    let distance = (py - 160.5 - 0.7 * (px - 320.5)) / 2.8;
                    let gain = (-0.2 * (-0.5 * distance * distance).exp()).exp();
                    let pixel = &mut source.pixels[(y * source.width + x) as usize];
                    *pixel = (f64::from(source.black)
                        + (f64::from(*pixel) - f64::from(source.black)) * gain)
                        .round() as u16;
                }
            }
            decode(dir.path(), &format!("line{index}.DNG"), &source)
        })
        .collect();
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze_with_extra(image, true).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for (image, evidence) in images.iter_mut().zip(&evidence) {
        let before = image.pixels.clone();
        let report = calibration.plan(evidence).apply(image).unwrap();
        assert_eq!(report.changed_samples, 0, "{report:?}");
        assert_eq!(image.pixels, before);
    }
}

#[test]
fn extra_correction_preserves_one_view_details_joined_to_recurring_dirt() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = |index: usize, dust: bool| {
        let mut source = curved_scene(index, dust.then_some((320.5, 160.5)));
        if index == 0 {
            for y in 0..source.height {
                for x in 0..source.width {
                    let px = f64::from(x / 2 * 2) + 0.5;
                    let py = f64::from(y / 2 * 2) + 0.5;
                    let r2 = ((px - 338.5).powi(2) + (py - 157.5).powi(2)) / 2.5f64.powi(2);
                    let gain = (-0.12 * (-0.5 * r2).exp()).exp();
                    let pixel = &mut source.pixels[(y * source.width + x) as usize];
                    *pixel = (f64::from(source.black)
                        + (f64::from(*pixel) - f64::from(source.black)) * gain)
                        .round() as u16;
                }
            }
        }
        source
    };
    let images: Vec<_> = (0..4)
        .map(|index| {
            decode(
                dir.path(),
                &format!("joined{index}.DNG"),
                &fixture(index, true),
            )
        })
        .collect();
    let clean = decode(dir.path(), "clean.DNG", &fixture(0, false));
    let evidence: Vec<_> = [0, 1, 2, 3, 0, 0, 0]
        .into_iter()
        .map(|index| dirty::analyze_with_extra(&images[index], true).unwrap())
        .collect();
    for references in [&evidence[..4], &evidence[..]] {
        let mut target = raw::decode(&dir.path().join("joined0.DNG"), false).unwrap();
        let original = target.pixels.clone();
        let report = Calibration::new(references)
            .unwrap()
            .plan(&evidence[0])
            .apply(&mut target)
            .unwrap();
        let (mut before, mut after, mut detail_samples) = (0u64, 0u64, 0usize);
        for (i, ((&original, &corrected), &clean)) in original
            .iter()
            .zip(&target.pixels)
            .zip(&clean.pixels)
            .enumerate()
        {
            if original.abs_diff(clean) > 20 {
                before += u64::from(original.abs_diff(clean));
                after += u64::from(corrected.abs_diff(clean));
            }
            let x = i % target.metadata.width + target.metadata.crop[0];
            let y = i / target.metadata.width + target.metadata.crop[1];
            if (337..=341).contains(&x) && (155..=159).contains(&y) {
                detail_samples += 1;
                assert!(
                    corrected.abs_diff(clean) <= 20,
                    "one-view scene detail was brightened at {x},{y}: {corrected} vs {clean}; {report:?}"
                );
            }
        }
        assert_eq!(detail_samples, 25);
        assert!(before > 5_000);
        assert!(
            (after as f64) < before as f64 * 0.2,
            "recurring curve residual {after}/{before}; {report:?}"
        );
        assert_eq!(report.clipped_samples, 0);
    }
}

#[test]
fn unconfirmed_extra_shapes_neither_hide_compact_dirt_nor_borrow_its_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let build = |index: usize, dust: bool| {
        let mut source = scene(index, dust);
        if index == 0 {
            let clean = scene(index, false);
            let curved = curved_scene(index, Some((215.5, 160.5)));
            let black = f64::from(source.black);
            for ((pixel, &curved), &clean) in source
                .pixels
                .iter_mut()
                .zip(&curved.pixels)
                .zip(&clean.pixels)
            {
                let gain = ((f64::from(curved) - black) / (f64::from(clean) - black)).powf(0.4);
                *pixel = (black + (f64::from(*pixel) - black) * gain).round() as u16;
            }
        }
        source
    };
    let images: Vec<_> = (0..3)
        .map(|index| {
            decode(
                dir.path(),
                &format!("nearby{index}.DNG"),
                &build(index, true),
            )
        })
        .collect();
    let ordinary: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze(image).unwrap())
        .collect();
    let extra: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze_with_extra(image, true).unwrap())
        .collect();
    let ordinary_calibration = Calibration::new(&ordinary).unwrap();
    let extra_calibration = Calibration::new(&extra).unwrap();
    for (index, (ordinary, extra)) in ordinary.iter().zip(&extra).enumerate() {
        let input = dir.path().join(format!("nearby{index}.DNG"));
        let mut expected = raw::decode(&input, false).unwrap();
        let ordinary_report = ordinary_calibration
            .plan(ordinary)
            .apply(&mut expected)
            .unwrap();
        assert_eq!(ordinary_report.spots.len(), 2, "{ordinary_report:?}");
        let mut actual = raw::decode(&input, false).unwrap();
        let extra_report = extra_calibration.plan(extra).apply(&mut actual).unwrap();
        assert_eq!(extra_report.spots.len(), 2, "{extra_report:?}");
        // Extra mode only refines the confirmed compact shadows at full resolution: beyond
        // them nothing may differ, and at them the repair may not get worse.
        let clean = decode(
            dir.path(),
            &format!("clean{index}.DNG"),
            &build(index, false),
        );
        let (mut plain, mut refined) = (0u64, 0u64);
        for (i, (&actual, &expected)) in actual.pixels.iter().zip(&expected.pixels).enumerate() {
            let (x, y) = ((i % 640) as f64, (i / 640) as f64);
            let distance = CENTERS
                .iter()
                .map(|&(cx, cy)| (x - cx).hypot(y - cy))
                .fold(f64::INFINITY, f64::min);
            if distance > 65.0 {
                assert_eq!(
                    actual, expected,
                    "extra mode changed sample {i} away from confirmed dirt in frame {index}: {extra_report:?}"
                );
            } else if distance <= 8.0 {
                plain += u64::from(expected.abs_diff(clean.pixels[i]));
                refined += u64::from(actual.abs_diff(clean.pixels[i]));
            }
        }
        assert!(
            refined <= plain,
            "extra mode lost compact correction in frame {index}: {refined} vs {plain}; {extra_report:?}"
        );
    }
}

#[test]
fn extra_refinement_clears_the_core_and_bright_fringe_of_sharp_dust() {
    let dir = tempfile::tempdir().unwrap();
    // Stopped-down dust: a hard-edged shadow inside a bright diffraction fringe, which an
    // analysis-resolution, brightening-only gain cannot follow.
    let center = (300.5, 200.5);
    let sharp = |x: f64, y: f64| {
        let radius = (x - center.0).hypot(y - center.1);
        -0.3 * ((7.5 - radius) / 1.5).clamp(0.0, 1.0)
            + 0.06 * (1.0 - (radius - 9.5).abs() / 1.5).clamp(0.0, 1.0)
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("sharp{index}.DNG"),
                &shaded(index, sharp),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let clean = scene(0, false);
    let original = shaded(0, sharp);
    let mut image = decode(dir.path(), "target.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    assert!(report.warning.is_none(), "{report:?}");
    let before = worst_error(&original.pixels, &clean, center, 16.0);
    let after = worst_error(&image.pixels, &clean, center, 16.0);
    assert!(
        before > 0.25 && after < 0.35 * before,
        "{after} of {before}; {report:?}"
    );
    // The core is flat again and the fringe, which only darkening can remove, is halved.
    assert!(
        worst_error(&image.pixels, &clean, center, 4.0) < 0.01,
        "{report:?}"
    );
    let width = clean.width as usize;
    for (i, (&value, &clean)) in image.pixels.iter().zip(&clean.pixels).enumerate() {
        let radius =
            ((i % width) as f64 + 0.5 - center.0).hypot((i / width) as f64 + 0.5 - center.1);
        if (8.5..=10.5).contains(&radius) {
            let error = (f64::from(value) - 1023.0) / (f64::from(clean) - 1023.0) - 1.0;
            assert!(
                error < 0.035,
                "bright fringe {error} left at radius {radius}; {report:?}"
            );
        } else if radius > 40.0 {
            assert_eq!(
                value, original.pixels[i],
                "sample {i} changed away from the dust"
            );
        }
    }
}

#[test]
fn extra_repairs_the_sharp_shape_of_dirt_confirmed_only_as_a_soft_disc() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // Wide open a fibre is a soft disc; stopped down it is a dark L of its own shape.
    let soft = |x: f64, y: f64| {
        -0.07 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 144.0).exp()
    };
    let bar = |along: f64, across: f64| {
        ((17.0 - along.abs()) / 2.0).clamp(0.0, 1.0) * ((4.5 - across.abs()) / 2.0).clamp(0.0, 1.0)
    };
    let sharp = |x: f64, y: f64| {
        // Two legs balanced on the center of the soft disc.
        let (dx, dy) = (x - center.0 - 2.5, y - center.1 + 4.5);
        -0.2 * bar(dy + 3.0, dx + 10.0).max(bar(dx - 5.0, dy - 12.0))
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("soft{index}.DNG"),
                &shaded(index, soft),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let clean = scene(0, false);
    let original = shaded(0, sharp);
    let mut image = decode(dir.path(), "target.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    assert_eq!(report.spots.len(), 1, "{report:?}");
    let width = clean.width as usize;
    let (mut before, mut after) = (0.0, 0.0);
    for (i, ((&value, &original), &clean)) in image
        .pixels
        .iter()
        .zip(&original.pixels)
        .zip(&clean.pixels)
        .enumerate()
    {
        let (x, y) = ((i % width) as f64 + 0.5, (i / width) as f64 + 0.5);
        let error = |value: u16| (f64::from(value) - 1023.0) / (f64::from(clean) - 1023.0) - 1.0;
        if sharp(x, y) < -0.01 {
            before += error(original).abs();
            after += error(value).abs();
        } else {
            // What lies beside the legs is not dirt.
            assert!(
                error(value) < 0.1,
                "clean sample brightened by {} at {x},{y}; {report:?}",
                error(value)
            );
            assert!(
                (x - center.0).hypot(y - center.1) <= 60.0 || value == original,
                "sample {i} changed away from the dirt"
            );
        }
    }
    assert!(
        before > 50.0 && after < 0.15 * before,
        "{after} of {before}; {report:?}"
    );
    // The same shape away from every confirmed spot stays what it is: scenery.
    let moved = |x: f64, y: f64| sharp(x - 60.0, y + 30.0);
    let original = shaded(0, moved);
    let mut image = decode(dir.path(), "moved.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    assert!(image.pixels == original.pixels, "{report:?}");
}

#[test]
fn extra_measures_confirmed_dirt_too_faint_for_the_target_to_propose() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    let dust = |depth: f64| {
        move |x: f64, y: f64| {
            -depth * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 64.0).exp()
        }
    };
    let analyze = |extra: bool| -> Vec<_> {
        (0..4)
            .map(|index| {
                let image = decode(
                    dir.path(),
                    &format!("deep{index}.DNG"),
                    &shaded(index, dust(0.08)),
                );
                dirty::analyze_with_extra(&image, extra).unwrap()
            })
            .collect()
    };
    let clean = scene(0, false);
    let width = clean.width as usize;
    // Stopped up, the target's own shadow is below what a single frame may propose.
    let original = shaded(0, dust(0.011));
    let mut image = decode(dir.path(), "target.DNG", &original);
    let plain = Calibration::new(&analyze(false)).unwrap();
    let report = plain
        .plan(&dirty::analyze(&image).unwrap())
        .apply(&mut image)
        .unwrap();
    assert_eq!(report.confirmed_sensor_spots, 1, "{report:?}");
    assert_eq!(image.pixels, original.pixels);
    let extra = Calibration::new(&analyze(true)).unwrap();
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = extra.plan(&target).apply(&mut image).unwrap();
    assert_eq!(report.spots.len(), 1, "{report:?}");
    let before = worst_error(&original.pixels, &clean, center, 30.0);
    let after = worst_error(&image.pixels, &clean, center, 30.0);
    assert!(
        before > 0.01 && after < 0.3 * before,
        "{after} of {before}; {report:?}"
    );
    for (i, (&value, &original)) in image.pixels.iter().zip(&original.pixels).enumerate() {
        let radius =
            ((i % width) as f64 + 0.5 - center.0).hypot((i / width) as f64 + 0.5 - center.1);
        assert!(
            radius <= 48.0 || value == original,
            "sample {i} changed away from the dirt"
        );
    }
    // Scenery at the confirmed spot is not measured as dirt: a wire through it, a dark
    // object over it, or nothing at all.
    let wire = |x: f64, y: f64| {
        -0.06 * (-0.5 * ((y - center.1 - 0.2 * (x - center.0)) / 1.2).powi(2)).exp()
    };
    let wired = shaded(0, wire);
    let mut image = decode(
        dir.path(),
        "wired.DNG",
        &shaded(0, |x, y| dust(0.011)(x, y) + wire(x, y)),
    );
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = extra.plan(&target).apply(&mut image).unwrap();
    for (i, (&value, &wired)) in image.pixels.iter().zip(&wired.pixels).enumerate() {
        let (x, y) = ((i % width) as f64 + 0.5, (i / width) as f64 + 0.5);
        if wire(x, y) < -0.03 {
            let ratio = (f64::from(value) - 1023.0) / (f64::from(wired) - 1023.0);
            assert!(
                ratio < 1.005,
                "wire brightened x{ratio} at {x},{y}; {report:?}"
            );
        }
    }
    let object = |x: f64, y: f64| {
        -0.3 * ((26.0 - (x - center.0 - 12.0).hypot(y - center.1 + 6.0)) / 2.0).clamp(0.0, 1.0)
    };
    for (name, source) in [
        ("object.DNG", shaded(0, object)),
        ("clean.DNG", scene(1, false)),
    ] {
        let mut image = decode(dir.path(), name, &source);
        let target = dirty::analyze_with_extra(&image, true).unwrap();
        let report = extra.plan(&target).apply(&mut image).unwrap();
        assert!(image.pixels == source.pixels, "{name}: {report:?}");
    }
}

#[test]
fn extra_keeps_the_fitted_repair_of_dirt_that_the_stack_renders_far_fainter() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // Wide open the dirt is a broad faint disc; in the stopped-down target it is small
    // and deep.
    let spot = |depth: f64, sigma: f64| {
        move |x: f64, y: f64| {
            -depth
                * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / sigma.powi(2)).exp()
        }
    };
    let bands = |_: f64, y: f64| 0.02 * (y / 9.0).sin();
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("wide{index}.DNG"),
                &shaded(index, spot(0.05, 5.0)),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    // A banded sky keeps the target from measuring the stacked shadow itself, so the stack
    // offers its own faint shape.
    let banded = shaded(0, bands);
    let original = shaded(0, |x, y| spot(0.35, 2.5)(x, y) + bands(x, y));
    let mut image = decode(dir.path(), "target.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    assert_eq!(report.stacked_regions, 1, "{report:?}");
    let before = worst_error(&original.pixels, &banded, center, 12.0);
    let after = worst_error(&image.pixels, &banded, center, 12.0);
    assert!(
        before > 0.25 && after < 0.2 * before,
        "{after} of {before}; {report:?}"
    );
}

#[test]
fn stacking_repairs_dust_too_faint_for_any_single_frame_fit() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    let faint = |x: f64, y: f64| {
        -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
    };
    let analyze = |extra: bool| -> Vec<_> {
        (0..4)
            .map(|index| {
                let image = decode(
                    dir.path(),
                    &format!("faint{index}.DNG"),
                    &shaded(index, faint),
                );
                dirty::analyze_with_extra(&image, extra).unwrap()
            })
            .collect()
    };
    let clean = scene(0, false);
    let original = shaded(0, faint);
    // No single frame yields a confirmed fit, so ordinary correction leaves it alone.
    let mut image = decode(dir.path(), "target.DNG", &original);
    let plain = Calibration::new(&analyze(false)).unwrap();
    let report = plain
        .plan(&dirty::analyze(&image).unwrap())
        .apply(&mut image)
        .unwrap();
    assert_eq!(report.confirmed_sensor_spots, 0, "{report:?}");
    assert_eq!(image.pixels, original.pixels);
    // The stack of the other views finds it and the target measures it.
    let extra = Calibration::new(&analyze(true)).unwrap();
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = extra.plan(&target).apply(&mut image).unwrap();
    assert_eq!(report.confirmed_sensor_spots, 0, "{report:?}");
    assert_eq!(
        (report.stacked_regions, report.spots.len()),
        (1, 1),
        "{report:?}"
    );
    assert!(report.warning.is_none());
    let before = worst_error(&original.pixels, &clean, center, 12.0);
    let after = worst_error(&image.pixels, &clean, center, 12.0);
    assert!(
        before > 0.01 && after < 0.2 * before,
        "{after} of {before}; {report:?}"
    );
    let width = clean.width as usize;
    for (i, (&value, &original)) in image.pixels.iter().zip(&original.pixels).enumerate() {
        let radius =
            ((i % width) as f64 + 0.5 - center.0).hypot((i / width) as f64 + 0.5 - center.1);
        assert!(
            radius <= 40.0 || value == original,
            "sample {i} changed away from the dust"
        );
    }
    // A clean target of another view has nothing to confirm and stays untouched.
    let mut other = decode(dir.path(), "clean.DNG", &scene(1, false));
    let untouched = other.pixels.clone();
    let target = dirty::analyze_with_extra(&other, true).unwrap();
    let report = extra.plan(&target).apply(&mut other).unwrap();
    assert_eq!(
        (report.stacked_regions, &other.pixels),
        (0, &untouched),
        "{report:?}"
    );
}

#[test]
fn scenery_running_through_stacked_dust_is_not_erased() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // Dust only the stack can find, so nothing but the stack decides here.
    let dust = |x: f64, y: f64| {
        -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
    };
    // A thin dark wire through the middle of the dust, only in the target's scene.
    let wire = |x: f64, y: f64| {
        -0.06 * (-0.5 * ((y - center.1 - 0.2 * (x - center.0)) / 1.2).powi(2)).exp()
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("dust{index}.DNG"),
                &shaded(index, dust),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let wired = shaded(0, wire);
    let mut image = decode(
        dir.path(),
        "target.DNG",
        &shaded(0, |x, y| dust(x, y) + wire(x, y)),
    );
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    let width = wired.width as usize;
    for (i, (&value, &wired)) in image.pixels.iter().zip(&wired.pixels).enumerate() {
        let (x, y) = ((i % width) as f64 + 0.5, (i / width) as f64 + 0.5);
        // On the wire, even inside the dust, at most the dust itself may be taken back.
        if wire(x, y) < -0.03 {
            let ratio = (f64::from(value) - 1023.0) / (f64::from(wired) - 1023.0);
            assert!(
                ratio < 1.005,
                "wire brightened x{ratio} at {x},{y}; {report:?}"
            );
        }
    }
}

#[test]
fn stacked_dust_among_cloud_bands_is_scaled_from_the_stack_or_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    let bands = |amount: f64| move |_: f64, y: f64| amount * (y / 9.0).sin();
    for (depth, amount) in [(0.011, 0.004), (0.08, 0.02)] {
        let dust = move |x: f64, y: f64| {
            -depth * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
        };
        let evidence: Vec<_> = (0..4)
            .map(|index| {
                let image = decode(
                    dir.path(),
                    &format!("band{index}.DNG"),
                    &shaded(index, dust),
                );
                dirty::analyze_with_extra(&image, true).unwrap()
            })
            .collect();
        let calibration = Calibration::new(&evidence).unwrap();
        // Only the target's sky is banded.
        let banded = shaded(0, bands(amount));
        let original = shaded(0, |x, y| dust(x, y) + bands(amount)(x, y));
        let mut image = decode(dir.path(), "target.DNG", &original);
        let target = dirty::analyze_with_extra(&image, true).unwrap();
        let report = calibration.plan(&target).apply(&mut image).unwrap();
        if depth < 4.0 * amount {
            // Hardly deeper than the bands: any correction would follow them instead.
            assert!(
                image.pixels == original.pixels,
                "banded sky changed: {report:?}"
            );
        } else {
            // Clearly deeper: the stack's shape removes the dust and the bands remain.
            let before = worst_error(&original.pixels, &banded, center, 16.0);
            let after = worst_error(&image.pixels, &banded, center, 16.0);
            assert!(
                before > 0.07 && after < 0.25 * before,
                "{after} of {before}; {report:?}"
            );
        }
    }
}

#[test]
fn extra_refinement_treats_a_color_mosaic_developed_as_monochrome_evenly() {
    let dir = tempfile::tempdir().unwrap();
    let center = (300.5, 200.5);
    let dust = |x: f64, y: f64| {
        let radius = (x - center.0).hypot(y - center.1);
        -0.3 * ((7.5 - radius) / 1.5).clamp(0.0, 1.0)
    };
    // Four very different site responses, as a Bayer mosaic of a tinted scene has.
    let mosaic = |index: usize, dust: &dyn Fn(f64, f64) -> f64| {
        let site = |x: f64, y: f64| [0.0, -0.7, -0.5, -1.4][(y as usize % 2) * 2 + x as usize % 2];
        shaded(index, |x, y| dust(x, y) + site(x, y))
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("mosaic{index}.DNG"),
                &mosaic(index, &dust),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let clean = mosaic(0, &|_, _| 0.0);
    let original = mosaic(0, &dust);
    let mut image = decode(dir.path(), "target.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration.plan(&target).apply(&mut image).unwrap();
    assert!(report.warning.is_none(), "{report:?}");
    let before = worst_error(&original.pixels, &clean, center, 16.0);
    let after = worst_error(&image.pixels, &clean, center, 16.0);
    assert!(
        before > 0.25 && after < 0.4 * before,
        "{after} of {before}; {report:?}"
    );
    assert!(
        worst_error(&image.pixels, &clean, center, 4.0) < 0.02,
        "{report:?}"
    );
}

#[test]
fn curved_dirt_near_a_scene_edge_uses_its_enclosing_shape_not_a_large_circle() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = |index: usize, dust: bool| {
        let mut source = curved_scene(index, dust.then_some((320.5, 160.5)));
        for y in 202..source.height {
            for x in 0..source.width {
                let pixel = &mut source.pixels[(y * source.width + x) as usize];
                *pixel = (f64::from(source.black)
                    + (f64::from(*pixel) - f64::from(source.black)) * 0.45)
                    .round() as u16;
            }
        }
        source
    };
    let mut images: Vec<_> = (0..4)
        .map(|index| {
            decode(
                dir.path(),
                &format!("edge{index}.DNG"),
                &fixture(index, true),
            )
        })
        .collect();
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze_with_extra(image, true).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for (index, (image, evidence)) in images.iter_mut().zip(&evidence).enumerate() {
        let clean = decode(
            dir.path(),
            &format!("clean{index}.DNG"),
            &fixture(index, false),
        );
        let original = image.pixels.clone();
        let report = calibration.plan(evidence).apply(image).unwrap();
        let (mut before, mut after) = (0u64, 0u64);
        for (i, ((&original, &corrected), &clean)) in original
            .iter()
            .zip(&image.pixels)
            .zip(&clean.pixels)
            .enumerate()
        {
            if original.abs_diff(clean) > 20 {
                before += u64::from(original.abs_diff(clean));
                after += u64::from(corrected.abs_diff(clean));
            }
            let x = (i % image.metadata.width) as f64 + image.metadata.crop[0] as f64;
            let y = (i / image.metadata.width) as f64 + image.metadata.crop[1] as f64;
            if (x - 320.5).hypot(y - 160.5) > 75.0 {
                assert_eq!(corrected, original, "scene changed away from hair");
            }
        }
        assert!(before > 5_000);
        assert!(
            (after as f64) < before as f64 * 0.2,
            "near-edge curve {index} residual {after}/{before}; {report:?}"
        );
        assert_eq!(report.clipped_samples, 0);
    }
}

#[test]
fn clean_targets_duplicates_and_insufficient_evidence_are_noops() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let mut clean = decode(dir.path(), "clean.DNG", &scene(0, false));
    let before = clean.pixels.clone();
    let calibration = Calibration::new(&evidence).unwrap();
    let report = calibration
        .plan(&dirty::analyze(&clean).unwrap())
        .apply(&mut clean)
        .unwrap();
    assert!(report.confirmed_sensor_spots >= 2);
    assert_eq!(report.changed_samples, 0);
    assert!(report.spots.is_empty());
    assert_eq!(clean.pixels, before);

    let mut target = decode(dir.path(), "dust.DNG", &scene(0, true));
    let before = target.pixels.clone();
    let duplicates: Vec<_> = (0..6)
        .map(|index| {
            let mut similar = decode(dir.path(), "similar.DNG", &scene(0, true));
            let black = similar.metadata.black_level;
            for value in &mut similar.pixels {
                *value = (black + (f64::from(*value) - black) * (0.8 + f64::from(index) * 0.05))
                    .round() as u16;
            }
            dirty::analyze(&similar).unwrap()
        })
        .collect();
    for references in [&duplicates[..1], &duplicates[..], &evidence[..2]] {
        let report = Calibration::new(references)
            .unwrap()
            .plan(&dirty::analyze(&target).unwrap())
            .apply(&mut target)
            .unwrap();
        assert!(report.warning.unwrap().contains("three distinct"));
        assert_eq!(report.changed_samples, 0);
        assert_eq!(target.pixels, before);
    }
    assert!(Calibration::new(&[]).is_err());
}

#[test]
fn extra_correction_recovers_annular_dust_without_losing_its_separate_center_dot() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = Vec::new();
    let mut clean_images = Vec::new();
    for index in 0..4 {
        let clean = scene(index, false);
        let mut source = scene(index, false);
        for y in 0..source.height {
            for x in 0..source.width {
                let px = f64::from(x / 2 * 2) + 0.5;
                let py = f64::from(y / 2 * 2) + 0.5;
                let radius = (px - 320.5).hypot(py - 160.5);
                let shadow = 0.25 * (-0.5 * ((radius - 20.0) / 2.8).powi(2)).exp()
                    + 0.18 * (-0.5 * (radius / 2.0).powi(2)).exp();
                let pixel = &mut source.pixels[(y * source.width + x) as usize];
                *pixel = (f64::from(source.black)
                    + (f64::from(*pixel) - f64::from(source.black)) * (-shadow).exp())
                .round() as u16;
            }
        }
        images.push(decode(dir.path(), &format!("ring{index}.DNG"), &source));
        clean_images.push(decode(dir.path(), &format!("clean{index}.DNG"), &clean));
    }
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze_with_extra(image, true).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for ((image, clean), evidence) in images.iter_mut().zip(clean_images).zip(&evidence) {
        let original = image.pixels.clone();
        let report = calibration.plan(evidence).apply(image).unwrap();
        let (mut before, mut after) = (0u64, 0u64);
        for ((&original, &corrected), &clean) in
            original.iter().zip(&image.pixels).zip(&clean.pixels)
        {
            if original.abs_diff(clean) > 20 {
                before += u64::from(original.abs_diff(clean));
                after += u64::from(corrected.abs_diff(clean));
            }
        }
        assert!(before > 5_000);
        assert!(
            (after as f64) < before as f64 * 0.2,
            "annular shadow residual {after}/{before}; {report:?}"
        );
        assert_eq!(report.clipped_samples, 0);
    }
}

#[test]
fn confirmed_irregular_dirt_repairs_compact_targets_without_borrowing_reference_votes() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = |index: usize, compact: bool| {
        let mut source = scene(index, false);
        for y in 0..source.height {
            for x in 0..source.width {
                let px = f64::from(x / 2 * 2) + 0.5;
                let py = f64::from(y / 2 * 2) + 0.5;
                let radius = (px - 320.5).hypot(py - 160.5);
                let shadow = if compact {
                    0.24 * (-0.5 * (radius / 8.0).powi(2)).exp()
                } else {
                    0.6 / (1.0 + (radius - 20.0).exp())
                };
                let pixel = &mut source.pixels[(y * source.width + x) as usize];
                *pixel = (f64::from(source.black)
                    + (f64::from(*pixel) - f64::from(source.black)) * (-shadow).exp())
                .round() as u16;
            }
        }
        source
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("flat{index}.DNG"),
                &fixture(index, false),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let mixed: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("mixed{index}.DNG"),
                &fixture(index, index >= 2),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let mut target = decode(dir.path(), "target.DNG", &fixture(0, true));
    let original = target.pixels.clone();
    let measured = dirty::analyze_with_extra(&target, true).unwrap();
    let insufficient = Calibration::new(&mixed).unwrap().plan(&measured);
    assert_eq!(insufficient.apply(&mut target).unwrap().changed_samples, 0);
    assert_eq!(target.pixels, original);

    let report = Calibration::new(&evidence)
        .unwrap()
        .plan(&measured)
        .apply(&mut target)
        .unwrap();
    assert!(
        report.spots.iter().any(|spot| {
            (spot.sensor_x - 320.5).hypot(spot.sensor_y - 160.5) < 3.0 && spot.distinct_views >= 3
        }),
        "{report:?}"
    );
    let clean = decode(dir.path(), "clean.DNG", &scene(0, false));
    let (mut before, mut after) = (0u64, 0u64);
    for (i, ((&original, &corrected), &clean)) in original
        .iter()
        .zip(&target.pixels)
        .zip(&clean.pixels)
        .enumerate()
    {
        if original.abs_diff(clean) > 20 {
            before += u64::from(original.abs_diff(clean));
            after += u64::from(corrected.abs_diff(clean));
        }
        let x = (i % target.metadata.width) as f64;
        let y = (i / target.metadata.width) as f64;
        if (x - 320.5).hypot(y - 160.5) > 64.0 {
            assert_eq!(corrected, original);
        }
    }
    assert!(before > 5_000);
    assert!(
        (after as f64) < before as f64 * 0.2,
        "{after}/{before}; {report:?}"
    );
    assert_eq!(report.clipped_samples, 0);
}

#[test]
fn compact_target_support_preserves_small_unshadowed_scene_details() {
    let dir = tempfile::tempdir().unwrap();
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let mut source = scene(index, false);
            for y in 0..source.height {
                for x in 0..source.width {
                    let px = f64::from(x / 2 * 2) + 0.5;
                    let py = f64::from(y / 2 * 2) + 0.5;
                    let radius = (px - 320.5).hypot(py - 160.5);
                    let shadow = if (px - 331.5).abs() <= 4.0 && (py - 171.5).abs() <= 4.0 {
                        0.0
                    } else {
                        0.6 / (1.0 + ((radius - 80.0) / 4.0).exp())
                    };
                    let pixel = &mut source.pixels[(y * source.width + x) as usize];
                    *pixel = (f64::from(source.black)
                        + (f64::from(*pixel) - f64::from(source.black)) * (-shadow).exp())
                    .round() as u16;
                }
            }
            dirty::analyze_with_extra(
                &decode(dir.path(), &format!("hollow{index}.DNG"), &source),
                true,
            )
            .unwrap()
        })
        .collect();
    let mut source = scene(0, false);
    for y in 0..source.height {
        for x in 0..source.width {
            let px = f64::from(x / 2 * 2) + 0.5;
            let py = f64::from(y / 2 * 2) + 0.5;
            let radius = (px - 320.5).hypot(py - 160.5);
            let shadow = 0.24 * (-0.5 * (radius / 32.0).powi(2)).exp();
            let pixel = &mut source.pixels[(y * source.width + x) as usize];
            *pixel = (f64::from(source.black)
                + (f64::from(*pixel) - f64::from(source.black)) * (-shadow).exp())
            .round() as u16;
        }
    }
    let mut target = decode(dir.path(), "compact.DNG", &source);
    let original = target.pixels.clone();
    let report = Calibration::new(&evidence)
        .unwrap()
        .plan(&dirty::analyze_with_extra(&target, true).unwrap())
        .apply(&mut target)
        .unwrap();
    assert!(report.changed_samples > 1_000, "{report:?}");
    for y in 171..=172 {
        for x in 331..=332 {
            let i = y * target.metadata.width + x;
            assert!(
                target.pixels[i].abs_diff(original[i]) <= 2,
                "unsupported detail at {x},{y} brightened: {} vs {}",
                target.pixels[i],
                original[i]
            );
        }
    }
    assert_eq!(report.clipped_samples, 0);
}

#[test]
fn broader_shadows_allow_their_measured_centroid_to_move_slightly() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let mut source = scene(0, false);
    for y in 0..source.height {
        for x in 0..source.width {
            let r2 =
                ((f64::from(x) - 185.5).powi(2) + (f64::from(y) - 154.5).powi(2)) / 14.0f64.powi(2);
            let value = &mut source.pixels[(y * source.width + x) as usize];
            *value = (f64::from(source.black)
                + (f64::from(*value) - f64::from(source.black)) * (-0.1 * (-0.5 * r2).exp()).exp())
            .round() as u16;
        }
    }
    let mut target = decode(dir.path(), "broad.DNG", &source);
    let plan = calibration.plan(&dirty::analyze(&target).unwrap());
    assert_eq!(plan.report.spots.len(), 1, "{:?}", plan.report);
    let spot = &plan.report.spots[0];
    assert!((spot.sensor_x - 185.5).hypot(spot.sensor_y - 154.5) < 2.0);
    assert!(plan.apply(&mut target).unwrap().changed_samples > 500);
}

#[test]
fn correction_preserves_black_and_saturated_samples_and_caps_brightening() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let mut image = decode(dir.path(), "target.DNG", &scene(0, true));
    let plan = calibration.plan(&dirty::analyze(&image).unwrap());
    let center = 150 * 640 + 180;
    let black = image.metadata.black_level as u16;
    let white = image.metadata.white_level;
    image.pixels[center] = black - 2;
    image.pixels[center + 1] = white;
    image.pixels[center + 2] = white - 1;
    let report = plan.apply(&mut image).unwrap();
    assert_eq!(
        &image.pixels[center..center + 3],
        &[black - 2, white, white]
    );
    assert!(report.clipped_samples > 0);
    assert!(report.maximum_gain > 1.0 && report.maximum_gain <= 4.0);
}

#[test]
fn strength_zero_default_fractional_and_high_values_scale_the_same_correction() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let mut image = decode(dir.path(), "target.DNG", &scene(0, true));
    let before = image.pixels.clone();
    let plan = calibration.plan(&dirty::analyze(&image).unwrap());
    let default = plan.apply(&mut image).unwrap();
    let normal = image.pixels.clone();
    let black = image.metadata.black_level;
    let center = 150 * 640 + 180;
    let normal_gain = (f64::from(normal[center]) - black) / (f64::from(before[center]) - black);
    assert!(normal_gain > 1.1);
    let mut previous = before.clone();
    for strength in [0.0, 0.375, 1.0, 1.35, 2.0] {
        image.pixels.clone_from(&before);
        let report = plan.apply_with_strength(&mut image, strength).unwrap();
        assert_eq!(report.strength, strength);
        assert!(image.pixels.iter().zip(&previous).all(|(a, b)| a >= b));
        let expected = black + (f64::from(before[center]) - black) * normal_gain.powf(strength);
        assert!((f64::from(image.pixels[center]) - expected).abs() <= 2.0);
        for ((original, ordinary), scaled) in before.iter().zip(&normal).zip(&image.pixels) {
            if original == ordinary {
                assert!(original.abs_diff(*scaled) <= 1);
            }
        }
        if strength == 0.0 {
            assert_eq!(image.pixels, before);
            assert_eq!(report.changed_samples, 0);
            assert_eq!(report.clipped_samples, 0);
            assert_eq!(report.maximum_gain, 1.0);
            assert!(report.spots.iter().all(|spot| spot.maximum_gain == 1.0));
            assert!(report.warning.unwrap().contains("zero"));
        } else if strength == 1.0 {
            assert_eq!(image.pixels, normal);
            assert_eq!(
                serde_json::to_value(report).unwrap(),
                serde_json::to_value(&default).unwrap()
            );
        }
        previous.clone_from(&image.pixels);
    }
    for invalid in [-0.01, 2.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        image.pixels.clone_from(&before);
        assert!(plan.apply_with_strength(&mut image, invalid).is_err());
        assert_eq!(image.pixels, before);
    }
}

#[test]
fn high_strength_keeps_gain_white_and_black_safety_limits() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = Vec::new();
    for index in 0..4 {
        let mut source = scene(index, false);
        for y in 0..source.height {
            for x in 0..source.width {
                let (px, py) = (f64::from(x / 2 * 2) + 0.5, f64::from(y / 2 * 2) + 0.5);
                let r2 = ((px - 180.5).powi(2) + (py - 150.5).powi(2)) / 10.0f64.powi(2);
                let value = &mut source.pixels[(y * source.width + x) as usize];
                *value = (f64::from(source.black)
                    + (f64::from(*value) - f64::from(source.black)) * (-(-0.5 * r2).exp()).exp())
                .round() as u16;
            }
        }
        images.push(decode(dir.path(), &format!("{index}.DNG"), &source));
    }
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze(image).unwrap())
        .collect();
    let plan = Calibration::new(&evidence).unwrap().plan(&evidence[0]);
    let image = &mut images[0];
    let center = 150 * 640 + 180;
    let black = image.metadata.black_level as u16;
    let white = image.metadata.white_level;
    image.pixels[center] = black - 1;
    image.pixels[center + 1] = white;
    image.pixels[center + 2] = white - 1;
    let report = plan.apply_with_strength(image, 2.0).unwrap();
    assert_eq!(report.maximum_gain, 4.0);
    assert!(report.spots.iter().all(|spot| spot.maximum_gain <= 4.0));
    assert_eq!(
        &image.pixels[center..center + 3],
        &[black - 1, white, white]
    );
    assert!(report.clipped_samples > 0);
    assert!(image.pixels.iter().all(|&pixel| pixel <= white));
}

#[test]
fn aperture_lens_and_exposure_metadata_do_not_drive_correction() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let mut first = decode(dir.path(), "target.DNG", &scene(0, true));
    let mut second = decode(dir.path(), "other.DNG", &scene(0, true));
    second.metadata.aperture_f_number = Some(64.0);
    second.metadata.aperture_value_apex = Some(0.0);
    second.metadata.lens_model = Some("unreliable adapter metadata".into());
    second.metadata.exposure_seconds = Some(60.0);
    second.metadata.iso = Some(12500);
    let a = calibration
        .plan(&dirty::analyze(&first).unwrap())
        .apply(&mut first)
        .unwrap();
    let b = calibration
        .plan(&dirty::analyze(&second).unwrap())
        .apply(&mut second)
        .unwrap();
    assert!(a.changed_samples > 0);
    assert_eq!(first.pixels, second.pixels);
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap()
    );
}

#[test]
fn geometry_mismatch_is_not_used_as_sensor_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = references(dir.path(), None);
    let calibration = Calibration::new(&evidence).unwrap();
    let mut image = decode(dir.path(), "target.DNG", &scene(0, true));
    let matching = calibration.plan(&dirty::analyze(&image).unwrap());
    image.metadata.model = "another sensor".into();
    let original = image.pixels.clone();
    assert!(matching.apply(&mut image).is_err());
    let report = calibration
        .plan(&dirty::analyze(&image).unwrap())
        .apply(&mut image)
        .unwrap();
    assert!(report.warning.unwrap().contains("no matching"));
    assert_eq!(report.compatible_reference_images, 0);
    assert_eq!(image.pixels, original);
    image.pixels.pop();
    assert!(dirty::analyze(&image).is_err());
}

#[test]
fn sensor_coordinates_survive_all_orientations_crop_and_bayer_downsampling() {
    let dir = tempfile::tempdir().unwrap();
    let crop = [12, 8, 601, 451];
    let evidence = references(dir.path(), Some(crop));
    let calibration = Calibration::new(&evidence).unwrap();
    let mut source = scene(0, true);
    source.crop = Some(crop);
    let mut corrected = decode(dir.path(), "target.DNG", &source);
    let plan = calibration.plan(&dirty::analyze(&corrected).unwrap());
    assert_eq!(plan.report.spots.len(), 2, "{:?}", plan.report);
    plan.apply(&mut corrected).unwrap();
    let mut expected = scene(0, true);
    expected.crop = Some(crop);
    for y in 0..crop[3] as usize {
        let start = (y + crop[1] as usize) * 640 + crop[0] as usize;
        expected.pixels[start..start + crop[2] as usize]
            .copy_from_slice(&corrected.pixels[y * crop[2] as usize..(y + 1) * crop[2] as usize]);
    }
    let mut expected_color = None;
    for orientation in 1..=8 {
        source.orientation = orientation;
        expected.orientation = orientation;
        let mut target = decode(dir.path(), "target.DNG", &source);
        let target_plan = calibration.plan(&dirty::analyze(&target).unwrap());
        assert_eq!(
            serde_json::to_value(&target_plan.report).unwrap(),
            serde_json::to_value(&plan.report).unwrap()
        );
        target_plan.apply(&mut target).unwrap();
        let expected = decode(dir.path(), "expected.DNG", &expected);
        assert_eq!(target.pixels, expected.pixels, "orientation {orientation}");

        let channels = color::decode(&dir.path().join("target.DNG"), false, Green::Average, true)
            .unwrap()
            .channels;
        let mut first = None;
        for (_, mut channel) in channels {
            let report = target_plan.apply(&mut channel).unwrap();
            assert!(report.changed_samples > 0);
            if let Some(first) = &first {
                assert_eq!(
                    &channel.pixels, first,
                    "common gain must preserve equal channels"
                );
            } else {
                first = Some(channel.pixels.clone());
            }
            if orientation == 1 {
                expected_color = Some(channel.pixels.clone());
            } else {
                let mut reference = Dng::ramp(crop[2] / 2, crop[3] / 2);
                reference.pixels = expected_color.clone().unwrap();
                reference.orientation = orientation;
                let reference = decode(dir.path(), "expected-color.DNG", &reference);
                assert_eq!(channel.pixels, reference.pixels);
            }
        }
    }
}

#[test]
fn analysis_and_application_are_deterministic_across_thread_counts() {
    let dir = tempfile::tempdir().unwrap();
    let images: Vec<_> = (0..4)
        .map(|i| decode(dir.path(), &format!("{i}.DNG"), &scene(i, true)))
        .collect();
    let run = |threads, extra| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                let evidence: Vec<_> = images
                    .iter()
                    .map(|image| dirty::analyze_with_extra(image, extra).unwrap())
                    .collect();
                let plan = Calibration::new(&evidence).unwrap().plan(&evidence[0]);
                let mut target = decode(dir.path(), "target.DNG", &scene(0, true));
                let report = plan.apply(&mut target).unwrap();
                (target.pixels, serde_json::to_value(report).unwrap())
            })
    };
    for extra in [false, true] {
        assert_eq!(run(1, extra), run(2, extra), "extra {extra}");
    }
}

#[test]
fn cli_aliases_and_separate_reference_lists_parse_without_changing_other_flags() {
    let parse = |arguments: &[&str]| {
        Cli::try_parse_compat(
            ["dng-monochrome", "target.ARW"]
                .into_iter()
                .chain(arguments.iter().copied())
                .map(OsString::from),
        )
    };
    for flag in ["-fd", "--fd", "-fix-dirty", "--fix-dirty"] {
        let cli = parse(&[flag]).unwrap();
        assert!(cli.fix_dirty);
        assert!(!cli.fix_dirty_extra);
        assert!(cli.fix_dirty_list.is_empty());
        assert_eq!(cli.inputs, [PathBuf::from("target.ARW")]);
    }
    for flag in [
        "-fdx",
        "--fdx",
        "-fix-dirty-extra",
        "--fix-dirty-extra",
        "-fix-dust-extra",
        "--fix-dust-extra",
    ] {
        let cli = parse(&[flag, "-c", "-nc", "-nr", "-fds", "1.25"]).unwrap();
        assert!(cli.fix_dirty_extra && cli.color && cli.natural_color && cli.noise_reduction);
        assert_eq!(cli.fix_dust_strength, Some(1.25));
        assert_eq!(cli.inputs, [PathBuf::from("target.ARW")]);
    }
    for flag in ["-fdl", "--fdl", "-fix-dirty-list", "--fix-dirty-list"] {
        let cli = parse(&[
            flag,
            "ref1.DNG",
            "directory/",
            "-fdl",
            "ref2.ARW",
            "-c",
            "-nc",
        ])
        .unwrap();
        assert!(cli.color && cli.natural_color);
        assert_eq!(cli.inputs, [PathBuf::from("target.ARW")]);
        assert_eq!(
            cli.fix_dirty_list,
            ["ref1.DNG", "directory/", "ref2.ARW"].map(PathBuf::from)
        );
        assert!(parse(&[flag]).is_err());
    }
    for (save, load) in [
        ("-fdl-save", "-fdl-load"),
        ("--fdl-save", "--fdl-load"),
        ("-fix-dirty-save", "-fix-dirty-load"),
        ("--fix-dirty-save", "--fix-dirty-load"),
    ] {
        let cli = parse(&[save, "dirt.fdl", "-fdl", "ref1.DNG", "-c"]).unwrap();
        assert_eq!(cli.fix_dirty_save.as_deref(), Some(Path::new("dirt.fdl")));
        assert_eq!(cli.fix_dirty_list, [PathBuf::from("ref1.DNG")]);
        assert!(cli.color && cli.fix_dirty_load.is_none());
        let cli = parse(&[load, "dirt.fdl", "-fdx"]).unwrap();
        assert_eq!(cli.fix_dirty_load.as_deref(), Some(Path::new("dirt.fdl")));
        assert!(cli.fix_dirty_extra && cli.fix_dirty_save.is_none());
        assert_eq!(cli.inputs, [PathBuf::from("target.ARW")]);
        assert!(parse(&[load, "dirt.fdl", "-fdl", "ref1.DNG"]).is_err());
        assert!(parse(&[load, "dirt.fdl", save, "other.fdl"]).is_err());
        assert!(parse(&[save]).is_err() && parse(&[load]).is_err());
        assert_eq!(cli.fix_dirty_compression, None);
    }
    for flag in [
        "-fdl-compression",
        "--fdl-compression",
        "-fix-dirty-compression",
        "--fix-dirty-compression",
    ] {
        for level in 1..=9 {
            let cli = parse(&["-fdl-save", "dirt.fdl", flag, &level.to_string()]).unwrap();
            assert_eq!(cli.fix_dirty_compression, Some(level));
        }
        for level in ["0", "10", "-1", "best"] {
            assert!(parse(&["-fdl-save", "dirt.fdl", flag, level]).is_err());
        }
        // There is nothing to compress without saving.
        assert!(parse(&[flag, "5"]).is_err());
        assert!(parse(&["-fdl-load", "dirt.fdl", flag, "5"]).is_err());
    }
    // Only saving admits a run without inputs.
    let bare = |arguments: &[&str]| {
        Cli::try_parse_compat(
            std::iter::once("dng-monochrome")
                .chain(arguments.iter().copied())
                .map(OsString::from),
        )
    };
    let cli = bare(&["-fdl", "ref1.DNG", "-fdl-save", "dirt.fdl"]).unwrap();
    assert!(cli.inputs.is_empty());
    assert!(bare(&["-fdl", "ref1.DNG"]).is_err());
    assert!(bare(&["-fdl-load", "dirt.fdl"]).is_err());
    assert!(parse(&["--fix-dirty=true"]).is_err());
    assert!(parse(&["--fix-dirty-extra=true"]).is_err());
    assert!(parse(&["-func", "-fd"]).unwrap().function.as_deref() == Some("-fd"));
    let cli = parse(&["-func", "-fdx"]).unwrap();
    assert_eq!(cli.function.as_deref(), Some("-fdx"));
    assert!(!cli.fix_dirty_extra);
    let cli = parse(&["--", "-fd", "-fdl", "-fdx"]).unwrap();
    assert!(!cli.fix_dirty && !cli.fix_dirty_extra && cli.fix_dirty_list.is_empty());
}

#[test]
fn extra_cli_is_opt_in_and_enables_correction_for_mono_and_color() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    for index in 0..4 {
        curved_scene(index, Some((320.5, 160.5))).write(&refs.join(format!("curve{index}.DNG")));
    }
    let output = dir.path().join("output");
    for color in [false, true] {
        for flags in [&["-fd"][..], &["-fdx"], &["-fdx", "-fds", "0"]] {
            let mut invocation = command(&refs, &output);
            invocation.args(["--analyze", "--silent"]).args(flags);
            if color {
                invocation.args(["-c", "-4"]);
            }
            let result = success(invocation.output().unwrap());
            let reports: Vec<serde_json::Value> = String::from_utf8(result.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(reports.len(), 4);
            for report in reports {
                let dirt = &report["dirty_correction"];
                assert_eq!(dirt["reference_images"], 4);
                assert_eq!(dirt["clipped_samples"], 0);
                let changed = dirt["changed_samples"].as_u64().unwrap();
                if flags == ["-fdx"] {
                    assert!(changed > 100, "{report}");
                } else {
                    assert_eq!(changed, 0, "{report}");
                }
            }
        }
    }
    assert!(!output.exists());
}

#[test]
fn strength_aliases_accept_finite_fractional_values_and_reject_invalid_values() {
    let parse = |arguments: &[&str]| {
        Cli::try_parse_compat(
            ["dng-monochrome", "target.ARW"]
                .into_iter()
                .chain(arguments.iter().copied())
                .map(OsString::from),
        )
    };
    assert!(parse(&[]).unwrap().fix_dust_strength.is_none());
    for flag in ["-fds", "--fds", "-fix-dust-strength", "--fix-dust-strength"] {
        for value in ["0", "0.375", "1", "1.35", "2"] {
            let cli = parse(&[flag, value, "-c", "-nc"]).unwrap();
            assert_eq!(cli.fix_dust_strength, Some(value.parse::<f64>().unwrap()));
            assert!(cli.color && cli.natural_color);
        }
        for value in ["-0.01", "2.01", "NaN", "inf", "-inf", "1e309", "bad", ""] {
            assert!(parse(&[flag, value]).is_err(), "{flag} {value}");
        }
        assert!(parse(&[flag]).is_err());
    }
    assert_eq!(
        parse(&["-fds=1.125"]).unwrap().fix_dust_strength,
        Some(1.125)
    );
    let expression = parse(&["-func", "-fds"]).unwrap();
    assert_eq!(expression.function.as_deref(), Some("-fds"));
    assert!(expression.fix_dust_strength.is_none());
    let positional = parse(&["--", "-fds"]).unwrap();
    assert!(positional.fix_dust_strength.is_none());
    assert_eq!(positional.inputs.last().unwrap(), &PathBuf::from("-fds"));
}

#[test]
fn extra_stacking_uses_the_references_nearest_to_a_separate_target() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // Fifteen distinct views; only the newer half of the references carries a shadow too
    // faint for any single-frame fit, as dust that settled between two sessions.
    let view = |k: usize, dusty: bool| {
        let (sin, cos) = (k as f64 * 0.41).sin_cos();
        shaded(5, move |x, y| {
            let shadow = if dusty {
                -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
            } else {
                0.0
            };
            cos * (x / 640.0 - 0.5) + sin * (y / 480.0 - 0.5) + shadow
        })
    };
    let references: Vec<_> = (0..14)
        .map(|k| {
            let path = dir.path().join(format!("ref{k:02}.DNG"));
            view(k, k >= 7).write(&path);
            path
        })
        .collect();
    // The target sorts after every reference, so its neighbors are the newer frames.
    let target = dir.path().join("target.DNG");
    view(14, true).write(&target);
    let output = dir.path().join("output");
    success(
        command(&target, &output)
            .args(["-fdx", "--report", "-fdl"])
            .args(&references)
            .output()
            .unwrap(),
    );
    let report = json(&output.join("target.json"));
    let dirty = &report["dirty_correction"];
    assert_eq!(dirty["reference_images"], 14);
    assert_eq!(dirty["stacked_regions"], 1, "{dirty}");
    assert!(dirty["changed_samples"].as_u64().unwrap() > 0, "{dirty}");
}

#[test]
fn extra_stacking_keeps_the_dust_of_a_session_that_borders_on_a_clean_one() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // Seven views with a shadow too faint for any single-frame fit, then seven of a later
    // session that no longer has it. The target is the last frame of the first session, so
    // half of its neighbors, taken from both sides, are clean.
    let view = |k: usize, dusty: bool| {
        let (sin, cos) = (k as f64 * 0.41).sin_cos();
        shaded(5, move |x, y| {
            let shadow = if dusty {
                -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
            } else {
                0.0
            };
            cos * (x / 640.0 - 0.5) + sin * (y / 480.0 - 0.5) + shadow
        })
    };
    let references: Vec<_> = (0..14)
        .map(|k| {
            let path = dir.path().join(format!("ref{k:02}.DNG"));
            view(k, k < 7).write(&path);
            path
        })
        .collect();
    let convert = |name: &str, dusty: bool| {
        let target = dir.path().join(format!("{name}.DNG"));
        view(14, dusty).write(&target);
        let output = dir.path().join(format!("output-{name}"));
        success(
            command(&target, &output)
                .args(["-fdx", "--report", "-fdl"])
                .args(&references)
                .output()
                .unwrap(),
        );
        json(&output.join(format!("{name}.json")))["dirty_correction"].clone()
    };
    let dirty = convert("ref06b", true);
    assert_eq!(dirty["stacked_regions"], 1, "{dirty}");
    assert!(dirty["changed_samples"].as_u64().unwrap() > 0, "{dirty}");
    // The first frame of the clean session has nothing to repair, whatever came before.
    let clean = convert("ref06c", false);
    assert_eq!(clean["changed_samples"], 0, "{clean}");
}

#[test]
fn extra_one_sided_stacks_never_replace_a_shadow_fitted_on_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    let spot = |depth: f64, sigma: f64| {
        move |x: f64, y: f64| {
            -depth
                * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / sigma.powi(2)).exp()
        }
    };
    let tilt = |k: usize| {
        let (sin, cos) = (k as f64 * 0.41).sin_cos();
        move |x: f64, y: f64| cos * (x / 640.0 - 0.5) + sin * (y / 480.0 - 0.5)
    };
    // Seven views of a session shot wide open, where the dirt is a broad soft disc, then
    // seven of a later session that no longer has it.
    let evidence: Vec<_> = (0..14)
        .map(|k| {
            let dust = spot(if k < 7 { 0.1 } else { 0.0 }, 5.0);
            let image = decode(
                dir.path(),
                &format!("ref{k:02}.DNG"),
                &shaded(5, |x, y| tilt(k)(x, y) + dust(x, y)),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    // The last frame of the first session, stopped down: the dirt is smaller and deeper.
    // Only the stack of the earlier frames shows it, and on a banded sky that stack has no
    // more to offer than its own broad shape, a poor stand-in for the fitted shadow.
    let bands = |x: f64, y: f64| tilt(14)(x, y) + 0.02 * (y / 9.0).sin();
    let banded = shaded(5, bands);
    let original = shaded(5, |x, y| bands(x, y) + spot(0.2, 3.5)(x, y));
    let mut image = decode(dir.path(), "target.DNG", &original);
    let target = dirty::analyze_with_extra(&image, true).unwrap();
    let report = calibration
        .plan_near(&target, Some(7))
        .apply(&mut image)
        .unwrap();
    assert_eq!(
        (report.stacked_regions, report.spots.len()),
        (0, 1),
        "{report:?}"
    );
    let before = worst_error(&original.pixels, &banded, center, 12.0);
    let after = worst_error(&image.pixels, &banded, center, 12.0);
    assert!(
        before > 0.15 && after < 0.2 * before,
        "{after} of {before}; {report:?}"
    );
}

#[test]
fn saved_reference_data_reproduces_the_run_that_analyzed_the_references() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    // As above: dust that only stacking finds, in the newer half of fourteen views.
    let view = |k: usize, dusty: bool| {
        let (sin, cos) = (k as f64 * 0.41).sin_cos();
        shaded(5, move |x, y| {
            let shadow = if dusty {
                -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
            } else {
                0.0
            };
            cos * (x / 640.0 - 0.5) + sin * (y / 480.0 - 0.5) + shadow
        })
    };
    let references: Vec<_> = (0..14)
        .map(|k| {
            let path = dir.path().join(format!("ref{k:02}.DNG"));
            view(k, k >= 7).write(&path);
            path
        })
        .collect();
    let target = dir.path().join("target.DNG");
    view(14, true).write(&target);
    let saved = dir.path().join("dirt.fdl");
    let run = |name: &str, arguments: &[&std::ffi::OsStr]| {
        let output = dir.path().join(name);
        let result = success(
            command(&target, &output)
                .args(["-fdx", "--report"])
                .args(arguments)
                .output()
                .unwrap(),
        );
        (
            fs::read(output.join("target.png")).unwrap(),
            json(&output.join("target.json"))["dirty_correction"].clone(),
            String::from_utf8(result.stderr).unwrap(),
        )
    };
    let list: Vec<&std::ffi::OsStr> = std::iter::once("-fdl".as_ref())
        .chain(references.iter().map(|path| path.as_os_str()))
        .collect();
    let (expected, report, _) = run("analyzed", &list);
    assert_eq!(report["stacked_regions"], 1, "{report}");
    // Saving changes nothing in the run that saves.
    let saving: Vec<&std::ffi::OsStr> = list
        .iter()
        .copied()
        .chain(["-fdl-save".as_ref(), saved.as_os_str()])
        .collect();
    let (pixels, saving_report, text) = run("saving", &saving);
    assert!(text.contains("data of 14 reference(s) saved to"), "{text}");
    assert!(pixels == expected && saving_report == report);
    // Loading analyzes the target alone and still writes the same bytes.
    let (pixels, loaded_report, text) = run("loaded", &["-fdl-load".as_ref(), saved.as_os_str()]);
    assert!(text.contains("14 saved reference(s)"), "{text}");
    assert!(
        text.contains("dirt analysis [1/1]") && !text.contains("ref00.DNG"),
        "{text}"
    );
    assert!(pixels == expected, "loaded data changed the output");
    assert_eq!(loaded_report, report);
    // Compressed data is smaller and is recognized on loading without any flag.
    let compressed = dir.path().join("dirt-xz.fdl");
    let packing: Vec<&std::ffi::OsStr> = list
        .iter()
        .copied()
        .chain([
            "--fdl-save".as_ref(),
            compressed.as_os_str(),
            "-fdl-compression".as_ref(),
            "9".as_ref(),
        ])
        .collect();
    let (pixels, packing_report, text) = run("packing", &packing);
    assert!(text.contains("xz preset 9"), "{text}");
    assert!(pixels == expected && packing_report == report);
    assert!(
        fs::metadata(&compressed).unwrap().len() * 2 < fs::metadata(&saved).unwrap().len(),
        "compression saved too little"
    );
    let (pixels, unpacked_report, _) = run(
        "unpacked",
        &["--fix-dirty-load".as_ref(), compressed.as_os_str()],
    );
    assert!(pixels == expected && unpacked_report == report);
    // References alone can be saved, to the same bytes, without converting anything.
    let alone = dir.path().join("alone.fdl");
    let save_alone = || {
        Command::new(BIN)
            .args(["--threads", "2", "--jobs", "1", "-fdx", "--fix-dirty-save"])
            .arg(&alone)
            .args(&list)
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    success(save_alone());
    assert!(fs::read(&alone).unwrap() == fs::read(&saved).unwrap());
    assert!(!dir.path().join("dng-mono").exists());
    let refused = save_alone();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("refusing to overwrite"));
    // Saved data is for the detection mode it was made in, and damage is noticed.
    let fails = |name: &str, arguments: &[&std::ffi::OsStr], message: &str| {
        let result = command(&target, &dir.path().join(name))
            .args(arguments)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&result.stderr).into_owned();
        assert!(!result.status.success() && text.contains(message), "{text}");
        assert!(!dir.path().join(name).exists(), "{name} wrote outputs");
    };
    fails(
        "plain",
        &["-fd".as_ref(), "--fdl-load".as_ref(), saved.as_os_str()],
        "was saved with --fix-dirty-extra",
    );
    let bytes = fs::read(&saved).unwrap();
    let damaged = dir.path().join("damaged.fdl");
    let load: [&std::ffi::OsStr; 3] = ["-fdx".as_ref(), "-fdl-load".as_ref(), damaged.as_os_str()];
    fs::write(&damaged, &bytes[..bytes.len() - 9]).unwrap();
    fails("truncated", &load, "ends early");
    let mut flipped = bytes.clone();
    *flipped.last_mut().unwrap() ^= 1;
    fs::write(&damaged, &flipped).unwrap();
    fails("flipped", &load, "is damaged");
    let mut renamed = bytes.clone();
    renamed[40] ^= 1;
    fs::write(&damaged, &renamed).unwrap();
    fails("renamed", &load, "is damaged");
    fs::write(&damaged, b"not dirt data at all").unwrap();
    fails("foreign", &load, "not sensor-dirt data");
}

#[test]
fn saved_reference_data_serves_plain_correction_and_targets_among_the_references() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    references(&refs, None);
    let saved = dir.path().join("dirt.fdl");
    // One of the references is itself converted, another target is not a reference.
    let outside = dir.path().join("outside.DNG");
    scene(5, true).write(&outside);
    let run = |name: &str, arguments: &[&std::ffi::OsStr]| {
        let output = dir.path().join(name);
        success(
            command(&refs.join("ref2.DNG"), &output)
                .arg(&outside)
                .arg("--report")
                .args(arguments)
                .output()
                .unwrap(),
        );
        ["ref2", "outside"].map(|stem| {
            (
                fs::read(output.join(format!("{stem}.png"))).unwrap(),
                json(&output.join(format!("{stem}.json")))["dirty_correction"].clone(),
            )
        })
    };
    let expected = run(
        "analyzed",
        &[
            "-fdl".as_ref(),
            refs.as_os_str(),
            "--fdl-save".as_ref(),
            saved.as_os_str(),
        ],
    );
    for (_, report) in &expected {
        assert_eq!(report["spots"].as_array().unwrap().len(), 2, "{report}");
    }
    // Without extra detection only the confirmed spots are kept.
    assert!(fs::metadata(&saved).unwrap().len() < 4096);
    let loaded = run("loaded", &["--fix-dirty-load".as_ref(), saved.as_os_str()]);
    assert!(loaded == expected, "loaded data changed the outputs");
    let result = command(&outside, &dir.path().join("mixed"))
        .args(["-fdx", "-fdl-load"])
        .arg(&saved)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("was saved without --fix-dirty-extra")
    );
}

#[test]
fn saved_stack_data_is_read_only_for_the_references_a_target_needs() {
    let dir = tempfile::tempdir().unwrap();
    let center = (420.5, 120.5);
    let faint = |x: f64, y: f64| {
        -0.011 * (-0.5 * ((x - center.0).powi(2) + (y - center.1).powi(2)) / 36.0).exp()
    };
    let evidence: Vec<_> = (0..4)
        .map(|index| {
            let image = decode(
                dir.path(),
                &format!("faint{index}.DNG"),
                &shaded(index, faint),
            );
            dirty::analyze_with_extra(&image, true).unwrap()
        })
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    let paths: Vec<PathBuf> = (0..4)
        .map(|index| dir.path().join(format!("faint{index}.DNG")))
        .collect();
    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let mut bytes = Vec::new();
    calibration.save(&mut bytes, true, &paths, None).unwrap();
    assert!(calibration.save(Vec::new(), false, &paths, None).is_err());
    assert!(
        calibration
            .save(Vec::new(), true, &paths[..3], None)
            .is_err()
    );
    for level in [0, 10] {
        assert!(
            calibration
                .save(Vec::new(), true, &paths, Some(level))
                .is_err()
        );
    }
    let original = shaded(0, faint);
    let repaired = |calibration: &Calibration| {
        let mut image = decode(dir.path(), "target.DNG", &original);
        let target = dirty::analyze_with_extra(&image, true).unwrap();
        let report = calibration.plan(&target).apply(&mut image).unwrap();
        (image.pixels, report.stacked_regions)
    };
    let expected = repaired(&calibration);
    assert_eq!(expected.1, 1);
    let open = || dirty::Saved::open(std::io::Cursor::new(bytes.as_slice())).unwrap();
    let saved = open();
    assert!(saved.extra);
    assert_eq!(saved.references, paths);
    assert!(repaired(&saved.calibration(|_| true).unwrap()) == expected);
    // Fewer than three stacked views cannot confirm the shadow, exactly as when the
    // other references had not kept their stack data.
    let mut windowed = Calibration::new(&evidence).unwrap();
    windowed.keep_stacks(|index| index < 2);
    let partial = repaired(&open().calibration(|index| index < 2).unwrap());
    assert!(partial == repaired(&windowed));
    assert_eq!(partial.1, 0);
    assert!(partial.0 == original.pixels);
    // Compressed residuals are much shorter, decode to the same samples, can be skipped
    // like stored ones, and damage inside them is noticed.
    for level in [1, 6, 9] {
        let mut packed = Vec::new();
        calibration
            .save(&mut packed, true, &paths, Some(level))
            .unwrap();
        assert!(
            packed.len() * 2 < bytes.len(),
            "level {level}: {} of {} bytes",
            packed.len(),
            bytes.len()
        );
        let open = |bytes: &[u8]| dirty::Saved::open(std::io::Cursor::new(bytes.to_vec())).unwrap();
        let saved = open(&packed);
        assert!(saved.extra && saved.compressed && !open(&bytes).compressed);
        assert!(repaired(&saved.calibration(|_| true).unwrap()) == expected);
        assert!(repaired(&open(&packed).calibration(|index| index < 2).unwrap()) == partial);
        let mut damaged = packed.clone();
        let middle = damaged.len() - 40;
        damaged[middle] ^= 0x40;
        assert!(open(&damaged).calibration(|_| true).is_err());
        assert!(open(&damaged).calibration(|index| index < 2).is_ok());
    }
}

#[test]
fn all_references_are_analyzed_before_only_the_separate_target_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    references(&refs, None);
    let target = dir.path().join("target.DNG");
    scene(0, true).write(&target);
    let before = fs::read(&target).unwrap();
    let output = dir.path().join("output");
    let result = success(
        command(&target, &output)
            .arg("--report")
            .arg("-fdl")
            .arg(&refs)
            .output()
            .unwrap(),
    );
    let text = String::from_utf8(result.stderr).unwrap();
    assert!(text.contains("4 reference(s), 1 additional target(s)"));
    let barrier = text.find("analysis complete for all 5 RAWs").unwrap();
    assert!(text.find("dirt analysis [5/5]").unwrap() < barrier);
    assert!(barrier < text.find("[1/1] target.DNG").unwrap());
    assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
    assert!(output.join("target.png").is_file());
    let report = json(&output.join("target.json"));
    assert_eq!(report["dirty_correction"]["reference_images"], 4);
    assert_eq!(
        report["dirty_correction"]["spots"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        report["dirty_correction"]["changed_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(fs::read(&target).unwrap(), before);
    assert!(!refs.join("dng-mono").exists());
}

#[test]
fn multi_directory_reference_globs_and_recursive_directories_preserve_correction() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    references(&refs, None);
    let target = dir.path().join("target.DNG");
    scene(0, true).write(&target);
    let single = dir.path().join("single");
    success(
        command(&target, &single)
            .args(["--report", "-fdl"])
            .arg(&refs)
            .output()
            .unwrap(),
    );
    let expected = fs::read(single.join("target.png")).unwrap();
    let report = json(&single.join("target.json"));
    let expected_dirty = &report["dirty_correction"];
    assert_eq!(expected_dirty["reference_images"], 4);
    assert_eq!(expected_dirty["distinct_reference_views"], 4);
    assert_eq!(expected_dirty["spots"].as_array().unwrap().len(), 2);
    assert!(expected_dirty["changed_samples"].as_u64().unwrap() > 0);

    let first = refs.join("first");
    let second = refs.join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    for index in 0..4 {
        fs::rename(
            refs.join(format!("ref{index}.DNG")),
            if index < 2 { &first } else { &second }.join(format!("ref{}.DNG", index % 2)),
        )
        .unwrap();
    }
    for (name, inputs) in [
        (
            "files",
            vec![
                first.join("ref0.DNG"),
                first.join("ref1.DNG"),
                second.join("ref0.DNG"),
                second.join("ref1.DNG"),
            ],
        ),
        ("directories", vec![first.clone(), second.clone()]),
        (
            "recursive-overlap",
            vec![refs.clone(), first.join("ref0.DNG"), second.clone()],
        ),
        ("glob-files", vec![refs.join("*/*.DNG")]),
        ("glob-directories", vec![refs.join("*")]),
        (
            "glob-recursive-overlap",
            vec![refs.join("**/*.DNG"), first.join("ref0.DNG")],
        ),
    ] {
        let output = dir.path().join(name);
        let result = success(
            command(&target, &output)
                .args(["--report", "--fdl"])
                .args(inputs)
                .output()
                .unwrap(),
        );
        let text = String::from_utf8(result.stderr).unwrap();
        assert!(
            text.contains("4 reference(s), 1 additional target(s)"),
            "{text}"
        );
        assert!(text.contains("analysis complete for all 5 RAWs"), "{text}");
        assert_eq!(fs::read(output.join("target.png")).unwrap(), expected);
        assert_eq!(
            json(&output.join("target.json"))["dirty_correction"],
            *expected_dirty
        );
        assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
    }
    assert!(!refs.join("dng-mono").exists());
}

#[test]
fn failed_last_reference_prevents_every_output_but_all_inputs_are_attempted() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    let other = dir.path().join("z-other-references");
    fs::create_dir(&refs).unwrap();
    fs::create_dir(&other).unwrap();
    Dng::ramp(32, 24).write(&refs.join("a.DNG"));
    fs::write(other.join("z-bad.DNG"), b"broken raw").unwrap();
    let target = dir.path().join("target.DNG");
    Dng::ramp(32, 24).write(&target);
    let output = dir.path().join("output");
    let result = command(&target, &output)
        .arg("-fdl")
        .arg(&refs)
        .arg(&other)
        .output()
        .unwrap();
    assert!(!result.status.success());
    let text = String::from_utf8_lossy(&result.stderr);
    assert!(text.contains("dirt analysis [3/3]"), "{text}");
    assert!(
        text.contains("prepass failed for 1 RAW(s); no output files"),
        "{text}"
    );
    assert!(!text.contains("starting target processing"));
    assert!(!output.exists());

    fs::remove_file(other.join("z-bad.DNG")).unwrap();
    let result = command(dir.path(), &output).arg("-fd").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("single directory"));
    assert!(!output.exists());
}

#[test]
fn overlapping_reference_lists_are_deduplicated_and_default_list_is_equivalent() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    references(&refs, None);
    let output = dir.path().join("output");
    let implicit = success(
        command(&refs, &output)
            .args(["--analyze", "-fd", "-silent"])
            .output()
            .unwrap(),
    );
    let explicit = success(
        command(&refs, &output)
            .args(["--analyze", "-silent", "-fdl"])
            .arg(&refs)
            .arg("-fdl")
            .arg(refs.join("ref0.DNG"))
            .output()
            .unwrap(),
    );
    let strength_only = success(
        command(&refs, &output)
            .args(["--analyze", "-silent", "-fds", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(implicit.stdout, explicit.stdout);
    assert_eq!(implicit.stdout, strength_only.stdout);
    assert!(implicit.stderr.is_empty() && explicit.stderr.is_empty());
    assert_eq!(
        String::from_utf8(implicit.stdout).unwrap().lines().count(),
        4
    );
    assert!(!output.exists());
}

#[test]
fn target_only_images_cannot_manufacture_reference_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    let targets = dir.path().join("targets");
    fs::create_dir(&refs).unwrap();
    fs::create_dir(&targets).unwrap();
    for index in 0..4 {
        scene(index, true)
            .write(&if index < 2 { &refs } else { &targets }.join(format!("{index}.DNG")));
    }
    let output = dir.path().join("output");
    let result = success(
        command(&targets, &output)
            .args(["--analyze", "-silent", "-fdl"])
            .arg(&refs)
            .output()
            .unwrap(),
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert_eq!(text.lines().count(), 2);
    for line in text.lines() {
        let report: serde_json::Value = serde_json::from_str(line).unwrap();
        let dirty = &report["dirty_correction"];
        assert_eq!(dirty["reference_images"], 2);
        assert_eq!(dirty["distinct_reference_views"], 2);
        assert_eq!(dirty["confirmed_sensor_spots"], 0);
        assert_eq!(dirty["changed_samples"], 0);
    }
    assert!(!output.exists());
}

#[test]
fn unconfirmed_correction_preserves_default_output_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("single.DNG");
    Dng::ramp(64, 48).write(&input);
    let ordinary = dir.path().join("ordinary");
    let enabled = dir.path().join("enabled");
    success(command(&input, &ordinary).arg("--report").output().unwrap());
    let result = success(
        command(&input, &enabled)
            .args(["--report", "-fd"])
            .output()
            .unwrap(),
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("three distinct"));
    assert_eq!(
        fs::read(ordinary.join("single.png")).unwrap(),
        fs::read(enabled.join("single.png")).unwrap()
    );
    assert!(
        json(&ordinary.join("single.json"))
            .get("dirty_correction")
            .is_none()
    );
    assert_eq!(
        json(&enabled.join("single.json"))["dirty_correction"]["changed_samples"],
        0
    );
}

#[test]
fn zero_and_unit_strength_preserve_encoded_mono_rgb_and_channel_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    references(&refs, None);
    let input = dir.path().join("target.DNG");
    scene(0, true).write(&input);
    for color in [false, true] {
        for (variant, strength) in [
            ("plain", None),
            ("zero", Some("0")),
            ("implicit", None),
            ("one", Some("1")),
            ("strong", Some("1.5")),
        ] {
            let output = dir.path().join(format!("{color}-{variant}"));
            let mut invocation = command(&input, &output);
            invocation.args(["--report", "--transfer", "srgb"]);
            if color {
                invocation.args(["-c", "-4", "-nc"]);
            }
            if variant != "plain" {
                invocation.arg("-fdl").arg(&refs);
            }
            if let Some(strength) = strength {
                invocation.args(["-fds", strength]);
            }
            let result = success(invocation.output().unwrap());
            if variant == "zero" {
                assert!(
                    String::from_utf8_lossy(&result.stderr)
                        .contains("analysis complete for all 5 RAWs")
                );
            }
            let report = json(&output.join("target.json"));
            if variant == "plain" {
                assert!(report.get("dirty_correction").is_none());
            } else {
                assert_eq!(
                    report["dirty_correction"]["strength"].as_f64(),
                    Some(strength.unwrap_or("1").parse().unwrap())
                );
            }
        }
        for suffix in if color {
            &["", "R", "G1", "G2", "B"][..]
        } else {
            &[""][..]
        } {
            let bytes = |variant| {
                fs::read(
                    dir.path()
                        .join(format!("{color}-{variant}/target{suffix}.png")),
                )
                .unwrap()
            };
            assert_eq!(
                bytes("plain"),
                bytes("zero"),
                "zero strength {color}/{suffix}"
            );
            assert_eq!(
                bytes("implicit"),
                bytes("one"),
                "unit strength {color}/{suffix}"
            );
            assert_ne!(
                bytes("one"),
                bytes("strong"),
                "strength must reach {color}/{suffix}"
            );
        }
    }
}

#[test]
fn color_channels_receive_dirt_correction_before_development_and_natural_color() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    let evidence = references(&refs, None);
    let input = dir.path().join("target.DNG");
    let original = scene(0, true);
    original.write(&input);
    let image = raw::decode(&input, false).unwrap();
    let plan = Calibration::new(&evidence)
        .unwrap()
        .plan(&dirty::analyze(&image).unwrap());
    let output = dir.path().join("color");
    success(
        command(&input, &output)
            .args(["-c", "-4", "-nc", "--transfer", "srgb", "--report", "-fdl"])
            .arg(&refs)
            .output()
            .unwrap(),
    );
    let decoded = color::decode(&input, false, Green::Average, true).unwrap();
    let report = json(&output.join("target.json"));
    let mut changed = 0;
    for (name, mut channel) in decoded.channels {
        let correction = plan.apply(&mut channel).unwrap();
        changed += correction.changed_samples;
        let mut source = Dng::ramp(320, 240);
        source.pixels = channel.pixels;
        let path = dir.path().join(format!("reference{}.DNG", name.name()));
        source.write(&path);
        let expected = dir.path().join(format!("expected{}", name.name()));
        success(
            command(&path, &expected)
                .args(["--transfer", "srgb"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            png(&output.join(format!("target{}.png", name.name()))),
            png(&expected.join(format!("reference{}.png", name.name()))),
            "{} must be corrected before its independent range/tone mapping",
            name.name()
        );
        assert!(
            report["channels"][name.name()]["dirty_correction"]["changed_samples"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
    assert!(changed > 0);
    assert_eq!(report["dirty_correction"]["changed_samples"], changed);
    assert!(report["natural_balance"].is_object());
}

#[test]
#[ignore = "set DNG_DIRTY_SAMPLES to the original Sony ARW directory; read-only"]
fn original_sony_dust_is_not_crowded_out_by_textured_foreground() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_SAMPLES").unwrap());
    let inputs = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    let evidence: Vec<_> = inputs
        .iter()
        .map(|input| dirty::analyze(&raw::decode(&input.path, false).unwrap()).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for name in ["DSC01839.ARW", "DSC01845.ARW", "DSC01869.ARW"] {
        let mut image = raw::decode(&root.join(name), false).unwrap();
        let plan = calibration.plan(&dirty::analyze(&image).unwrap());
        assert!(
            plan.report
                .spots
                .iter()
                .any(|spot| { (spot.sensor_x - 244.0).hypot(spot.sensor_y - 566.0) < 20.0 }),
            "{name}: known recurring upper-left sky shadow was missed: {:?}",
            plan.report
        );
        assert!(plan.apply(&mut image).unwrap().changed_samples > 0);
    }
}

#[test]
#[ignore = "set DNG_DIRTY_UVFS_SAMPLES to Sony 20261110-UVFS, searched recursively; optionally DNG_DIRTY_BINARY"]
fn original_uvfs_sony_extra_correction_recovers_the_broad_recurring_shadow() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_UVFS_SAMPLES").unwrap());
    let binary = std::env::var_os("DNG_DIRTY_BINARY").unwrap_or_else(|| OsString::from(BIN));
    let inputs = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    // The session may be split into subdirectories; all of it is the reference set.
    let targets = ["DSC01990.ARW", "DSC01991.ARW", "DSC01992.ARW"].map(|name| {
        let input = inputs
            .iter()
            .find(|input| input.path.file_name().is_some_and(|file| file == name));
        input.unwrap().path.clone()
    });
    let output = success(
        Command::new(binary)
            .args(&targets)
            .args([
                "-c",
                "-fdx",
                "--analyze",
                "--threads",
                "2",
                "--jobs",
                "1",
                "-fdl",
            ])
            .args(inputs.iter().map(|input| &input.path))
            .output()
            .unwrap(),
    );
    let reports: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(reports.len(), targets.len());
    for target in targets {
        let report = reports
            .iter()
            .find(|report| report["source"].as_str() == target.to_str())
            .unwrap();
        let dirty = &report["dirty_correction"];
        assert!(
            dirty["spots"].as_array().unwrap().iter().any(|spot| {
                (spot["sensor_x"].as_f64().unwrap() - 1570.0)
                    .hypot(spot["sensor_y"].as_f64().unwrap() - 1230.0)
                    < 60.0
                    && spot["radius_sigma"].as_f64().unwrap() >= 25.0
                    && spot["maximum_gain"].as_f64().unwrap() > 1.05
                    && spot["distinct_views"].as_u64().unwrap() >= 3
            }),
            "{}: broad recurring shadow has no confirmed correction",
            target.display()
        );
        assert_eq!(dirty["clipped_samples"], 0);
    }
}

#[test]
#[ignore = "set DNG_DIRTY_UVFS_LEICA_SAMPLES to Leica 20261110-UVFS/DNG; optionally DNG_DIRTY_BINARY"]
fn original_uvfs_leica_extra_correction_recovers_the_near_edge_hair() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_UVFS_LEICA_SAMPLES").unwrap());
    let binary = std::env::var_os("DNG_DIRTY_BINARY").unwrap_or_else(|| OsString::from(BIN));
    let inputs = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    let target = root.join("M1101022.DNG");
    let output = success(
        Command::new(binary)
            .arg(&target)
            .args(["-fdx", "--analyze", "--threads", "2", "--jobs", "1", "-fdl"])
            .args(inputs.iter().map(|input| &input.path))
            .output()
            .unwrap(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let dirty = &report["dirty_correction"];
    assert!(
        dirty["spots"].as_array().unwrap().iter().any(|spot| {
            (spot["sensor_x"].as_f64().unwrap() - 3859.0)
                .hypot(spot["sensor_y"].as_f64().unwrap() - 2599.0)
                < 60.0
                && spot["radius_sigma"].as_f64().unwrap() >= 15.0
                && spot["maximum_gain"].as_f64().unwrap() > 1.04
                && spot["distinct_views"].as_u64().unwrap() >= 3
        }),
        "M1101022: confirmed near-edge hair has no locally measured correction"
    );
    assert_eq!(dirty["clipped_samples"], 0);
}

#[test]
fn textured_foreground_does_not_cause_proposal_starvation() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = Vec::new();
    for (index, (a, b)) in [(0.4, 0.1), (-0.2, 0.4), (-0.3, -0.3), (0.2, -0.4)]
        .into_iter()
        .enumerate()
    {
        let mut source = Dng::ramp(1280, 960);
        for y in 0..source.height {
            for x in 0..source.width {
                let (px, py) = (f64::from(x / 2 * 2) + 0.5, f64::from(y / 2 * 2) + 0.5);
                let mut light = 0.35f64.ln() + a * (px / 1280.0 - 0.5) + b * (py / 960.0 - 0.5);
                if y >= 210 {
                    let mut hash = (x / 2).wrapping_mul(0x9e37_79b1)
                        ^ (y / 2).wrapping_mul(0x85eb_ca77)
                        ^ (index as u32).wrapping_mul(0xc2b2_ae3d);
                    hash = (hash ^ (hash >> 16)).wrapping_mul(0x7feb_352d);
                    hash ^= hash >> 15;
                    light += (f64::from(hash) / f64::from(u32::MAX) - 0.5) * 1.6;
                }
                let r2 = ((px - 450.5).powi(2) + (py - 150.5).powi(2)) / 10.0f64.powi(2);
                light -= 0.04 * (-0.5 * r2).exp();
                source.pixels[(y * source.width + x) as usize] = (f64::from(source.black)
                    + f64::from(source.white - source.black) * light.exp().min(1.0))
                .round() as u16;
            }
        }
        images.push(decode(dir.path(), &format!("{index}.DNG"), &source));
    }
    let evidence: Vec<_> = images
        .iter()
        .map(|image| dirty::analyze(image).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for (image, evidence) in images.iter_mut().zip(&evidence) {
        let plan = calibration.plan(evidence);
        assert!(
            plan.report
                .spots
                .iter()
                .any(|spot| { (spot.sensor_x - 450.5).hypot(spot.sensor_y - 150.5) < 8.0 }),
            "faint smooth-sky shadow was crowded out: {:?}",
            plan.report
        );
        assert!(plan.apply(image).unwrap().changed_samples > 0);
    }
}

#[test]
#[ignore = "set DNG_DIRTY_LEICA_SAMPLES to the original BearLake DNG directory; read-only"]
fn original_leica_dust_is_not_lost_to_proposal_starvation() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_LEICA_SAMPLES").unwrap());
    let inputs = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    let evidence: Vec<_> = inputs
        .iter()
        .map(|input| dirty::analyze(&raw::decode(&input.path, false).unwrap()).unwrap())
        .collect();
    let calibration = Calibration::new(&evidence).unwrap();
    for name in [
        "M1100981.DNG",
        "M1100983.DNG",
        "M1100984.DNG",
        "M1100995.DNG",
    ] {
        let index = inputs
            .iter()
            .position(|input| input.path.file_name().unwrap() == name)
            .unwrap();
        let mut image = raw::decode(&inputs[index].path, false).unwrap();
        let plan = calibration.plan(&evidence[index]);
        assert!(
            plan.report
                .spots
                .iter()
                .any(|spot| (spot.sensor_x - 4392.0).hypot(spot.sensor_y - 529.0) < 20.0),
            "{name}: known recurring Leica sky shadow was missed: {:?}",
            plan.report
        );
        assert!(plan.apply(&mut image).unwrap().changed_samples > 0);
    }
}

#[test]
#[ignore = "set DNG_DIRTY_SAMPLES; optionally DNG_DIRTY_BINARY for a static executable"]
fn original_sony_separate_references_export_decodable_color_jxl() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_SAMPLES").unwrap());
    let binary = std::env::var_os("DNG_DIRTY_BINARY").unwrap_or_else(|| OsString::from(BIN));
    let output = tempfile::tempdir().unwrap();
    success(
        Command::new(binary)
            .arg(root.join("DSC01869.ARW"))
            .args([
                "-c",
                "-nc",
                "--transfer",
                "srgb",
                "--none",
                "--lossy",
                "--jxl",
                "--jxl-quality",
                "90",
                "--report",
                "--threads",
                "2",
                "--jobs",
                "1",
            ])
            .arg("-o")
            .arg(output.path())
            .arg("-fdl")
            .arg(&root)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 2);
    let report = json(&output.path().join("DSC01869.json"));
    let references = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    assert_eq!(
        report["dirty_correction"]["reference_images"],
        references.len()
    );
    assert!(
        report["dirty_correction"]["changed_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(report["natural_balance"].is_object());
    let bytes = fs::read(output.path().join("DSC01869.jxl")).unwrap();
    let image = jxl_oxide::JxlImage::builder()
        .pool(jxl_oxide::JxlThreadPool::none())
        .read(bytes.as_slice())
        .unwrap();
    assert!(!image.pixel_format().is_grayscale());
    assert!(!image.pixel_format().has_alpha());
    assert_eq!((image.width(), image.height()), (3504, 2336));
    assert_eq!(
        image.image_header().metadata.bit_depth.bits_per_sample(),
        report["additional_formats"][0]["bits"].as_u64().unwrap() as u32
    );
    let render = image.render_frame(0).unwrap();
    let mut pixels = vec![0u16; 3504 * 2336 * 3];
    assert_eq!(render.stream().write_to_buffer(&mut pixels), pixels.len());
    assert!(pixels.iter().max() > pixels.iter().min());
}

#[test]
#[ignore = "set DNG_DIRTY_LEICA_SAMPLES; optionally DNG_DIRTY_BINARY for a static executable"]
fn original_leica_separate_references_export_decodable_mono_jxl() {
    let root = PathBuf::from(std::env::var_os("DNG_DIRTY_LEICA_SAMPLES").unwrap());
    let binary = std::env::var_os("DNG_DIRTY_BINARY").unwrap_or_else(|| OsString::from(BIN));
    let output = tempfile::tempdir().unwrap();
    success(
        Command::new(binary)
            .arg(root.join("M1100981.DNG"))
            .args([
                "--tone-band",
                "0:0.25:0.33",
                "--none",
                "--lossy",
                "--jxl",
                "--jxl-quality",
                "90",
                "--report",
                "--threads",
                "2",
                "--jobs",
                "1",
                "-fds",
                "1.25",
            ])
            .arg("-o")
            .arg(output.path())
            .arg("-fdl")
            .arg(&root)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 2);
    let report = json(&output.path().join("M1100981.json"));
    let references = dng_monochrome::cli::discover(std::slice::from_ref(&root)).unwrap();
    assert_eq!(
        report["dirty_correction"]["reference_images"],
        references.len()
    );
    assert_eq!(report["dirty_correction"]["strength"], 1.25);
    assert!(
        report["dirty_correction"]["changed_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    let bytes = fs::read(output.path().join("M1100981.jxl")).unwrap();
    let image = jxl_oxide::JxlImage::builder()
        .pool(jxl_oxide::JxlThreadPool::none())
        .read(bytes.as_slice())
        .unwrap();
    assert!(image.pixel_format().is_grayscale());
    assert!(!image.pixel_format().has_alpha());
    assert_eq!((image.width(), image.height()), (9528, 6328));
    assert_eq!(
        image.image_header().metadata.bit_depth.bits_per_sample(),
        report["additional_formats"][0]["bits"].as_u64().unwrap() as u32
    );
    let render = image.render_frame(0).unwrap();
    let mut pixels = vec![0u16; 9528 * 6328];
    assert_eq!(render.stream().write_to_buffer(&mut pixels), pixels.len());
    assert!(pixels.iter().max() > pixels.iter().min());
}
