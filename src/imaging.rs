//! Turning arbitrary uploaded bytes into RGBA pixels, and back into PNG.

use std::fmt;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use image::{DynamicImage, ImageFormat, RgbaImage};

use crate::clipboard::Image;

#[derive(Debug)]
pub struct ImageError(String);

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Cheap sniff used to tell a file upload apart from a text upload.
pub fn looks_like_image(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok() || is_heif(bytes)
}

/// Decodes into RGBA8. Blocking: run it on a blocking thread.
pub fn decode(bytes: &[u8]) -> Result<Image, ImageError> {
    let decoded = match image::load_from_memory(bytes) {
        Ok(image) => image,
        // The `image` crate has no HEIC/HEIF decoder, which is exactly what an
        // iPhone hands to the share sheet. `sips` ships with macOS and reads
        // every format the OS knows, so let it transcode to PNG for us.
        Err(native_err) => transcode_with_sips(bytes).map_err(|sips_err| {
            ImageError(format!(
                "{native_err}; sips fallback also failed: {sips_err}"
            ))
        })?,
    };

    let rgba = decoded.to_rgba8();
    Ok(Image {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
    })
}

/// Encodes clipboard pixels as PNG. Blocking: run it on a blocking thread.
pub fn encode_png(image: &Image) -> Result<Vec<u8>, ImageError> {
    let width = u32::try_from(image.width).map_err(|_| ImageError("image too wide".into()))?;
    let height = u32::try_from(image.height).map_err(|_| ImageError("image too tall".into()))?;
    let buffer = RgbaImage::from_raw(width, height, image.rgba.clone())
        .ok_or_else(|| ImageError("clipboard pixel buffer does not match its dimensions".into()))?;

    let mut png = Vec::new();
    buffer
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| ImageError(e.to_string()))?;
    Ok(png)
}

/// ISO base media files tag their brand at offset 4; `heic`/`mif1` and friends
/// all start with an `ftyp` box.
fn is_heif(bytes: &[u8]) -> bool {
    bytes.len() > 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"mif1" | b"msf1" | b"avif"
        )
}

fn transcode_with_sips(bytes: &[u8]) -> Result<DynamicImage, ImageError> {
    let scratch = Scratch::new()?;
    std::fs::write(&scratch.input, bytes).map_err(io_err)?;

    let output = Command::new("/usr/bin/sips")
        .arg("-s")
        .arg("format")
        .arg("png")
        .arg(&scratch.input)
        .arg("--out")
        .arg(&scratch.output)
        .output()
        .map_err(io_err)?;

    if !output.status.success() {
        return Err(ImageError(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let png = std::fs::read(&scratch.output).map_err(io_err)?;
    image::load_from_memory_with_format(&png, ImageFormat::Png)
        .map_err(|e| ImageError(e.to_string()))
}

/// A pair of temp paths that clean themselves up.
struct Scratch {
    dir: PathBuf,
    input: PathBuf,
    output: PathBuf,
}

impl Scratch {
    fn new() -> Result<Self, ImageError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("clipd-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        Ok(Self {
            input: dir.join("in"),
            output: dir.join("out.png"),
            dir,
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn io_err(err: std::io::Error) -> ImageError {
    ImageError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> Vec<u8> {
        let image = RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn png_round_trips_through_rgba() {
        let decoded = decode(&sample_png()).unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 3));
        assert_eq!(decoded.rgba.len(), 4 * 3 * 4);

        let reencoded = encode_png(&decoded).unwrap();
        let again = decode(&reencoded).unwrap();
        assert_eq!(again.rgba, decoded.rgba);
    }

    #[test]
    fn heif_magic_is_recognised() {
        let mut heic = vec![0, 0, 0, 24];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(b"\0\0\0\0mif1heic");
        assert!(is_heif(&heic));
        assert!(looks_like_image(&heic));

        assert!(!is_heif(b"plain text, not a file"));
        assert!(!looks_like_image(b"plain text, not a file"));
        assert!(looks_like_image(&sample_png()));
    }

    #[test]
    fn mismatched_pixel_buffer_is_an_error() {
        let broken = Image {
            width: 100,
            height: 100,
            rgba: vec![0; 16],
        };
        assert!(encode_png(&broken).is_err());
    }

    #[test]
    fn undecodable_bytes_report_both_failures() {
        let Err(err) = decode(b"definitely not an image") else {
            panic!("expected a decode failure");
        };
        let message = err.to_string();
        assert!(
            message.contains("sips fallback also failed"),
            "got: {message}"
        );
    }
}
