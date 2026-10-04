use std::{fs::File, io::BufReader, path::PathBuf};

#[test]
#[ignore = "set DNG_MONO_OUTPUTS to a generated --both --report output tree"]
fn generated_collection_decodes_with_matching_pixels_and_reports() {
    let root = PathBuf::from(std::env::var_os("DNG_MONO_OUTPUTS").expect("set DNG_MONO_OUTPUTS"));
    let mut reports = Vec::new();
    for entry in walkdir::WalkDir::new(&root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|s| s == "json") {
            reports.push(entry.into_path());
        }
    }
    reports.sort();
    assert!(!reports.is_empty(), "no JSON reports");
    let mut summaries = Vec::new();
    for path in &reports {
        let report: serde_json::Value = serde_json::from_reader(File::open(path).unwrap()).unwrap();
        let png_transfer = report
            .get("png_transfer")
            .unwrap_or(&report["transfer"])
            .as_str()
            .unwrap();
        // Older reports predate independent JPEG transfer and always used sRGB.
        let jpeg_transfer = report
            .get("jpeg_transfer")
            .map_or("srgb", |v| v.as_str().unwrap());
        let mut png = png::Decoder::new(BufReader::new(
            File::open(path.with_extension("png")).unwrap(),
        ))
        .read_info()
        .unwrap();
        assert_eq!(png.info().bit_depth, png::BitDepth::Sixteen);
        assert_eq!(png.info().color_type, png::ColorType::Grayscale);
        assert!(png.info().exif_metadata.is_some());
        if png_transfer == "linear" {
            assert_eq!(png.info().gamma().unwrap().into_value(), 1.0);
        }
        let mut data = vec![0; png.output_buffer_size().unwrap()];
        let frame = png.next_frame(&mut data).unwrap();
        assert_eq!(
            u64::from(frame.width),
            report["metadata"]["width"].as_u64().unwrap()
        );
        assert_eq!(
            u64::from(frame.height),
            report["metadata"]["height"].as_u64().unwrap()
        );
        let mut jpg = jpeg_decoder::Decoder::new(BufReader::new(
            File::open(path.with_extension("jpg")).unwrap(),
        ));
        let jpeg = jpg.decode().unwrap();
        let info = jpg.info().unwrap();
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::L8);
        assert_eq!(
            (u32::from(info.width), u32::from(info.height)),
            (frame.width, frame.height)
        );
        assert!(jpg.exif_data().is_some());
        assert_eq!(jpeg.len() * 2, frame.buffer_size());
        let table: Vec<u8> = (0..=65535)
            .map(|v| {
                let x = f64::from(v) / 65535.0;
                let y = match (png_transfer, jpeg_transfer) {
                    ("linear", "linear") | ("srgb", "srgb") => x,
                    ("linear", "srgb") => dng_monochrome::tone::srgb_encode(x),
                    ("srgb", "linear") => dng_monochrome::tone::srgb_decode(x),
                    _ => panic!("invalid transfer in {}", path.display()),
                };
                (y * 255.0).round() as u8
            })
            .collect();
        let mut used = vec![false; 65536];
        let (mut min, mut max, mut error) = (u16::MAX, 0, 0u64);
        for (bytes, &j) in data[..frame.buffer_size()].chunks_exact(2).zip(&jpeg) {
            let p = u16::from_be_bytes([bytes[0], bytes[1]]);
            used[usize::from(p)] = true;
            min = min.min(p);
            max = max.max(p);
            error += u64::from(table[usize::from(p)].abs_diff(j));
        }
        let occupied = used.iter().filter(|v| **v).count();
        assert_eq!(
            u64::from(min),
            report["output"]["png_min"].as_u64().unwrap()
        );
        assert_eq!(
            u64::from(max),
            report["output"]["png_max"].as_u64().unwrap()
        );
        assert_eq!(
            occupied as u64,
            report["output"]["png_occupied_codes"].as_u64().unwrap()
        );
        let mae = error as f64 / jpeg.len() as f64;
        assert!(mae < 8.0, "JPEG mismatch {}: MAE={mae}", path.display());
        summaries.push(serde_json::json!({
            "report": path, "width": frame.width, "height": frame.height,
            "png_min": min, "png_max": max, "png_occupied_codes": occupied,
            "jpeg_mean_absolute_error": mae,
        }));
    }
    if let Some(path) = std::env::var_os("DNG_MONO_VERIFY_REPORT") {
        serde_json::to_writer_pretty(File::create(path).unwrap(), &summaries).unwrap();
    }
    eprintln!(
        "Decoded {} full PNG16/JPEG pairs and verified geometry, EXIF, exact PNG statistics and JPEG approximation.",
        reports.len()
    );
}
