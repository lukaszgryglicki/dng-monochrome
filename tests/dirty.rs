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
    let run = |threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                let evidence: Vec<_> = images
                    .iter()
                    .map(|image| dirty::analyze(image).unwrap())
                    .collect();
                let plan = Calibration::new(&evidence).unwrap().plan(&evidence[0]);
                let mut target = decode(dir.path(), "target.DNG", &scene(0, true));
                let report = plan.apply(&mut target).unwrap();
                (target.pixels, serde_json::to_value(report).unwrap())
            })
    };
    assert_eq!(run(1), run(2));
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
        assert!(cli.fix_dirty_list.is_empty());
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
    assert!(parse(&["--fix-dirty=true"]).is_err());
    assert!(parse(&["-func", "-fd"]).unwrap().function.as_deref() == Some("-fd"));
    let cli = parse(&["--", "-fd", "-fdl"]).unwrap();
    assert!(!cli.fix_dirty && cli.fix_dirty_list.is_empty());
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
fn failed_last_reference_prevents_every_output_but_all_inputs_are_attempted() {
    let dir = tempfile::tempdir().unwrap();
    let refs = dir.path().join("references");
    fs::create_dir(&refs).unwrap();
    Dng::ramp(32, 24).write(&refs.join("a.DNG"));
    fs::write(refs.join("z-bad.DNG"), b"broken raw").unwrap();
    let target = dir.path().join("target.DNG");
    Dng::ramp(32, 24).write(&target);
    let output = dir.path().join("output");
    let result = command(&target, &output)
        .arg("-fdl")
        .arg(&refs)
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

    fs::remove_file(refs.join("z-bad.DNG")).unwrap();
    let result = command(&target, &output)
        .arg("-fdl")
        .arg(dir.path())
        .output()
        .unwrap();
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
    assert_eq!(implicit.stdout, explicit.stdout);
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
