//! Keep full-resolution wallpapers out of Iced's decoded-image and texture caches.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use cosmic::iced::Length;
use cosmic::{widget, Element};
use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::i18n::tr;

// The largest preview is 96 logical pixels wide; allow a 2x display scale.
const MAX_EDGE: u32 = 192;

pub fn view<'a, Message: 'a>(
    source: &Path,
    width: impl Into<Length>,
    height: impl Into<Length>,
) -> Element<'a, Message> {
    let result = dirs::cache_dir()
        .ok_or_else(|| std::io::Error::other("user cache directory is unavailable").into())
        .and_then(|root| cached_path(source, &root.join("cosmic-dynamic-wallpaper/thumbnails-v1")));
    match result {
        Ok(path) => widget::image(path).width(width).height(height).into(),
        Err(error) => {
            tracing::warn!(path = %source.display(), %error, "could not load wallpaper thumbnail");
            widget::text::caption(tr("(no preview)")).into()
        }
    }
}

fn cached_path(source: &Path, cache_dir: &Path) -> image::ImageResult<PathBuf> {
    let source = source.canonicalize()?;
    let metadata = source.metadata()?;
    let mut key = DefaultHasher::new();
    source.hash(&mut key);
    metadata.len().hash(&mut key);
    metadata.modified()?.hash(&mut key);
    let path = cache_dir.join(format!("{:016x}.png", key.finish()));
    if path.try_exists()? {
        return Ok(path);
    }

    // Decode one original at a time, then drop it before Iced sees the image.
    let mut decoder = ImageReader::open(source)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let image = DynamicImage::from_decoder(decoder)?;
    let mut thumbnail = if image.width() > MAX_EDGE || image.height() > MAX_EDGE {
        image.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        image
    };
    thumbnail.apply_orientation(orientation);
    let thumbnail = thumbnail.into_rgba8();

    std::fs::create_dir_all(cache_dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(cache_dir)?;
    thumbnail.write_to(temporary.as_file_mut(), image::ImageFormat::Png)?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twenty_four_wallpapers_use_less_than_four_mib_of_preview_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let original = dir.path().join("original.jpg");
        image::RgbImage::new(3840, 2160).save(&original).unwrap();
        let mut bytes = 0;
        for index in 0..24 {
            let source = dir.path().join(format!("{index}.jpg"));
            std::fs::copy(&original, &source).unwrap();
            let path = cached_path(&source, &cache).unwrap();
            assert_ne!(path, source);
            let preview = image::open(&path).unwrap().into_rgba8();
            assert_eq!(preview.dimensions(), (192, 108));
            bytes += preview.as_raw().len();
            assert_eq!(cached_path(&source, &cache).unwrap(), path);
        }
        assert!(bytes < 4 * 1024 * 1024, "{bytes} bytes of preview pixels");
        assert_eq!(image::image_dimensions(&original).unwrap(), (3840, 2160));
    }

    #[test]
    fn small_images_are_not_upscaled_and_source_changes_invalidate_cache() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("image.png");
        let cache = dir.path().join("cache");
        image::RgbaImage::new(12, 24).save(&source).unwrap();
        let first = cached_path(&source, &cache).unwrap();
        assert_eq!(image::image_dimensions(&first).unwrap(), (12, 24));

        image::RgbaImage::new(600, 1200).save(&source).unwrap();
        let second = cached_path(&source, &cache).unwrap();
        assert_ne!(first, second);
        assert_eq!(image::image_dimensions(&second).unwrap(), (96, 192));
    }

    #[test]
    fn invalid_or_missing_images_do_not_fall_back_to_originals() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("invalid.jpg");
        let cache = dir.path().join("cache");
        assert!(cached_path(&source, &cache).is_err());
        std::fs::write(&source, b"not an image").unwrap();
        assert!(cached_path(&source, &cache).is_err());
        assert!(!cache.exists());
    }

    #[test]
    fn jpeg_orientation_is_applied_before_caching() {
        use image::ImageEncoder;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("rotated.jpg");
        let file = std::fs::File::create(&source).unwrap();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(file);
        // TIFF IFD with Orientation=6 (rotate 90 degrees clockwise).
        encoder
            .set_exif_metadata(
                b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec(),
            )
            .unwrap();
        encoder
            .encode_image(&image::RgbImage::new(600, 300))
            .unwrap();
        drop(encoder);

        let path = cached_path(&source, &dir.path().join("cache")).unwrap();
        assert_eq!(image::image_dimensions(&path).unwrap(), (96, 192));
    }
}
