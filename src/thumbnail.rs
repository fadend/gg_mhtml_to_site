use image;
use image::imageops;

use std::io::Cursor;

const THUMBNAIL_HEIGHT: u32 = 150;

pub fn create_thumbnail(contents: &[u8], thumbnail_path: &std::path::PathBuf) {
    let reader = image::ImageReader::new(Cursor::new(contents))
        .with_guessed_format()
        .unwrap();
    let image = reader.decode().unwrap();
    let original_height = image.height();
    let original_width = image.width();
    let width =
        ((original_width as f32) / (original_height as f32) * THUMBNAIL_HEIGHT as f32) as u32;
    let thumbnail = imageops::thumbnail(&image, width, THUMBNAIL_HEIGHT);
    image::DynamicImage::ImageRgba8(thumbnail)
        .into_rgb8()
        .save(thumbnail_path)
        .expect("Failed to save");
}
