use crate::{
    formats::{Compression, Encoding, Format, quantize},
    raw::SensorMetadata,
    tone::Transfer,
};
use anyhow::{Context, Result, ensure};
use libheif_sys as heif;
use openjpeg_sys as jp2;
use rayon::prelude::*;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    mem::MaybeUninit,
    path::Path,
    ptr::{self, NonNull},
    slice,
    sync::{Mutex, PoisonError},
};
use tempfile::NamedTempFile;

pub(super) fn encode(
    parent: &Path,
    metadata: &SensorMetadata,
    pixels: &[u16],
    transfer: Transfer,
    exif: &[u8],
    encoding: &Encoding,
) -> Result<(NamedTempFile, Vec<String>)> {
    ensure!(
        metadata.width > 0
            && metadata.height > 0
            && metadata.width.checked_mul(metadata.height) == Some(pixels.len()),
        "native encoder dimensions do not match the developed image"
    );
    let valid_depth = match encoding.format {
        Format::Heic | Format::Avif => [8, 10, 12].contains(&encoding.bits),
        Format::J2k => (1..=16).contains(&encoding.bits),
        Format::Png | Format::Jpeg => false,
    };
    ensure!(
        valid_depth,
        "unsupported {} precision: {} bits",
        encoding.format,
        encoding.bits
    );
    ensure!(
        match encoding.compression {
            Compression::Lossless => encoding.quality.is_none(),
            Compression::Lossy => encoding
                .quality
                .is_some_and(|quality| (1..=100).contains(&quality)),
        },
        "invalid native compression/quality combination"
    );
    let mut file = super::temporary(parent)?;
    let warnings = match encoding.format {
        Format::Heic | Format::Avif => {
            encode_heif(
                file.as_file_mut(),
                metadata,
                pixels,
                transfer,
                exif,
                encoding,
            )?;
            Vec::new()
        }
        Format::J2k => {
            let warnings = encode_jp2(file.as_file_mut(), metadata, pixels, encoding)?;
            append_jp2_exif(file.as_file_mut(), exif)?;
            warnings
        }
        Format::Png | Format::Jpeg => unreachable!(),
    };
    file.as_file().sync_all()?;
    Ok((file, warnings))
}

struct Owned<T> {
    pointer: NonNull<T>,
    release: unsafe extern "C" fn(*mut T),
}

impl<T> Owned<T> {
    // The pointer must be uniquely owned and paired with its native destructor.
    unsafe fn new(
        pointer: *mut T,
        release: unsafe extern "C" fn(*mut T),
        name: &str,
    ) -> Result<Self> {
        Ok(Self {
            pointer: NonNull::new(pointer).with_context(|| format!("allocating {name}"))?,
            release,
        })
    }

    fn as_ptr(&self) -> *mut T {
        self.pointer.as_ptr()
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        unsafe { (self.release)(self.as_ptr()) };
    }
}

struct HeifInitialization;

impl HeifInitialization {
    fn new() -> Result<Self> {
        heif_result(unsafe { heif::heif_init(ptr::null_mut()) })?;
        Ok(Self)
    }
}

impl Drop for HeifInitialization {
    fn drop(&mut self) {
        unsafe { heif::heif_deinit() };
    }
}

fn heif_result(error: heif::heif_error) -> Result<()> {
    if error.code != heif::heif_error_code_heif_error_Ok {
        let message = if error.message.is_null() {
            "no native error message".into()
        } else {
            unsafe { CStr::from_ptr(error.message) }.to_string_lossy()
        };
        anyhow::bail!(
            "libheif: {message} (code {}, subcode {})",
            error.code,
            error.subcode
        );
    }
    Ok(())
}

unsafe extern "C" fn release_heif_image(image: *mut heif::heif_image) {
    unsafe { heif::heif_image_release(image) };
}

