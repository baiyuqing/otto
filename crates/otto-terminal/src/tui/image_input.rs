//! Explicit local image acquisition. Nothing reads the pasteboard or filesystem
//! until the frontend calls one of these functions after a user action.

use std::io::{Cursor, Read};
use std::path::Path;

use base64::Engine as _;
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use otto_core::model::Block;

const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
#[cfg(target_os = "macos")]
const MAX_CLIPBOARD_BYTES: usize = 20 * 1024 * 1024;
const MAX_EDGE: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const TOO_LARGE: &str = "Image exceeds the attachment limits (5 MiB encoded, 8192 pixels per edge, 16 megapixels). Resize or export a smaller PNG/JPEG and try again.";
const INVALID_IMAGE: &str = "Image data is damaged or cannot be decoded within the safety limits. Export a new PNG/JPEG and try again.";

/// Reads only native image data after an explicit local paste-image action.
/// Never interprets clipboard text or file URLs as paths, and never uploads.
#[cfg(target_os = "macos")]
pub(super) fn clipboard() -> Result<Block, String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF};

    let pasteboard = NSPasteboard::generalPasteboard();
    let revision = pasteboard.changeCount();
    // SAFETY: These are immutable, framework-owned NSString constants. We only
    // request image representations, never file objects or text/path fallbacks.
    #[allow(unsafe_code)]
    let types = unsafe { [NSPasteboardTypePNG, NSPasteboardTypeTIFF] };
    for kind in types {
        if let Some(data) = pasteboard.dataForType(kind) {
            if data.length() > MAX_CLIPBOARD_BYTES {
                return Err("Clipboard image exceeds 20 MiB. Export a smaller PNG/JPEG and attach its explicit file path instead.".into());
            }
            let bytes = data.to_vec();
            if pasteboard.changeCount() != revision {
                return Err("Clipboard changed while reading the image. Copy it again and retry the paste-image action.".into());
            }
            return image_block(&bytes, true);
        }
    }
    Err("Clipboard has no supported image data. Copy the image itself (not a file name or path), or attach an explicit PNG/JPEG file path.".into())
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clipboard() -> Result<Block, String> {
    Err("Clipboard image input is supported only on macOS. Attach an explicit PNG/JPEG file path instead.".into())
}

/// Opens exactly the user-selected path, pins that descriptor for the bounded
/// read, and returns an inline block. Errors deliberately omit paths/OS details.
pub(super) fn file(path: &Path, authorize: impl Fn(&Path) -> bool) -> Result<Block, String> {
    let absolute = checked_absolute(path)?;
    if !authorize(&absolute) {
        return Err("Image path is not readable under the current session policy. Choose an allowed PNG/JPEG file; no extra access is requested.".into());
    }
    let input = open_pinned(&absolute).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "Image file was not found. Check the explicit path and try again.".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Image file is not readable. Choose a readable PNG/JPEG file; no additional privileges will be requested.".to_owned(),
        _ => "Could not open the image file. Choose a readable, regular PNG/JPEG file and try again.".to_owned(),
    })?;
    let metadata = input.metadata().map_err(
        |_| "Could not inspect the image file. Choose a readable PNG/JPEG file and try again.",
    )?;
    if !metadata.is_file() {
        return Err(
            "Image input must be a regular PNG/JPEG file, not a directory, device, or pipe.".into(),
        );
    }
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(TOO_LARGE.into());
    }
    let mut bytes = Vec::new();
    input
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(
            |_| "Could not read the image file. Choose a readable PNG/JPEG file and try again.",
        )?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(TOO_LARGE.into());
    }
    image_block(&bytes, false)
}

// This local user-selected path is not a model file-tool request, so it is not
// workspace-confined. Preserve the shared built-in private-path denylist. The
// frontend supplies the session policy callback. It sees the absolute direct
// path before any open; no read exceptions are granted here. Since every
// component is opened without following symlinks, resolved aliases cannot
// bypass this callback.
fn checked_absolute(path: &Path) -> Result<std::path::PathBuf, String> {
    let absolute = std::path::absolute(path)
        .map_err(|_| "Could not resolve the image path. Choose an absolute PNG/JPEG path.")?;
    if absolute
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Image paths containing parent traversal are unsupported. Choose a direct absolute PNG/JPEG path.".into());
    }
    let home = std::env::var_os("HOME").ok_or(
        "Cannot check private image path protections without HOME. Configure HOME and retry.",
    )?;
    if otto::sandbox::read_denylist::ReadDenylist::new(home, Vec::new()).covers(&absolute) {
        return Err("Image path is protected by the private-file read policy. Choose a PNG/JPEG outside protected directories; no extra access is requested.".into());
    }
    Ok(absolute)
}

fn open_pinned(path: &Path) -> std::io::Result<std::fs::File> {
    use otto_platform::fsops::{open_at_no_follow, open_dir_at_no_follow, open_dir_no_follow};
    use std::path::Component;

    // Walk from a pinned root; never follow a symlink in any component. The
    // helpers supply O_CLOEXEC and O_NOFOLLOW. O_NONBLOCK rejects pipes without
    // blocking before the regular-file metadata check.
    let mut directory = open_dir_no_follow(Path::new("/"))?;
    let mut parts = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .peekable();
    while let Some(part) = parts.next() {
        let name = part
            .to_str()
            .ok_or_else(|| std::io::Error::other("unsupported path encoding"))?;
        if parts.peek().is_none() {
            return open_at_no_follow(&directory, name, libc::O_RDONLY | libc::O_NONBLOCK);
        }
        directory = open_dir_at_no_follow(&directory, name)?;
    }
    Err(std::io::Error::other("image path must name a file"))
}

fn image_block(bytes: &[u8], allow_tiff: bool) -> Result<Block, String> {
    let format = image::guess_format(bytes).map_err(|_| "Image input must contain PNG or JPEG data. Export the image as PNG/JPEG and try again.")?;
    let mime_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        #[cfg(target_os = "macos")]
        ImageFormat::Tiff if allow_tiff => "image/png",
        _ => {
            return Err(
                "Unsupported image format. Export the image as PNG/JPEG and try again.".into(),
            );
        }
    };
    // Keep the helper portable without enabling TIFF on other targets.
    let _ = allow_tiff;
    if format != ImageFormat::Tiff && bytes.len() > MAX_IMAGE_BYTES {
        return Err(TOO_LARGE.into());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|_| INVALID_IMAGE)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(INVALID_IMAGE.into());
    }
    if width > MAX_EDGE
        || height > MAX_EDGE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(TOO_LARGE.into());
    }
    // Validate the complete image rather than trusting its header/signature.
    let decoded = image::DynamicImage::from_decoder(decoder).map_err(|_| INVALID_IMAGE)?;
    #[cfg(target_os = "macos")]
    if format == ImageFormat::Tiff {
        return encode_clipboard_png(decoded);
    }
    drop(decoded);
    Ok(Block::image(
        base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type,
    ))
}

#[cfg(target_os = "macos")]
fn encode_clipboard_png(decoded: image::DynamicImage) -> Result<Block, String> {
    use image::ImageEncoder as _;

    let rgba = decoded.to_rgba8();
    let mut output = BoundedPng(Vec::new());
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        .map_err(|_| "Clipboard image could not be encoded within 5 MiB. Resize it or export a smaller PNG/JPEG and try again.")?;
    Ok(Block::image(
        base64::engine::general_purpose::STANDARD.encode(output.0),
        "image/png",
    ))
}

#[cfg(target_os = "macos")]
struct BoundedPng(Vec<u8>);

#[cfg(target_os = "macos")]
impl std::io::Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("encoded image limit exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
