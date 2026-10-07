use dng_monochrome::{
    expression::{Expression, FunctionPolicy},
    parameters::Parameters,
    range::{self, Histogram, RangeOptions},
    raw::{MonoImage, SensorMetadata},
    tone::{self, ToneCurve, Transfer},
};

#[test]
fn selective_srgb_zero_full_and_midpoint_preserve_all_code_quantization() {
    let image = image((0..=65535).collect(), 256, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, manual()).unwrap();
    let render = |optimized, source: Option<&str>, transfer| {
        let expression = source.map(|source| Expression::parse(source).unwrap());
        tone::render(
            &image,
            &hist,
            &range,
            optimized,
            expression.as_ref(),
            FunctionPolicy::Clip,
            tone::Transfers {
                png: transfer,
                jpeg: transfer,
            },
        )
        .unwrap()
    };
    for optimized in [false, true] {
        let baseline = render(optimized, None, Transfer::Linear);
        let zero = render(optimized, Some("srgb_band(x,0,.35,0)"), Transfer::Linear);
        assert_eq!(zero.png, baseline.png);
        assert_eq!(zero.jpeg, baseline.jpeg);
        let srgb = render(optimized, None, Transfer::Srgb);
        let full = render(optimized, Some("srgb_band(x,0,1,1)"), Transfer::Linear);
        assert_eq!(full.png, srgb.png);
        assert_eq!(full.jpeg, srgb.jpeg);
        let half = render(optimized, Some("srgb_band(x,0,1,.5)"), Transfer::Linear);
        let explicit = render(optimized, Some("0.5*x+0.5*srgb(x)"), Transfer::Linear);
        assert_eq!(half.png, explicit.png);
        assert_eq!(half.jpeg, explicit.jpeg);
        let lifted = render(optimized, Some("srgb_band(x,0,.35,.4)"), Transfer::Linear);
        assert_eq!(lifted.png[0], baseline.png[0]);
        assert_eq!(lifted.png[65535], baseline.png[65535]);
        for (code, (&before, &after)) in baseline.png.iter().zip(&lifted.png).enumerate() {
            let x = baseline.tone.map(f64::from(image.pixels[code]) / 65535.0);
            assert!(after >= before);
            if x >= 0.35 {
                assert_eq!(before, after);
            }
        }
        let codes = |values: &[u8]| -> std::collections::BTreeSet<u8> {
            image
                .pixels
                .iter()
                .zip(values)
                .filter(|(code, _)| baseline.tone.map(f64::from(**code) / 65535.0) <= 0.02)
                .map(|(_, &value)| value)
                .collect()
        };
        assert!(codes(&lifted.jpeg).len() > codes(&baseline.jpeg).len());
    }
}

#[test]
fn configurable_noise_geometry_and_envelope_preserve_known_flat_noise() {
    let pixels = normal_noise(512 * 512)
        .into_iter()
        .map(|noise| (6023.0 + 20.0 * noise).round() as u16)
        .collect();
    let image = image(pixels, 512, 1023.0, 16383);
    let hist = Histogram::new(&image.pixels).unwrap();
    for (side, quantile) in [(8, 0.05), (16, 0.25), (24, 0.5), (32, 0.75), (64, 0.95)] {
        let parameters = Parameters {
            noise_patch_side: side,
            noise_variance_quantile: quantile,
            ..Parameters::default()
        };
        let estimate =
            dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &parameters)
                .unwrap();
        assert_eq!(estimate.patch_side, side);
        let step = ((image.pixels.len() as f64 / parameters.noise_max_patches as f64)
            .sqrt()
            .ceil() as usize)
            .max(side);
        let across = (512 - side) / step + 1;
        assert_eq!(estimate.patches_examined, across * across);
        let measured = estimate.dark_sigma_codes.unwrap();
        assert!(
            (measured / 20.0 - 1.0).abs() < 0.2,
            "side={side}, quantile={quantile}, sigma={measured}"
        );
    }
    for parameters in [
        Parameters {
            noise_max_patches: 1,
            ..Parameters::default()
        },
        Parameters {
            noise_min_patches: 1000000,
            ..Parameters::default()
        },
        Parameters {
            noise_min_sigma: 100.0,
            ..Parameters::default()
        },
    ] {
        let estimate =
            dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &parameters)
                .unwrap();
        assert!(estimate.dark_sigma_codes.is_none());
    }
    let strict = Parameters {
        noise_texture_max: 0.01,
        ..Parameters::default()
    };
    let estimate =
        dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &strict).unwrap();
    assert_eq!(estimate.weak_texture_patches, 0);
    assert!(
        estimate
            .notes
            .iter()
            .any(|note| note.contains("Too few weak-texture"))
    );
}