unsafe extern "C" fn release_heif_handle(handle: *mut heif::heif_image_handle) {
    unsafe { heif::heif_image_handle_release(handle) };
}

fn encode_heif(
    file: &mut File,
    metadata: &SensorMetadata,
    pixels: &[u16],
    transfer: Transfer,
    exif: &[u8],
    encoding: &Encoding,
) -> Result<()> {
    let _initialization = HeifInitialization::new()?;
    let width = i32::try_from(metadata.width).context("HEIF width exceeds its codec limit")?;
    let height = i32::try_from(metadata.height).context("HEIF height exceeds its codec limit")?;
    let (format, name) = match encoding.format {
        Format::Heic => (heif::heif_compression_format_heif_compression_HEVC, c"x265"),
        Format::Avif => (heif::heif_compression_format_heif_compression_AV1, c"aom"),
        _ => anyhow::bail!("expected HEIC or AVIF"),
    };
    unsafe {
        let context = Owned::new(
            heif::heif_context_alloc(),
            heif::heif_context_free,
            "HEIF context",
        )?;
        let mut descriptor = ptr::null();
        ensure!(
            heif::heif_get_encoder_descriptors(format, name.as_ptr(), &mut descriptor, 1) == 1
                && !descriptor.is_null(),
            "{} encoder {} is unavailable; install its codec library and rebuild",
            encoding.format,
            name.to_string_lossy()
        );
        let mut encoder = ptr::null_mut();
        heif_result(heif::heif_context_get_encoder(
            context.as_ptr(),
            descriptor,
            &mut encoder,
        ))?;
        let encoder = Owned::new(encoder, heif::heif_encoder_release, "HEIF encoder")?;
        heif_result(heif::heif_encoder_set_logging_level(encoder.as_ptr(), 0))?;
        if let Some(quality) = encoding.quality {
            heif_result(heif::heif_encoder_set_lossy_quality(
                encoder.as_ptr(),
                i32::from(quality),
            ))?;
        }
        heif_result(heif::heif_encoder_set_lossless(
            encoder.as_ptr(),
            i32::from(encoding.compression == Compression::Lossless),
        ))?;
        let threads = rayon::current_num_threads();
        if encoding.format == Format::Heic {
            for (key, value) in [(c"preset", c"placebo"), (c"x265:frame-threads", c"1")] {
                heif_result(heif::heif_encoder_set_parameter_string(
                    encoder.as_ptr(),
                    key.as_ptr(),
                    value.as_ptr(),
                ))?;
            }
            heif_result(heif::heif_encoder_set_parameter_integer(
                encoder.as_ptr(),
                c"complexity".as_ptr(),
                100,
            ))?;
            // libheif uses 16-pixel CTUs below 32 pixels; placebo's depth 4 is invalid there.
            let depth = if width.min(height) < 32 { 3 } else { 4 };
            heif_result(heif::heif_encoder_set_parameter_integer(
                encoder.as_ptr(),
                c"tu-intra-depth".as_ptr(),
                depth,
            ))?;
            if depth == 3 {
                heif_result(heif::heif_encoder_set_parameter_string(
                    encoder.as_ptr(),
                    c"x265:tu-inter-depth".as_ptr(),
                    c"3".as_ptr(),
                ))?;
            }
            // One frame worker plus the WPP pool must fit the per-file worker budget.
            let pools = CString::new(if threads == 1 {
                "none".into()
            } else {
                (threads - 1).to_string()
            })?;
            heif_result(heif::heif_encoder_set_parameter_string(
                encoder.as_ptr(),
                c"x265:pools".as_ptr(),
                pools.as_ptr(),
            ))?;
        } else {
            heif_result(heif::heif_encoder_set_parameter_integer(
                encoder.as_ptr(),
                c"speed".as_ptr(),
                0,
            ))?;
            heif_result(heif::heif_encoder_set_parameter_integer(
                encoder.as_ptr(),
                c"threads".as_ptr(),
                threads.min(64) as i32,
            ))?;
            heif_result(heif::heif_encoder_set_parameter_string(
                encoder.as_ptr(),
                c"tune".as_ptr(),
                c"ssim".as_ptr(),
            ))?;
        }
        let mut image = ptr::null_mut();
        heif_result(heif::heif_image_create(
            width,
            height,
            heif::heif_colorspace_heif_colorspace_monochrome,
            heif::heif_chroma_heif_chroma_monochrome,
            &mut image,
        ))?;
        let image = Owned::new(image, release_heif_image, "HEIF image")?;
        let channel = heif::heif_channel_heif_channel_Y;
        heif_result(heif::heif_image_add_plane(
            image.as_ptr(),
            channel,
            width,
            height,
            i32::from(encoding.bits),
        ))?;
        let mut stride = 0;
        let plane = heif::heif_image_get_plane(image.as_ptr(), channel, &mut stride);
        let stride = usize::try_from(stride).context("invalid HEIF plane stride")?;
        let sample_bytes = if encoding.bits > 8 { 2 } else { 1 };
        ensure!(
            !plane.is_null() && stride >= metadata.width * sample_bytes,
            "invalid HEIF output plane"
        );
        let length = stride
            .checked_mul(metadata.height)
            .context("HEIF plane size overflow")?;
        let plane = slice::from_raw_parts_mut(plane, length);
        plane
            .par_chunks_exact_mut(stride)
            .zip(pixels.par_chunks_exact(metadata.width))
            .for_each(|(row, source)| {
                row.fill(0);
                for (target, &sample) in row.chunks_exact_mut(sample_bytes).zip(source) {
                    let sample = quantize(sample, encoding.bits);
                    if sample_bytes == 1 {
                        target[0] = sample as u8;
                    } else {
                        target.copy_from_slice(&sample.to_ne_bytes());
                    }
                }
            });
        let profile = Owned::new(
            heif::heif_nclx_color_profile_alloc(),
            heif::heif_nclx_color_profile_free,
            "HEIF color profile",
        )?;
        let nclx = &mut *profile.as_ptr();
        nclx.color_primaries = heif::heif_color_primaries_heif_color_primaries_ITU_R_BT_709_5;
        nclx.transfer_characteristics = match transfer {
            Transfer::Linear => {
                heif::heif_transfer_characteristics_heif_transfer_characteristic_linear
            }
            Transfer::Srgb => {
                heif::heif_transfer_characteristics_heif_transfer_characteristic_IEC_61966_2_1
            }
        };
        nclx.matrix_coefficients =
            heif::heif_matrix_coefficients_heif_matrix_coefficients_ITU_R_BT_709_5;
        nclx.full_range_flag = 1;
        heif_result(heif::heif_image_set_nclx_color_profile(
            image.as_ptr(),
            profile.as_ptr(),
        ))?;
        let options = Owned::new(
            heif::heif_encoding_options_alloc(),
            heif::heif_encoding_options_free,
            "HEIF encoding options",
        )?;
        (*options.as_ptr()).output_nclx_profile = profile.as_ptr();
        (*options.as_ptr()).macOS_compatibility_workaround_no_nclx_profile = 0;
        let mut handle = ptr::null_mut();
        heif_result(heif::heif_context_encode_image(
            context.as_ptr(),
            image.as_ptr(),
            encoder.as_ptr(),
            options.as_ptr(),
            &mut handle,
        ))?;
        let handle = Owned::new(handle, release_heif_handle, "encoded HEIF image")?;
        heif_result(heif::heif_context_add_exif_metadata(
            context.as_ptr(),
            handle.as_ptr(),
            exif.as_ptr().cast(),
            i32::try_from(exif.len()).context("HEIF EXIF is too large")?,
        ))?;
        let mut output = OutputStream { file, error: None };
        let mut writer = heif::heif_writer {
            writer_api_version: 1,
            write: Some(heif_write),
        };
        let result = heif::heif_context_write(
            context.as_ptr(),
            &mut writer,
            (&mut output as *mut OutputStream<'_>).cast(),
        );
        output.finish()?;
        heif_result(result)?;
    }
    Ok(())
}

struct OutputStream<'a> {
    file: &'a mut File,
    error: Option<io::Error>,
}

