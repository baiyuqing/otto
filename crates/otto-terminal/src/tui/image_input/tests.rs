use super::*;
use image::{DynamicImage, Rgb, RgbImage};

fn fixture(format: ImageFormat) -> Vec<u8> {
    let pixels = RgbImage::from_pixel(2, 3, Rgb([40, 80, 120]));
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut bytes, format)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn explicit_png_and_jpeg_are_valid_inline_blocks() {
    for (format, mime) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
    ] {
        let bytes = fixture(format);
        let dir = tempfile::tempdir().unwrap();
        // Detect by bytes, not a misleading extension.
        let path = dir.path().join("synthetic.data");
        std::fs::write(&path, &bytes).unwrap();
        let block = file(&path, |_| true).unwrap();
        assert_eq!(block.mime_type, mime);
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&block.data)
                .unwrap(),
            bytes
        );
        block.validate().unwrap();
    }
}

#[test]
fn rejects_empty_text_and_truncated_images() {
    assert!(image_block(b"", false).is_err());
    assert!(image_block(b"/synthetic/image.png", false).is_err());
    let mut bytes = fixture(ImageFormat::Png);
    bytes.truncate(40);
    assert!(image_block(&bytes, false).is_err());
    let mut bytes = fixture(ImageFormat::Jpeg);
    bytes.truncate(bytes.len() / 2);
    assert!(image_block(&bytes, false).is_err());
}

#[test]
fn rejects_unsupported_magic_without_reading_paths() {
    assert!(
        image_block(b"GIF89a", false)
            .unwrap_err()
            .contains("Unsupported")
    );
}

#[test]
fn denied_path_is_rejected_before_open() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.png");
    let error = file(&missing, |absolute| {
        assert!(absolute.is_absolute());
        assert_eq!(absolute, missing);
        false
    })
    .unwrap_err();
    assert!(error.contains("session policy"));
}

#[test]
fn errors_omit_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-private-name.png");
    let error = file(&path, |_| true).unwrap_err();
    assert!(error.contains("not found"));
    assert!(!error.contains("synthetic-private-name"));
    assert!(!error.contains(&*dir.path().to_string_lossy()));
    assert!(file(dir.path(), |_| true).unwrap_err().contains("regular"));
}

#[test]
fn rejects_oversized_files_before_reading() {
    let input = tempfile::NamedTempFile::new().unwrap();
    input.as_file().set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
    assert!(file(input.path(), |_| true).unwrap_err().contains("5 MiB"));
}

#[test]
fn rejects_excessive_dimensions_and_pixel_count() {
    for (width, height) in [(MAX_EDGE + 1, 1), (4097, 4096)] {
        let pixels = RgbImage::new(width, height);
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        assert!(image_block(bytes.get_ref(), false).is_err());
    }
}

#[cfg(unix)]
#[test]
fn rejects_named_pipe_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-pipe");
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a valid nul-terminated path in our synthetic temp dir.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(file(&path, |_| true).unwrap_err().contains("regular"));
}

#[cfg(target_os = "macos")]
#[test]
fn tiff_is_only_accepted_for_clipboard_and_becomes_png() {
    let bytes = fixture(ImageFormat::Tiff);
    assert!(image_block(&bytes, false).is_err());
    let block = image_block(&bytes, true).unwrap();
    assert_eq!(block.mime_type, "image/png");
    let png = base64::engine::general_purpose::STANDARD
        .decode(block.data)
        .unwrap();
    assert_eq!(image::guess_format(&png).unwrap(), ImageFormat::Png);
    assert_eq!(image::load_from_memory(&png).unwrap().width(), 2);
}

#[cfg(target_os = "macos")]
#[test]
fn png_output_writer_refuses_to_grow_past_limit() {
    use std::io::Write as _;

    let mut writer = BoundedPng(vec![0; MAX_IMAGE_BYTES - 1]);
    assert!(writer.write_all(&[1, 2]).is_err());
    assert_eq!(writer.0.len(), MAX_IMAGE_BYTES - 1);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_clipboard_is_actionable_without_accessing_it() {
    assert!(clipboard().unwrap_err().contains("only on macOS"));
}
