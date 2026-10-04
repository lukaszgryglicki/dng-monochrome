#![allow(dead_code)]

use std::path::Path;

pub struct Dng {
    pub width: u32,
    pub height: u32,
    pub bits: u16,
    pub black: u32,
    pub white: u32,
    pub orientation: u16,
    pub crop: Option<[u32; 4]>,
    pub cfa: bool,
    pub big_endian: bool,
    pub pixels: Vec<u16>,
    pub iso: Option<u32>,
    pub noise_profile: Option<[f64; 2]>,
    pub exposure: [u32; 2],
    pub aperture: Option<[u32; 2]>,
    pub apex: Option<[u32; 2]>,
    pub lens_model: String,
}

impl Dng {
    pub fn ramp(width: u32, height: u32) -> Self {
        let n = width * height;
        Self {
            width,
            height,
            bits: 16,
            black: 1023,
            white: 16383,
            orientation: 1,
            crop: None,
            cfa: false,
            big_endian: false,
            iso: Some(125),
            noise_profile: None,
            exposure: [1, 125],
            aperture: Some([28, 10]),
            apex: None,
            lens_model: "Recorded manual M lens".into(),
            pixels: (0..n)
                .map(|i| (1023 + u64::from(i) * 15360 / u64::from(n - 1)) as u16)
                .collect(),
        }
    }

    pub fn write(&self, path: &Path) {
        let short = |v: u16| {
            if self.big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let long = |v: u32| {
            if self.big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let mut raster = Vec::new();
        for row in self.pixels.chunks_exact(self.width as usize) {
            if self.bits == 16 {
                for &v in row {
                    raster.extend(short(v));
                }
            } else {
                let mut byte = 0u8;
                let mut used = 0;
                for &v in row {
                    for bit in (0..self.bits).rev() {
                        byte = (byte << 1) | ((v >> bit) & 1) as u8;
                        used += 1;
                        if used == 8 {
                            raster.push(byte);
                            byte = 0;
                            used = 0;
                        }
                    }
                }
                if used > 0 {
                    raster.push(byte << (8 - used));
                }
            }
        }
        let mut tags: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (254, 4, 1, long(0)),
            (256, 4, 1, long(self.width)),
            (257, 4, 1, long(self.height)),
            (258, 3, 1, short(self.bits)),
            (259, 3, 1, short(1)),
            (262, 3, 1, short(if self.cfa { 32803 } else { 34892 })),
            (271, 2, 10, b"Synthetic\0".to_vec()),
            (272, 2, 5, b"Mono\0".to_vec()),
            (273, 4, 1, long(0)),
            (274, 3, 1, short(self.orientation)),
            (277, 3, 1, short(1)),
            (278, 4, 1, long(self.height)),
            (279, 4, 1, long(raster.len() as u32)),
            (284, 3, 1, short(1)),
            (50706, 1, 4, vec![1, 4, 0, 0]),
            (50714, 4, 1, long(self.black)),
            (50717, 4, 1, long(self.white)),
        ];
        if let Some([x, y, w, h]) = self.crop {
            tags.push((50719, 4, 2, [long(x), long(y)].concat()));
            tags.push((50720, 4, 2, [long(w), long(h)].concat()));
        }
        if self.cfa {
            tags.push((33421, 3, 2, [short(2), short(2)].concat()));
            tags.push((33422, 1, 4, vec![0, 1, 1, 2]));
        }
        let mut exif_tags: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (
                0x829a,
                5,
                1,
                [long(self.exposure[0]), long(self.exposure[1])].concat(),
            ),
            (0x9003, 2, 20, b"2026:10:03 12:34:56\0".to_vec()),
            (0x920a, 5, 1, [long(35), long(1)].concat()),
            (
                0xa434,
                2,
                (self.lens_model.len() + 1) as u32,
                [self.lens_model.as_bytes(), b"\0"].concat(),
            ),
        ];
        for (tag, value) in [(0x829d, self.aperture), (0x9202, self.apex)] {
            if let Some([n, d]) = value {
                exif_tags.push((tag, 5, 1, [long(n), long(d)].concat()));
            }
        }
        tags.push((34665, 4, 1, long(0)));
        if let Some(iso) = self.iso {
            exif_tags.extend([
                (0x8827, 3, 1, short(iso.min(65535) as u16)),
                (0x8831, 4, 1, long(iso)),
                (0x8833, 4, 1, long(iso)),
            ]);
        }
        if let Some(profile) = self.noise_profile {
            let payload = profile
                .into_iter()
                .flat_map(|v| {
                    if self.big_endian {
                        v.to_be_bytes()
                    } else {
                        v.to_le_bytes()
                    }
                })
                .collect();
            tags.push((51041, 12, 2, payload));
        }
        tags.sort_by_key(|v| v.0);
        let base = 8 + 2 + 12 * tags.len() + 4;
        let extra: usize = tags
            .iter()
            .filter(|v| v.3.len() > 4)
            .map(|v| v.3.len())
            .sum();
        exif_tags.sort_by_key(|entry| entry.0);
        let exif_payload = base + extra + 2 + 12 * exif_tags.len() + 4;
        let mut exif = short(exif_tags.len() as u16);
        let mut exif_tail = Vec::new();
        for (tag, typ, count, payload) in exif_tags {
            exif.extend(short(tag));
            exif.extend(short(typ));
            exif.extend(long(count));
            if payload.len() <= 4 {
                let mut inline = [0; 4];
                inline[..payload.len()].copy_from_slice(&payload);
                exif.extend(inline);
            } else {
                exif.extend(long((exif_payload + exif_tail.len()) as u32));
                exif_tail.extend(payload);
            }
        }
        exif.extend(long(0));
        exif.extend(exif_tail);
        tags.iter_mut().find(|v| v.0 == 273).unwrap().3 = long((base + extra + exif.len()) as u32);
        if let Some(tag) = tags.iter_mut().find(|v| v.0 == 34665) {
            tag.3 = long((base + extra) as u32);
        }
        let mut bytes = if self.big_endian {
            b"MM\0\x2a".to_vec()
        } else {
            b"II\x2a\0".to_vec()
        };
        bytes.extend(long(8));
        bytes.extend(short(tags.len() as u16));
        let mut tail = Vec::new();
        for (tag, typ, count, payload) in tags {
            bytes.extend(short(tag));
            bytes.extend(short(typ));
            bytes.extend(long(count));
            if payload.len() <= 4 {
                let mut inline = [0; 4];
                inline[..payload.len()].copy_from_slice(&payload);
                bytes.extend(inline);
            } else {
                bytes.extend(long((base + tail.len()) as u32));
                tail.extend(payload);
            }
        }
        bytes.extend(long(0));
        bytes.extend(tail);
        bytes.extend(exif);
        bytes.extend(raster);
        std::fs::write(path, bytes).unwrap();
    }
}