impl OutputStream<'_> {
    fn perform<T>(&mut self, operation: impl FnOnce(&mut File) -> io::Result<T>) -> Option<T> {
        if self.error.is_some() {
            return None;
        }
        match operation(self.file) {
            Ok(value) => Some(value),
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(error) = self.error.take() {
            return Err(error).context("writing native image output");
        }
        Ok(())
    }
}

unsafe extern "C" fn heif_write(
    _context: *mut heif::heif_context,
    data: *const c_void,
    size: usize,
    userdata: *mut c_void,
) -> heif::heif_error {
    let output = unsafe { &mut *userdata.cast::<OutputStream<'_>>() };
    let bytes = if size == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data.cast::<u8>(), size) }
    };
    if output.perform(|file| file.write_all(bytes)).is_some() {
        heif::heif_error {
            code: heif::heif_error_code_heif_error_Ok,
            subcode: heif::heif_suberror_code_heif_suberror_Unspecified,
            message: c"Success".as_ptr(),
        }
    } else {
        heif::heif_error {
            code: heif::heif_error_code_heif_error_Encoding_error,
            subcode: heif::heif_suberror_code_heif_suberror_Unspecified,
            message: c"Output I/O error".as_ptr(),
        }
    }
}

unsafe extern "C" fn jp2_write(data: *mut c_void, size: usize, userdata: *mut c_void) -> usize {
    let output = unsafe { &mut *userdata.cast::<OutputStream<'_>>() };
    let bytes = if size == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data.cast::<u8>(), size) }
    };
    output
        .perform(|file| file.write_all(bytes))
        .map_or(usize::MAX, |()| size)
}