#[test]
fn configurable_noise_fit_and_profile_evidence_change_decisions() {
    let mut image = noisy_scene(0.2, 25.0, false, false);
    let hist = Histogram::new(&image.pixels).unwrap();
    let baseline = range::estimate_noise(&image, &hist);
    assert!(baseline.read_noise_resolved);
    for parameters in [
        Parameters {
            noise_fit_min_span: 65535.0,
            ..Parameters::default()
        },
        Parameters {
            noise_fit_max_error: 0.0,
            ..Parameters::default()
        },
        Parameters {
            noise_patches_per_bin: 1000000,
            ..Parameters::default()
        },
        Parameters {
            noise_fit_min_bins: 128,
            noise_max_bins: 128,
            ..Parameters::default()
        },
    ] {
        let estimate =
            dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &parameters)
                .unwrap();
        assert!(estimate.model.is_none());
        assert!(!estimate.read_noise_resolved);
        assert!(estimate.dark_sigma_codes.is_some());
    }
    let no_refinement = Parameters {
        noise_refinement_passes: 0,
        ..Parameters::default()
    };
    let unrefined =
        dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &no_refinement)
            .unwrap();
    assert_ne!(
        serde_json::to_value(&baseline.bins).unwrap(),
        serde_json::to_value(&unrefined.bins).unwrap()
    );
    let span = 16383.0 - 1023.0;
    image.metadata.noise_profile = Some([0.2 / span, 25.0 / (span * span)]);
    assert!(range::estimate_noise(&image, &hist).read_noise_resolved);
    let strict = Parameters {
        noise_profile_tolerance: 1.0,
        ..Parameters::default()
    };
    let rejected =
        dng_monochrome::noise::estimate_noise_with_parameters(&image, &hist, &strict).unwrap();
    assert!(!rejected.read_noise_resolved);
    assert!(
        rejected
            .notes
            .iter()
            .any(|note| note.contains("more than 1x"))
    );
}

#[test]
fn configurable_range_targets_coherence_and_snr_keep_manual_bounds_authoritative() {
    let ramp = image((0..10000).collect(), 100, 0.0, 65535);
    let hist = Histogram::new(&ramp.pixels).unwrap();
    let stronger = Parameters {
        strength_target_min: 2.0,
        strength_target_max: 2.0,
        ..Parameters::default()
    };
    let options = RangeOptions {
        clip_strength: 9,
        ..RangeOptions::default()
    };
    let range = range::analyze_with_parameters(&ramp, &hist, options, &stronger).unwrap();
    assert_eq!((range.lower, range.upper), (200.0, 9799.0));
    let manual = range::analyze_with_parameters(&ramp, &hist, manual(), &stronger).unwrap();
    assert_eq!((manual.lower, manual.upper), (0.0, 9999.0));

    let mut pixels: Vec<u16> = (0..65536).map(|i| 2000 + (i % 256) as u16 * 12).collect();
    for y in 30..50 {
        for x in 30..50 {
            pixels[y * 256 + x] = 400;
        }
        for x in 100..120 {
            pixels[y * 256 + x] = 12000;
        }
    }
    let coherent = image(pixels, 256, 0.0, 65535);
    let hist = Histogram::new(&coherent.pixels).unwrap();
    let neighborhood = Parameters {
        coherent_radius: 2,
        coherent_neighbors: 13,
        ..Parameters::default()
    };
    let protected =
        range::analyze_with_parameters(&coherent, &hist, conservative(), &neighborhood).unwrap();
    assert_eq!((protected.lower, protected.upper), (400.0, 12000.0));
    let insufficient = Parameters {
        coherent_min_support: 1000000,
        ..neighborhood
    };
    let clipped =
        range::analyze_with_parameters(&coherent, &hist, conservative(), &insufficient).unwrap();
    assert!(clipped.lower > 400.0 && clipped.upper < 12000.0);
    let no_knee = Parameters {
        tail_knee_score: 1.0,
        ..insufficient
    };
    let retained =
        range::analyze_with_parameters(&coherent, &hist, conservative(), &no_knee).unwrap();
    assert_eq!((retained.lower, retained.upper), (400.0, 12000.0));

    let mut calibrated = image((1023..11023).collect(), 10000, 1023.0, 16383);
    calibrated.metadata.noise_profile = Some([0.0, (20.0f64 / 15360.0).powi(2)]);
    let hist = Histogram::new(&calibrated.pixels).unwrap();
    for snr in [1.0, 2.0, 3.0] {
        let parameters = Parameters {
            dark_snr: snr,
            ..Parameters::default()
        };
        let range = range::analyze_with_parameters(&calibrated, &hist, conservative(), &parameters)
            .unwrap();
        assert_eq!(range.lower, 1023.0 + 20.0 * snr);
        let range = range::analyze_with_parameters(
            &calibrated,
            &hist,
            RangeOptions {
                dark: Some(0.0),
                ..conservative()
            },
            &parameters,
        )
        .unwrap();
        assert_eq!(range.lower, 1023.0);
    }
    let mut ties = vec![100; 100];
    ties[0] = 0;
    ties[99] = 200;
    let tied = image(ties, 10, 0.0, 255);
    let hist = Histogram::new(&tied.pixels).unwrap();
    let parameters = Parameters {
        tied_span: 2.0,
        ..Parameters::default()
    };
    let range = range::analyze_with_parameters(&tied, &hist, options, &parameters).unwrap();
    assert_eq!((range.lower, range.upper), (99.0, 101.0));
}

