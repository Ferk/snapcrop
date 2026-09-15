use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::geometry::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    Xrgb8888,
    Argb8888,
}

pub struct Pixels<'a> {
    pub bytes: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

pub fn encode_crop(source: Pixels<'_>, crop: Rect) -> Result<Vec<u8>, String> {
    if crop
        .x
        .checked_add(crop.width)
        .is_none_or(|x| x > source.width)
        || crop
            .y
            .checked_add(crop.height)
            .is_none_or(|y| y > source.height)
        || crop.width == 0
        || crop.height == 0
    {
        return Err("crop lies outside the captured image".into());
    }
    let required = usize::try_from(source.stride)
        .ok()
        .and_then(|stride| stride.checked_mul(source.height as usize))
        .ok_or("captured image is too large")?;
    if source.bytes.len() < required {
        return Err("captured pixel buffer is shorter than its stride and height".into());
    }

    let capacity = crop.width as usize * crop.height as usize * 4;
    let mut rgba = Vec::with_capacity(capacity);
    for y in crop.y..crop.y + crop.height {
        let row = y as usize * source.stride as usize;
        for x in crop.x..crop.x + crop.width {
            let offset = row + x as usize * 4;
            let b = source.bytes[offset];
            let g = source.bytes[offset + 1];
            let r = source.bytes[offset + 2];
            let a = match source.format {
                PixelFormat::Xrgb8888 => 255,
                PixelFormat::Argb8888 => source.bytes[offset + 3],
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, crop.width, crop.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("cannot start PNG encoding: {e}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| format!("cannot encode PNG: {e}"))?;
    }
    Ok(encoded)
}

pub fn save_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or("output path has no file name")?
        .to_string_lossy();
    let temporary: PathBuf = parent.join(format!(".{name}.{}.tmp", std::process::id()));

    let result = (|| -> io::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(bytes)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        std::fs::rename(&temporary, path)
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_bgra_memory_into_rgba_png() {
        let bytes = [
            0, 0, 255, 0, 0, 255, 0, 0, // red, green
            255, 0, 0, 0, 255, 255, 255, 0, // blue, white
        ];
        let encoded = encode_crop(
            Pixels {
                bytes: &bytes,
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Xrgb8888,
            },
            Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 2,
            },
        )
        .unwrap();

        let decoder = png::Decoder::new(std::io::Cursor::new(encoded));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(
            &decoded[..info.buffer_size()],
            &[0, 255, 0, 255, 255, 255, 255, 255]
        );
    }
}