unsafe extern "C" fn jp2_skip(offset: i64, userdata: *mut c_void) -> i64 {
    let output = unsafe { &mut *userdata.cast::<OutputStream<'_>>() };
    output
        .perform(|file| file.seek(SeekFrom::Current(offset)))
        .map_or(-1, |_| offset)
}

unsafe extern "C" fn jp2_seek(position: i64, userdata: *mut c_void) -> i32 {
    let output = unsafe { &mut *userdata.cast::<OutputStream<'_>>() };
    i32::from(
        output
            .perform(|file| {
                let position = u64::try_from(position).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "negative codec seek")
                })?;
                file.seek(SeekFrom::Start(position))
            })
            .is_some(),
    )
}

#[derive(Default)]
struct Jp2Messages {
    errors: String,
    warnings: Vec<String>,
}

unsafe extern "C" fn jp2_error(message: *const c_char, userdata: *mut c_void) {
    let messages = unsafe { &*userdata.cast::<Mutex<Jp2Messages>>() };
    if !message.is_null() {
        let mut messages = messages.lock().unwrap_or_else(PoisonError::into_inner);
        messages
            .errors
            .push_str(&unsafe { CStr::from_ptr(message) }.to_string_lossy());
    }
}

unsafe extern "C" fn jp2_warning(message: *const c_char, userdata: *mut c_void) {
    let messages = unsafe { &*userdata.cast::<Mutex<Jp2Messages>>() };
    if !message.is_null() {
        let mut messages = messages.lock().unwrap_or_else(PoisonError::into_inner);
        messages.warnings.push(
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .trim()
                .to_owned(),
        );
    }
}

fn jp2_result(success: bool, operation: &str, messages: &Mutex<Jp2Messages>) -> Result<()> {
    let messages = messages
        .lock()
        .map_err(|_| anyhow::anyhow!("JPEG2000 diagnostic callback panicked"))?;
    ensure!(success, "{operation}: {}", messages.errors.trim());
    Ok(())
}