#[test]
fn extreme_tone_parameters_remain_finite_monotonic_and_endpoint_preserving() {
    let image = image(
        (0..=65535)
            .map(|i| ((f64::from(i) / 65535.0).powi(4) * 65535.0).round() as u16)
            .collect(),
        256,
        0.0,
        65535,
    );
    let hist = Histogram::new(&image.pixels).unwrap();
    let mut range = range::analyze(&image, &hist, manual()).unwrap();
    range.noise = Default::default();
    for bins in [16, 257] {
        for (power, prior, minimum, cap) in [
            (0.0, 0.000001, 0.0, 64.0),
            (4.0, 0.000001, 1.0, 64.0),
            (0.5, 0.15, 0.0, 1.0),
            (4.0, 100.0, 0.0, 64.0),
        ] {
            for ev in [-16.0, 16.0] {
                for radius in [0, 16] {
                    let parameters = Parameters {
                        tone_bins: bins,
                        tone_density_power: power,
                        tone_density_prior: prior,
                        tone_slope_min: minimum,
                        tone_cap_min: cap,
                        tone_cap_max: cap,
                        tone_min_exposure_ev: ev,
                        tone_max_exposure_ev: ev,
                        tone_exposure_bias: ev,
                        tone_smoothing_radius: radius,
                        tone_smoothing_passes: 1,
                        tone_solver_upper: (1.0 / prior).max(32.0),
                        tone_solver_iterations: 128,
                        tone_contrast_strength: 1.0,
                        ..Parameters::default()
                    };
                    let curve =
                        ToneCurve::fit_with_parameters(&hist, &range, true, 0.0, &parameters)
                            .unwrap_or_else(|error| panic!("{parameters:?}: {error}"));
                    let mut previous = 0.0;
                    for i in 0..=1000 {
                        let value = curve.map(f64::from(i) / 1000.0);
                        assert!(value.is_finite() && (0.0..=1.0).contains(&value));
                        assert!(value >= previous);
                        previous = value;
                    }
                    assert_eq!(curve.map(0.0), 0.0);
                    assert_eq!(curve.map(1.0), 1.0);
                }
            }
        }
    }
}

#[test]
fn original_default_processing_fingerprints() {
    let expected = [
        [
            (6717569564993042814, 6953471804162552853),
            (9533947061431649438, 14967674621621298926),
        ],
        [
            (12103359711031240887, 14661262307880227674),
            (15488340430656129726, 7694179671921873682),
        ],
        [
            (15607283465254012220, 9231956993350605572),
            (17409184533110170649, 8099323266574252888),
        ],
    ];
    let skewed = image(
        (0..=65535)
            .map(|i| ((f64::from(i) / 65535.0).powi(4) * 65535.0).round() as u16)
            .collect(),
        256,
        0.0,
        65535,
    );
    for (case, image) in [
        skewed,
        noisy_scene(0.2, 25.0, false, false),
        noisy_scene(3.0, 900.0, true, true),
    ]
    .into_iter()
    .enumerate()
    {
        let hist = Histogram::new(&image.pixels).unwrap();
        let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
        for optimized in [false, true] {
            let rendered = tone::render(
                &image,
                &hist,
                &range,
                optimized,
                None,
                FunctionPolicy::Clip,
                tone::Transfers {
                    png: Transfer::Linear,
                    jpeg: Transfer::Srgb,
                },
            )
            .unwrap();
            let fingerprint = |values: Vec<u64>| {
                values
                    .into_iter()
                    .fold(0xcbf29ce484222325u64, |hash, value| {
                        (hash ^ value).wrapping_mul(0x100000001b3)
                    })
            };
            assert_eq!(
                (
                    fingerprint(rendered.png.iter().map(|v| u64::from(*v)).collect()),
                    fingerprint(rendered.jpeg.iter().map(|v| u64::from(*v)).collect()),
                ),
                expected[case][usize::from(optimized)],
                "original default pixels changed: case={case}, optimized={optimized}",
            );
        }
    }
}

fn image(pixels: Vec<u16>, width: usize, black: f64, white: u16) -> MonoImage {
    assert_eq!(pixels.len() % width, 0);
    let height = pixels.len() / width;
    MonoImage {
        pixels,
        source_metadata: Default::default(),
        metadata: SensorMetadata {
            make: "Synthetic".into(),
            model: "Mono".into(),
            raw_width: width,
            raw_height: height,
            width,
            height,
            crop: [0, 0, width, height],
            orientation: 1,
            storage_bits: 16,
            black_level: black,
            white_level: white,
            iso: None,
            standard_output_sensitivity: None,
            exposure_seconds: None,
            aperture_f_number: None,
            aperture_value_apex: None,
            focal_length_mm: None,
            lens_model: None,
            noise_profile: None,
        },
    }
}

fn manual() -> RangeOptions {
    RangeOptions {
        dark: Some(0.0),
        light: Some(0.0),
        ..RangeOptions::default()
    }
}

fn png_transfer(png: Transfer) -> tone::Transfers {
    tone::Transfers {
        png,
        ..tone::Transfers::default()
    }
}

fn conservative() -> RangeOptions {
    RangeOptions {
        clip_strength: 1,
        ..RangeOptions::default()
    }
}

