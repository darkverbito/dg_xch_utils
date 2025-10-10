use lifehash_lib::Version::Version2;
use lifehash_lib::lifehash;
use petname::{Generator, Petnames};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use sha2::{Digest, Sha256};
use shared_types::RgbaImage;

pub fn petname(data: &[u8]) -> Result<String, String> {
    let sha256: [u8; 32] = Sha256::digest(data).into();
    Petnames::medium()
        .generate(&mut ChaCha20Rng::from_seed(sha256), 2, "-")
        .ok_or("Failed to generate petname".to_string())
}

pub fn lifehash(data: &'_ [u8]) -> Result<RgbaImage, String> {
    let image = lifehash::from_data(data, Version2, 1, true)
        .map(|v| v.0)
        .map_err(|e| e.to_string())?;
    let tauri_image = if image.channels < 4 {
        let pixel_count = image.width * image.height;
        let mut rgba_pixels = Vec::with_capacity(pixel_count * 4);
        match image.channels {
            1 => {
                for &gray_value in &image.pixels {
                    rgba_pixels.push(gray_value); // Red = Gray
                    rgba_pixels.push(gray_value); // Green = Gray
                    rgba_pixels.push(gray_value); // Blue = Gray
                    rgba_pixels.push(255); // Alpha = Full opacity
                }
            }
            2 => {
                for ga_chunk in image.pixels.chunks_exact(2) {
                    let gray_value = ga_chunk[0];
                    let alpha_value = ga_chunk[1];
                    rgba_pixels.push(gray_value); // Red = Gray
                    rgba_pixels.push(gray_value); // Green = Gray
                    rgba_pixels.push(gray_value); // Blue = Gray
                    rgba_pixels.push(alpha_value); // Alpha = Original alpha
                }
            }
            3 => {
                for rgb_chunk in image.pixels.chunks_exact(3) {
                    rgba_pixels.push(rgb_chunk[0]); // Red
                    rgba_pixels.push(rgb_chunk[1]); // Green
                    rgba_pixels.push(rgb_chunk[2]); // Blue
                    rgba_pixels.push(255); // Alpha = Full opacity
                }
            }
            _ => unreachable!(),
        }
        RgbaImage {
            data: rgba_pixels,
            width: image.width as u32,
            height: image.height as u32,
        }
    } else {
        RgbaImage {
            data: image.pixels,
            width: image.width as u32,
            height: image.height as u32,
        }
    };
    Ok(tauri_image)
}