fn encode_jp2(
    file: &mut File,
    metadata: &SensorMetadata,
    pixels: &[u16],
    encoding: &Encoding,
) -> Result<Vec<String>> {
    let width = u32::try_from(metadata.width).context("JPEG2000 width exceeds its codec limit")?;
    let height =
        u32::try_from(metadata.height).context("JPEG2000 height exceeds its codec limit")?;
    let messages = Mutex::new(Jp2Messages::default());
    let mut output = OutputStream { file, error: None };
    unsafe {
        let mut component = jp2::opj_image_cmptparm_t {
            dx: 1,
            dy: 1,
            w: width,
            h: height,
            x0: 0,
            y0: 0,
            prec: u32::from(encoding.bits),
            bpp: u32::from(encoding.bits),
            sgnd: 0,
        };
        let image = Owned::new(
            jp2::opj_image_create(1, &mut component, jp2::COLOR_SPACE::OPJ_CLRSPC_GRAY),
            jp2::opj_image_destroy,
            "JPEG2000 image",
        )?;
        (*image.as_ptr()).x1 = width;
        (*image.as_ptr()).y1 = height;
        let components = (*image.as_ptr()).comps;
        ensure!(
            !components.is_null() && !(*components).data.is_null(),
            "missing JPEG2000 image plane"
        );
        slice::from_raw_parts_mut((*components).data, pixels.len())
            .par_iter_mut()
            .zip(pixels.par_iter())
            .for_each(|(target, &value)| *target = i32::from(quantize(value, encoding.bits)));

        let codec = Owned::new(
            jp2::opj_create_compress(jp2::CODEC_FORMAT::OPJ_CODEC_JP2),
            jp2::opj_destroy_codec,
            "JPEG2000 encoder",
        )?;
        let messages_pointer = ptr::from_ref(&messages).cast_mut().cast();
        ensure!(
            jp2::opj_set_error_handler(codec.as_ptr(), Some(jp2_error), messages_pointer) != 0
                && jp2::opj_set_warning_handler(
                    codec.as_ptr(),
                    Some(jp2_warning),
                    messages_pointer
                ) != 0,
            "cannot configure JPEG2000 diagnostics"
        );
        let mut parameters = MaybeUninit::uninit();
        jp2::opj_set_default_encoder_parameters(parameters.as_mut_ptr());
        let mut parameters = parameters.assume_init();
        parameters.cod_format = 1;
        parameters.cp_disto_alloc = 1;
        parameters.tcp_numlayers = 1;
        parameters.numresolution = (width.min(height).ilog2() + 1) as i32;
        parameters.irreversible = i32::from(encoding.compression == Compression::Lossy);
        // Match libheif/OpenJPEG quality semantics; zero rate is required for true lossless.
        parameters.tcp_rates[0] = encoding
            .quality
            .map_or(0.0, |quality| f32::from(1 + (100 - quality) / 2));
        jp2_result(
            jp2::opj_setup_encoder(codec.as_ptr(), &mut parameters, image.as_ptr()) != 0,
            "configuring JPEG2000 encoding",
            &messages,
        )?;
        jp2_result(
            jp2::opj_codec_set_threads(
                codec.as_ptr(),
                i32::try_from(rayon::current_num_threads())?,
            ) != 0,
            "configuring JPEG2000 worker budget",
            &messages,
        )?;
        let stream = Owned::new(
            jp2::opj_stream_default_create(0),
            jp2::opj_stream_destroy,
            "JPEG2000 stream",
        )?;
        jp2::opj_stream_set_write_function(stream.as_ptr(), Some(jp2_write));
        jp2::opj_stream_set_skip_function(stream.as_ptr(), Some(jp2_skip));
        jp2::opj_stream_set_seek_function(stream.as_ptr(), Some(jp2_seek));
        jp2::opj_stream_set_user_data(
            stream.as_ptr(),
            (&mut output as *mut OutputStream<'_>).cast(),
            None,
        );
        let success = jp2::opj_start_compress(codec.as_ptr(), image.as_ptr(), stream.as_ptr()) != 0
            && jp2::opj_encode(codec.as_ptr(), stream.as_ptr()) != 0
            && jp2::opj_end_compress(codec.as_ptr(), stream.as_ptr()) != 0;
        drop(stream);
        drop(codec);
        output.finish()?;
        jp2_result(success, "encoding JPEG2000", &messages)?;
    }
    Ok(messages
        .into_inner()
        .map_err(|_| anyhow::anyhow!("JPEG2000 diagnostic callback panicked"))?
        .warnings)
}

fn append_jp2_exif(file: &mut File, exif: &[u8]) -> Result<()> {
    let size = file.metadata()?.len();
    ensure!(
        size >= 12,
        "encoder produced an empty or truncated JP2 container"
    );
    let mut position = 0u64;
    while position < size {
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let mut length = u64::from(u32::from_be_bytes(header[..4].try_into()?));
        let open_ended = length == 0;
        let minimum = if length == 1 {
            let mut extended = [0; 8];
            file.read_exact(&mut extended)?;
            length = u64::from_be_bytes(extended);
            16
        } else {
            8
        };
        if open_ended {
            length = size - position;
            let length =
                u32::try_from(length).context("open-ended JP2 box is too large to attach EXIF")?;
            file.seek(SeekFrom::Start(position))?;
            file.write_all(&length.to_be_bytes())?;
        }
        ensure!(
            length >= minimum && length <= size - position,
            "encoder produced an invalid JP2 box"
        );
        position += length;
    }
    let length = u32::try_from(
        exif.len()
            .checked_add(24)
            .context("JP2 EXIF size overflow")?,
    )?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&length.to_be_bytes())?;
    file.write_all(b"uuidJpgTiffExif->JP2")?;
    file.write_all(exif)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_writers_append_chunks_and_support_jpeg2000_header_seeks() {
        let mut file = tempfile::tempfile().unwrap();
        {
            let mut output = OutputStream {
                file: &mut file,
                error: None,
            };
            let user = ptr::from_mut(&mut output).cast();
            unsafe {
                for bytes in [b"abc", b"def"] {
                    let status =
                        heif_write(ptr::null_mut(), bytes.as_ptr().cast(), bytes.len(), user);
                    assert_eq!(status.code, heif::heif_error_code_heif_error_Ok);
                }
                assert_eq!(jp2_seek(0, user), 1);
                assert_eq!(jp2_write(b"xy".as_ptr().cast_mut().cast(), 2, user), 2);
                assert_eq!(jp2_skip(4, user), 4);
                assert_eq!(jp2_write(b"!".as_ptr().cast_mut().cast(), 1, user), 1);
            }
            output.finish().unwrap();
        }
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"xycdef!");
    }

    #[test]
    fn native_write_errors_reach_the_rust_caller() {
        let temporary = tempfile::NamedTempFile::new().unwrap();
        for heif_writer in [true, false] {
            let mut file = File::open(temporary.path()).unwrap();
            let mut output = OutputStream {
                file: &mut file,
                error: None,
            };
            let user = ptr::from_mut(&mut output).cast();
            unsafe {
                if heif_writer {
                    assert_ne!(
                        heif_write(ptr::null_mut(), b"x".as_ptr().cast(), 1, user).code,
                        heif::heif_error_code_heif_error_Ok,
                    );
                } else {
                    assert_eq!(
                        jp2_write(b"x".as_ptr().cast_mut().cast(), 1, user),
                        usize::MAX
                    );
                }
            }
            assert!(
                output
                    .finish()
                    .unwrap_err()
                    .to_string()
                    .contains("writing native image")
            );
        }
    }
}