#[test]
fn full_sixteen_bit_histogram_and_linear_output_are_lossless() {
    let image = image((0..=65535).collect(), 256, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    assert!(hist.bins.iter().all(|&n| n == 1));
    assert_eq!((hist.min, hist.max, hist.total), (0, 65535, 65536));
    let range = range::analyze(&image, &hist, manual()).unwrap();
    assert_eq!((range.lower, range.upper), (0.0, 65535.0));
    assert_eq!(range.retained_codes, 65536);
    assert_eq!(range.histogram_entropy_bits, 16.0);
    assert_eq!(range.retained_span_bits, 16.0);
    let out = tone::render(
        &image,
        &hist,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        png_transfer(Transfer::Linear),
    )
    .unwrap();
    assert_eq!(out.png, image.pixels);
    assert_eq!(out.stats.png_occupied_codes, 65536);
    assert_eq!(out.stats.png_min, 0);
    assert_eq!(out.stats.png_max, 65535);
}

#[test]
fn manual_percentages_use_the_full_histogram_not_calibrated_or_pretrimmed_data() {
    let image = image((0..10000).collect(), 100, 100.0, 9000);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(
        &image,
        &hist,
        RangeOptions {
            dark: Some(0.8),
            light: Some(1.2),
            ..RangeOptions::default()
        },
    )
    .unwrap();
    assert_eq!(range.lower, 80.0);
    assert_eq!(range.upper, 9879.0);
    assert_eq!(range.clipped_dark_percent, 0.8);
    assert_eq!(range.clipped_light_percent, 1.2);
    let zero = range::analyze(&image, &hist, manual()).unwrap();
    assert_eq!((zero.lower, zero.upper), (0.0, 9999.0));
    assert_eq!(zero.white_level_bits, (9001.0f64).log2());
}

#[test]
fn tied_values_report_actual_not_requested_clipping() {
    let image = image((0..100).flat_map(|i| vec![i; 10]).collect(), 100, 0.0, 100);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(
        &image,
        &hist,
        RangeOptions {
            dark: Some(1.5),
            light: Some(2.5),
            ..RangeOptions::default()
        },
    )
    .unwrap();
    assert_eq!((range.lower, range.upper), (1.0, 97.0));
    assert_eq!(range.clipped_dark_percent, 1.0);
    assert_eq!(range.clipped_light_percent, 2.0);
}

#[test]
fn auto_detects_different_tail_masses_and_preserves_clean_gradients() {
    let clean = image((0..10000).map(|i| 1000 + i).collect(), 100, 0.0, 65535);
    let hist = Histogram::new(&clean.pixels).unwrap();
    let range = range::analyze(&clean, &hist, conservative()).unwrap();
    assert_eq!(
        (range.clipped_dark_percent, range.clipped_light_percent),
        (0.0, 0.0)
    );
    let mut previous = 0.0;
    for count in [10, 50, 100] {
        let mut dirty = image(clean.pixels.clone(), 100, 0.0, 65535);
        for i in 0..count {
            dirty.pixels[(2 + 2 * (i / 49)) * 100 + 2 + 2 * (i % 49)] = 0;
        }
        for i in 0..count * 2 {
            dirty.pixels[(3 + 2 * (i / 49)) * 100 + 3 + 2 * (i % 49)] = 65535;
        }
        let hist = Histogram::new(&dirty.pixels).unwrap();
        let range = range::analyze(&dirty, &hist, conservative()).unwrap();
        assert!(range.lower > 0.0);
        assert!(range.upper < 65535.0 || count == 100);
        assert!(range.clipped_dark_percent > previous);
        assert!(range.clipped_dark_percent <= 2.0);
        assert!(range.clipped_light_percent <= 2.0);
        previous = range.clipped_dark_percent;
    }
}

#[test]
fn coherent_small_highlights_and_shadows_survive_automatic_tail_detection() {
    let mut pixels: Vec<u16> = (0..65536).map(|i| 2000 + (i % 256) as u16 * 12).collect();
    for y in 30..50 {
        for x in 30..50 {
            pixels[y * 256 + x] = 400;
        }
        for x in 100..120 {
            pixels[y * 256 + x] = 12000;
        }
    }
    let mut image = image(pixels, 256, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, conservative()).unwrap();
    assert_eq!((range.lower, range.upper), (400.0, 12000.0));
    assert!(range.dark_coherent_samples > 100);
    assert!(range.light_coherent_samples > 100);
    assert_eq!(range.clipped_dark_percent, 0.0);
    assert_eq!(range.clipped_light_percent, 0.0);
    image.pixels[0] = 0;
    image.pixels[255] = 65535;
    let hist = Histogram::new(&image.pixels).unwrap();
    let isolated = range::analyze(&image, &hist, conservative()).unwrap();
    assert_eq!((isolated.lower, isolated.upper), (400.0, 12000.0));
    assert_eq!(isolated.clipped_dark_percent, 100.0 / 65536.0);
    assert_eq!(isolated.clipped_light_percent, 100.0 / 65536.0);
    let manual = range::analyze(&image, &hist, manual()).unwrap();
    assert_eq!((manual.lower, manual.upper), (0.0, 65535.0));
    assert_eq!(
        (manual.dark_coherent_samples, manual.light_coherent_samples),
        (0, 0)
    );
}

#[test]
fn spatial_noise_estimate_recovers_known_gaussian_sigma() {
    let pixels = normal_noise(65536)
        .into_iter()
        .map(|normal| (8000.0 + 20.0 * normal).round() as u16)
        .collect();
    let image = image(pixels, 256, 0.0, 16383);
    let hist = Histogram::new(&image.pixels).unwrap();
    let noise = range::estimate_noise(&image, &hist);
    assert!(
        (16.0..24.0).contains(&noise.dark_sigma_codes.unwrap()),
        "{noise:?}"
    );
    assert!(
        (16.0..24.0).contains(&noise.light_sigma_codes.unwrap()),
        "{noise:?}"
    );
    assert!(noise.dark_samples >= 32 && noise.light_samples >= 32);
    assert!(
        !noise.read_noise_resolved,
        "one brightness level cannot resolve a shot/read-noise model"
    );
}

#[test]
fn clipping_strength_is_monotonic_and_manual_ends_always_win() {
    let image = image((0..=65535).collect(), 256, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    let (mut lower, mut upper) = (0.0, 65535.0);
    assert_eq!(RangeOptions::default().clip_strength, 3);
    for strength in 1..=9 {
        let options = RangeOptions {
            clip_strength: strength,
            ..RangeOptions::default()
        };
        let range = range::analyze(&image, &hist, options).unwrap();
        assert!(range.lower >= lower && range.upper <= upper);
        assert!(range.lower < range.upper);
        assert_eq!(range.clip_strength, strength);
        if strength == 1 {
            assert_eq!((range.lower, range.upper), (0.0, 65535.0));
        }
        if strength == 9 {
            assert!((range.clipped_dark_percent - 1.0).abs() < 100.0 / 65536.0);
            assert!((range.clipped_light_percent - 1.0).abs() < 100.0 / 65536.0);
        }
        let dark = range::analyze(
            &image,
            &hist,
            RangeOptions {
                dark: Some(0.0),
                ..options
            },
        )
        .unwrap();
        let light = range::analyze(
            &image,
            &hist,
            RangeOptions {
                light: Some(0.0),
                ..options
            },
        )
        .unwrap();
        let both = range::analyze(
            &image,
            &hist,
            RangeOptions {
                clip_strength: strength,
                ..manual()
            },
        )
        .unwrap();
        assert_eq!(dark.lower, 0.0);
        assert_eq!(dark.upper, range.upper);
        assert_eq!(light.upper, 65535.0);
        assert_eq!(light.lower, range.lower);
        assert_eq!((both.lower, both.upper), (0.0, 65535.0));
        (lower, upper) = (range.lower, range.upper);
    }
    for strength in [0, 10, 255] {
        assert!(
            RangeOptions {
                clip_strength: strength,
                ..RangeOptions::default()
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn strong_clipping_handles_ties_and_never_changes_a_manual_bound() {
    let mut pixels = vec![100; 100];
    pixels[0] = 0;
    pixels[99] = 200;
    let image = image(pixels, 10, 0.0, 255);
    let hist = Histogram::new(&image.pixels).unwrap();
    let (mut lower, mut upper) = (0.0, 200.0);
    for strength in 1..=9 {
        let range = range::analyze(
            &image,
            &hist,
            RangeOptions {
                clip_strength: strength,
                ..RangeOptions::default()
            },
        )
        .unwrap();
        assert!(range.lower >= lower && range.upper <= upper);
        assert!(range.upper > range.lower);
        (lower, upper) = (range.lower, range.upper);
    }
    assert_eq!((lower, upper), (99.5, 100.5));
    for options in [
        RangeOptions {
            dark: Some(2.0),
            light: None,
            clip_strength: 9,
        },
        RangeOptions {
            dark: None,
            light: Some(2.0),
            clip_strength: 9,
        },
    ] {
        let range = range::analyze(&image, &hist, options).unwrap();
        if options.dark.is_some() {
            assert_eq!(range.lower, 100.0);
        }
        if options.light.is_some() {
            assert_eq!(range.upper, 100.0);
        }
        assert!(range.upper > range.lower);
        assert!(range.warnings.iter().any(|v| v.contains("manual bounds")));
    }
}

#[test]
fn fitted_photographic_curves_preserve_endpoints_and_bound_perceptual_contrast() {
    for power in [0.2, 1.0, 4.0, 12.0] {
        let pixels = (0..=65535)
            .map(|i| ((f64::from(i) / 65535.0).powf(power) * 65535.0).round() as u16)
            .collect();
        let image = image(pixels, 256, 0.0, 65535);
        let hist = Histogram::new(&image.pixels).unwrap();
        let range = range::analyze(&image, &hist, manual()).unwrap();
        let curve = ToneCurve::fit(&hist, &range, true, 0.0);
        assert_eq!(curve.perceptual_curve.len(), 1025);
        assert_eq!(curve.perceptual_curve[0], 0.0);
        assert_eq!(curve.perceptual_curve[1024], 1.0);
        for pair in curve.perceptual_curve.windows(2) {
            let slope = (pair[1] - pair[0]) * 1024.0;
            assert!(
                (0.5125 - 1e-8..=2.5 + 1e-8).contains(&slope),
                "slope={slope}"
            );
        }
        let mut previous = 0.0;
        for i in 0..=65535 {
            let value = curve.map(f64::from(i) / 65535.0);
            assert!(value.is_finite() && (0.0..=1.0).contains(&value));
            assert!(value >= previous);
            previous = value;
        }
        assert_eq!(curve.map(0.0), 0.0);
        assert_eq!(curve.map(1.0), 1.0);
    }
}

#[test]
fn photographic_optimization_limits_gain_when_noise_increases() {
    let mut pixels: Vec<u16> = (0..65536).map(|i| 500 + (i % 1000) as u16).collect();
    pixels[0] = 0;
    pixels[65535] = 16383;
    let image = image(pixels, 256, 0.0, 16383);
    let hist = Histogram::new(&image.pixels).unwrap();
    let mut range = range::analyze(&image, &hist, manual()).unwrap();
    let mut fit = |variance| {
        range.noise.model = Some(dng_monochrome::noise::NoiseModel {
            shot_coefficient: 0.0,
            read_variance: variance,
            source: "synthetic",
        });
        range.noise.read_noise_resolved = true;
        ToneCurve::fit(&hist, &range, true, 0.0)
    };
    let low = fit(4.0);
    let high = fit(250000.0);
    assert!(high.exposure_factor <= low.exposure_factor);
    assert!(
        high.predicted_midtone_noise_display.unwrap()
            > low.predicted_midtone_noise_display.unwrap()
    );
    let m = high.input_median;
    let linear = high.exposure_factor * m / (1.0 + (high.exposure_factor - 1.0) * m);
    let index = (tone::srgb_encode(linear) * 1024.0).floor() as usize;
    let slope = (high.perceptual_curve[index + 1] - high.perceptual_curve[index]) * 1024.0;
    assert!(slope <= 1.1125 + 1e-8, "noisy midtone slope={slope}");
}

fn normal_noise(count: usize) -> Vec<f64> {
    let mut state = 1234567u64;
    let mut uniform = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (((state >> 32) as f64) + 0.5) / 4294967296.0
    };
    (0..count)
        .map(|_| (-2.0 * uniform().ln()).sqrt() * (std::f64::consts::TAU * uniform()).cos())
        .collect()
}

fn noisy_scene(shot: f64, read: f64, correlated: bool, texture: bool) -> MonoImage {
    let width = 1024;
    let height = 512;
    let normals = normal_noise((width + 1) * (height + 1));
    let mut pixels = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let signal = 32.0 + 12000.0 * ((x / 16) as f64 / 63.0).powi(2);
            let offset = y * (width + 1) + x;
            let noise = if correlated {
                (normals[offset]
                    + normals[offset + 1]
                    + normals[offset + width + 1]
                    + normals[offset + width + 2])
                    * 0.5
            } else {
                normals[offset]
            };
            let structure = if texture && (y / 16) % 4 != 0 {
                500.0 * ((y % 16) as f64 * 0.4).sin()
            } else {
                0.0
            };
            pixels.push(
                (1023.0 + signal + noise * (shot * signal + read).sqrt() + structure)
                    .round()
                    .clamp(0.0, 16383.0) as u16,
            );
        }
    }
    image(pixels, width, 1023.0, 16383)
}

#[test]
fn poisson_gaussian_model_and_noise_limited_dr_recover_known_ground_truth() {
    for (shot, read) in [(0.2, 25.0), (3.0, 900.0), (25.0, 40000.0)] {
        let image = noisy_scene(shot, read, false, false);
        let hist = Histogram::new(&image.pixels).unwrap();
        let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
        let model = range
            .noise
            .model
            .expect("known linear variance model must be found");
        assert!(
            (model.shot_coefficient / shot - 1.0).abs() < 0.15,
            "{:#?}",
            range.noise
        );
        assert!(
            (model.read_variance / read - 1.0).abs() < 0.2,
            "{:#?}",
            range.noise
        );
        assert!(range.noise.read_noise_resolved, "{:#?}", range.noise);
        let known_floor = 0.5 * (shot + (shot * shot + 4.0 * read).sqrt());
        let expected = ((range.upper - 1023.0) / known_floor).max(1.0).log2();
        assert!((range.dynamic_range.snr1_stops.unwrap() - expected).abs() < 0.2);
        assert!(range.dynamic_range.snr3_stops.unwrap() < range.dynamic_range.snr1_stops.unwrap());
        assert!(range.dynamic_range.snr1_stops.unwrap() < range.retained_span_bits);
    }
}

#[test]
fn correlated_noise_and_heavy_texture_do_not_inflate_estimated_dr() {
    for (correlated, texture) in [(true, false), (false, true)] {
        let image = noisy_scene(0.2, 400.0, correlated, texture);
        let hist = Histogram::new(&image.pixels).unwrap();
        let noise = range::estimate_noise(&image, &hist);
        let signal = noise.shadow_signal_codes.unwrap();
        let expected = (400.0 + 0.2 * signal).sqrt();
        let relative_error = (noise.dark_sigma_codes.unwrap() / expected - 1.0).abs();
        assert!(
            relative_error < 0.2,
            "relative error={relative_error}; {noise:#?}"
        );
        if correlated {
            assert!(noise.correlation_ratio.unwrap() > 1.5);
            assert!(noise.notes.iter().any(|v| v.contains("correlation")));
        } else {
            assert!(noise.weak_texture_patches < noise.patches_examined / 2);
            assert!(noise.weak_texture_patches > 100);
        }
    }
}

#[test]
fn clipping_is_excluded_from_noise_patches_but_below_black_samples_are_not() {
    let mut image = noisy_scene(0.2, 10000.0, false, false);
    image.pixels[..1024 * 32].fill(0);
    image.pixels[1024 * 32..1024 * 64].fill(16383);
    let hist = Histogram::new(&image.pixels).unwrap();
    let noise = range::estimate_noise(&image, &hist);
    assert!(noise.censored_patches >= 256);
    assert!(noise.weak_texture_patches > 1000);
    assert!(image.pixels.iter().any(|v| *v > 0 && *v < 1023));
    let model = noise.model.unwrap();
    assert!(
        (model.read_variance / 10000.0 - 1.0).abs() < 0.2,
        "{noise:#?}"
    );
}

#[test]
fn dng_noise_profile_is_converted_from_normalized_units_and_disagreements_are_reported() {
    let mut image = noisy_scene(0.2, 25.0, false, false);
    let span = f64::from(image.metadata.white_level) - image.metadata.black_level;
    image.metadata.noise_profile = Some([0.2 / span, 25.0 / (span * span)]);
    let hist = Histogram::new(&image.pixels).unwrap();
    let noise = range::estimate_noise(&image, &hist);
    let model = noise.model.unwrap();
    assert_eq!(model.source, "dng_noise_profile");
    assert!((model.shot_coefficient - 0.2).abs() < 1e-12);
    assert!((model.read_variance - 25.0).abs() < 1e-12);
    assert!(noise.read_noise_resolved);
    let threshold = model.signal_for_snr(3.0);
    assert!((threshold / model.variance(threshold).sqrt() - 3.0).abs() < 1e-12);
    image.metadata.noise_profile = Some([0.000000001, 0.000000000001]);
    let noise = range::estimate_noise(&image, &hist);
    assert!(!noise.read_noise_resolved);
    assert!(noise.notes.iter().any(|note| note.contains("disagrees")));
    assert!(
        noise.sigma_at(2000.0).unwrap() > 10.0,
        "rendering must not reuse a rejected profile"
    );
}

#[test]
fn noiseless_gradients_and_tiny_images_do_not_invent_a_noise_measurement() {
    for image in [
        image((1..=65535).collect(), 255, 0.0, 65535),
        image(vec![1; 256], 16, 0.0, 16383),
    ] {
        let hist = Histogram::new(&image.pixels).unwrap();
        let noise = range::estimate_noise(&image, &hist);
        assert!(noise.dark_sigma_codes.is_none());
        assert!(!noise.read_noise_resolved);
        assert!(!noise.notes.is_empty());
    }
}

#[test]
fn calibrated_clipping_is_reported_separately() {
    let image = image((0..10000).collect(), 100, 1000.0, 9000);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
    assert_eq!(range.below_black_percent, 10.0);
    assert_eq!(range.above_white_percent, 9.99);
    assert_eq!(range.lower, 1000.0);
    assert_eq!(range.upper, 9000.0);
}

#[test]
fn constant_black_saturated_and_tiny_images_do_not_invent_contrast() {
    for (pixels, black, white, expected) in [
        (vec![5000; 64], 0.0, 10000, 32768),
        ((0..64).collect(), 1023.0, 16383, 0),
        ((100..164).collect(), 0.0, 99, 65535),
        (vec![5], 0.0, 10, 32768),
    ] {
        let image = image(pixels, 1, black, white);
        let hist = Histogram::new(&image.pixels).unwrap();
        let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
        assert!(!range.warnings.is_empty());
        let out = tone::render(
            &image,
            &hist,
            &range,
            false,
            None,
            FunctionPolicy::Clip,
            png_transfer(Transfer::Linear),
        )
        .unwrap();
        assert!(out.png.iter().all(|&v| v == expected));
        assert!(range.estimated_snr_span_stops.is_none());
    }
}

#[test]
fn invalid_percentages_and_collapsed_manual_range_are_errors() {
    for options in [
        RangeOptions {
            dark: Some(-1.0),
            light: None,
            ..RangeOptions::default()
        },
        RangeOptions {
            dark: Some(f64::NAN),
            light: None,
            ..RangeOptions::default()
        },
        RangeOptions {
            dark: None,
            light: Some(f64::INFINITY),
            ..RangeOptions::default()
        },
        RangeOptions {
            dark: Some(100.0),
            light: None,
            ..RangeOptions::default()
        },
        RangeOptions {
            dark: Some(50.0),
            light: Some(50.0),
            ..RangeOptions::default()
        },
    ] {
        assert!(options.validate().is_err());
    }
    assert!(Histogram::new(&[]).is_err());
    let mut pixels = vec![100; 100];
    pixels[0] = 0;
    pixels[99] = 200;
    let image = image(pixels, 10, 0.0, 255);
    let hist = Histogram::new(&image.pixels).unwrap();
    assert!(
        range::analyze(
            &image,
            &hist,
            RangeOptions {
                dark: Some(2.0),
                light: Some(2.0),
                ..RangeOptions::default()
            },
        )
        .is_err()
    );
}

#[test]
fn tone_curve_is_bounded_monotonic_and_endpoint_preserving() {
    for exposure_factor in [0.25, 0.5, 1.0, 2.0, 8.0] {
        let curve = ToneCurve {
            optimized: true,
            optimize_strength: 1.0,
            exposure_factor,
            input_median: 0.1,
            contrast_strength: 0.0,
            predicted_midtone_noise_display: None,
            perceptual_curve: Vec::new(),
        };
        let mut previous = 0.0;
        for i in 0..=65535 {
            let y = curve.map(f64::from(i) / 65535.0);
            assert!((0.0..=1.0).contains(&y));
            assert!(y >= previous);
            previous = y;
        }
        assert_eq!(curve.map(0.0), 0.0);
        assert_eq!(curve.map(1.0), 1.0);
    }
}

#[test]
fn optimizer_lifts_dark_midtones_and_keeps_highlights() {
    let mut pixels: Vec<u16> = (0..10000).map(|i| 1000 + i / 10).collect();
    pixels[0] = 0;
    pixels[9999] = 16383;
    let image = image(pixels, 100, 0.0, 16383);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, manual()).unwrap();
    let auto = tone::render(
        &image,
        &hist,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        png_transfer(Transfer::Srgb),
    )
    .unwrap();
    let best = tone::render(
        &image,
        &hist,
        &range,
        true,
        None,
        FunctionPolicy::Clip,
        png_transfer(Transfer::Srgb),
    )
    .unwrap();
    assert!(best.png[5000] > auto.png[5000]);
    assert!((0.25..=64.0).contains(&best.tone.exposure_factor));
    assert_eq!((best.png[0], best.png[9999]), (0, 65535));
    assert_eq!(auto.tone.exposure_factor, 1.0);
}

#[test]
fn png_transfer_and_jpeg_are_visually_consistent() {
    let image = image(vec![0, 16384, 32768, 65535], 2, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, manual()).unwrap();
    let linear = tone::render(
        &image,
        &hist,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        tone::Transfers {
            png: Transfer::Linear,
            jpeg: Transfer::Srgb,
        },
    )
    .unwrap();
    assert_eq!(linear.png, image.pixels);
    assert_eq!(linear.jpeg, [0, 137, 188, 255]);
    let srgb = tone::render(
        &image,
        &hist,
        &range,
        false,
        None,
        FunctionPolicy::Clip,
        tone::Transfers {
            png: Transfer::Srgb,
            jpeg: Transfer::Srgb,
        },
    )
    .unwrap();
    assert_eq!(srgb.jpeg, linear.jpeg);
    for (&png, &jpeg) in srgb.png.iter().zip(&srgb.jpeg) {
        assert_eq!(u32::from(jpeg), (u32::from(png) + 128) / 257);
    }
    for i in 0..=1000 {
        let x = f64::from(i) / 1000.0;
        assert!((tone::srgb_decode(tone::srgb_encode(x)) - x).abs() < 3e-8);
    }
}

#[test]
fn function_order_and_scale_are_numerically_correct() {
    let image = image(vec![0, 1000, 2000, 3000, 4000], 5, 0.0, 4000);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, manual()).unwrap();
    let expression = Expression::parse("x/2").unwrap();
    let clipped = tone::render(
        &image,
        &hist,
        &range,
        false,
        Some(&expression),
        FunctionPolicy::Clip,
        png_transfer(Transfer::Linear),
    )
    .unwrap();
    assert_eq!(clipped.png, [0, 8192, 16384, 24576, 32768]);
    let scaled = tone::render(
        &image,
        &hist,
        &range,
        false,
        Some(&expression),
        FunctionPolicy::Scale,
        png_transfer(Transfer::Linear),
    )
    .unwrap();
    assert_eq!(scaled.png, [0, 16384, 32768, 49151, 65535]);
    let expression = Expression::parse("x^2").unwrap();
    let best = tone::render(
        &image,
        &hist,
        &range,
        true,
        Some(&expression),
        FunctionPolicy::Clip,
        png_transfer(Transfer::Linear),
    )
    .unwrap();
    for (i, &value) in best.png.iter().enumerate() {
        let x = i as f64 / 4.0;
        let y = best.tone.map(x);
        assert_eq!(value, (y * y * 65535.0).round() as u16);
    }
}

#[test]
fn every_png_jpeg_transfer_combination_matches_independent_quantization() {
    let image = image((0..=65535).collect(), 256, 0.0, 65535);
    let hist = Histogram::new(&image.pixels).unwrap();
    let range = range::analyze(&image, &hist, manual()).unwrap();
    for png in [Transfer::Linear, Transfer::Srgb] {
        for jpeg in [Transfer::Linear, Transfer::Srgb] {
            let result = tone::render(
                &image,
                &hist,
                &range,
                false,
                None,
                FunctionPolicy::Clip,
                tone::Transfers { png, jpeg },
            )
            .unwrap();
            for (i, (&p, &j)) in result.png.iter().zip(&result.jpeg).enumerate() {
                let x = i as f64 / 65535.0;
                let encoded = if png == Transfer::Linear {
                    x
                } else {
                    tone::srgb_encode(x)
                };
                assert_eq!(p, (encoded * 65535.0).round() as u16);
                let stored = f64::from(p) / 65535.0;
                let expected = if png == jpeg {
                    stored
                } else if png == Transfer::Linear {
                    tone::srgb_encode(stored)
                } else {
                    tone::srgb_decode(stored)
                };
                assert_eq!(j, (expected * 255.0).round() as u8);
            }
        }
    }
}

#[test]
fn parallelism_does_not_change_range_or_pixels() {
    let image = image(
        (0..1_048_576).map(|i| ((i * 137) % 65536) as u16).collect(),
        1024,
        0.0,
        65535,
    );
    let process = || {
        let hist = Histogram::new(&image.pixels).unwrap();
        let range = range::analyze(&image, &hist, RangeOptions::default()).unwrap();
        let result = tone::render(
            &image,
            &hist,
            &range,
            true,
            None,
            FunctionPolicy::Clip,
            png_transfer(Transfer::Srgb),
        )
        .unwrap();
        (
            serde_json::to_string(&range).unwrap(),
            result.png,
            result.jpeg,
        )
    };
    let one = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(process);
    let four = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap()
        .install(process);
    assert_eq!(one, four);
}
