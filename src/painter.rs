//! Inspired by https://github.com/Starlwr/StarBot/blob/master/starbot/painter/PicGenerator.py

use std::{
    io::{BufReader, Cursor},
    path::Path,
};

use ab_glyph::{v2::GlyphImage, Font, GlyphImageFormat, PxScale};
use anyhow::{anyhow, Result};
use image::{
    imageops::{self, FilterType},
    ImageFormat, ImageReader, Rgba, RgbaImage,
};
use imageproc::definitions::HasBlack;
use tracing::debug;

use crate::{resource::Resource, RichTextNode};

pub struct PicGenerator {
    /// image buffer
    image: RgbaImage,
    /// current x coordinate within the buffer
    x: u32,
    /// current y coordinate within the buffer
    y: u32,
    /// distance between each row
    row_space: u32,
}

impl PicGenerator {
    pub fn new(width: u32, height: u32) -> PicGenerator {
        let image = RgbaImage::new(width, height);

        PicGenerator {
            image,
            x: 0,
            y: 0,
            row_space: 25,
        }
    }

    pub fn width(&self) -> u32 {
        self.image.width()
    }

    #[allow(dead_code)]
    pub fn height(&self) -> u32 {
        self.image.height()
    }

    pub fn x(&self) -> u32 {
        self.x
    }

    pub fn y(&self) -> u32 {
        self.y
    }

    /// Set current cursor position
    pub fn set_pos(&mut self, x: u32, y: u32) -> &mut Self {
        self.x = x;
        self.y = y;
        self
    }

    pub fn set_x(&mut self, x: u32) -> &mut Self {
        self.x = x;
        self
    }

    pub fn set_y(&mut self, y: u32) -> &mut Self {
        self.y = y;
        self
    }

    pub fn set_row_space(&mut self, rs: u32) -> &mut Self {
        self.row_space = rs;
        self
    }

    /// Draw an image onto the buffer. If xy is provided will draw from xy and don't move
    /// internal coordinate, otherwise move the coordinate to the next row.
    pub fn draw_img(&mut self, img: &RgbaImage, xy: Option<(u32, u32)>) -> &mut Self {
        if let Some((x, y)) = xy {
            paste_image(&mut self.image, img, x, y);
            return self;
        }

        paste_image(&mut self.image, img, self.x, self.y);

        // Move to the next row suitable for drawing
        self.y += img.height() + self.row_space;

        self
    }

    /// Draw an image onto the buffer blending the background. If xy is provided will draw from xy and don't move
    /// internal coordinate, otherwise move the coordinate to the next row.
    pub fn draw_img_alpha(&mut self, img: &RgbaImage, xy: Option<(u32, u32)>) -> &mut Self {
        if let Some((x, y)) = xy {
            paste_image_with_alpha(&mut self.image, img, x, y);
            return self;
        }

        paste_image_with_alpha(&mut self.image, img, self.x, self.y);

        // Move to the next row suitable for drawing
        self.y += img.height() + self.row_space;

        self
    }

    /// Draw text on the buffer. If xy is provided draw from xy and don't move internal coordinate, otherwise
    /// move the coordinate to the next row after concatenating all the given texts in a single row.
    pub fn draw_text(
        &mut self,
        texts: &[&str],
        colors: &[Rgba<u8>],
        font: &impl Font,
        scale: PxScale,
        xy: Option<(u32, u32)>,
    ) -> &mut Self {
        let (mut cx, cy) = match xy {
            Some((x, y)) => (x, y),
            None => (self.x, self.y),
        };

        // If move position, move past the text's height + row_space
        let mut text_height = 0;

        for (i, &text) in texts.iter().enumerate() {
            let color = colors.get(i).copied().unwrap_or(Rgba::<u8>::black());

            let (tw, th) = imageproc::drawing::text_size(scale, font, text);

            if text_height < th {
                text_height = th;
            }

            imageproc::drawing::draw_text_mut(
                &mut self.image,
                color,
                cx as i32,
                cy as i32,
                scale,
                font,
                text,
            );

            cx += tw;
        }

        if xy.is_none() {
            self.y += text_height + self.row_space;
        }

        self
    }

    /// Draw a rectangle on the buffer. This won't move the coordinate
    pub fn draw_rectangle(
        &mut self,
        x: u32,
        y: u32,
        height: u32,
        width: u32,
        color: Rgba<u8>,
    ) -> &mut Self {
        let rect = imageproc::rect::Rect::at(x as i32, y as i32).of_size(width, height);

        imageproc::drawing::draw_filled_rect_mut(&mut self.image, rect, color);

        self
    }

    #[allow(dead_code)]
    pub fn save(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        self.image.save(path)?;

        Ok(())
    }

    // Crop image to be of the same height as self.y
    pub fn crop_bottom(&mut self) -> &mut Self {
        let cropped = imageops::crop_imm(&self.image, 0, 0, self.image.width(), self.y);
        self.image = cropped.to_image();

        self
    }

    pub fn into_image(self) -> RgbaImage {
        self.image
    }
}

pub fn create_circular_image(input_image: &RgbaImage, diameter: u32) -> RgbaImage {
    // Create a transparent background image
    let mut circular_image = RgbaImage::from_pixel(
        diameter,
        diameter,
        Rgba([0, 0, 0, 0]), // Fully transparent
    );

    // Calculate scaling factors
    let (orig_width, orig_height) = (input_image.width(), input_image.height());
    let scale = f32::min(
        diameter as f32 / orig_width as f32,
        diameter as f32 / orig_height as f32,
    );

    // Resize the original image
    let resized_image = imageops::resize(
        input_image,
        (orig_width as f32 * scale) as u32,
        (orig_height as f32 * scale) as u32,
        imageops::FilterType::Lanczos3,
    );

    // Calculate position to center the resized image
    let x_offset = (diameter - resized_image.width()) / 2;
    let y_offset = (diameter - resized_image.height()) / 2;

    // Draw a circular mask
    let center = (diameter / 2, diameter / 2);
    let radius = diameter / 2;

    // Draw each pixel of the resized image within the circle
    for (x, y, pixel) in resized_image.enumerate_pixels() {
        let circle_x = x + x_offset;
        let circle_y = y + y_offset;

        // Check if the point is within the circle
        if is_point_in_circle(circle_x, circle_y, center, radius) {
            circular_image.put_pixel(circle_x, circle_y, *pixel);
        }
    }

    circular_image
}

#[allow(clippy::too_many_arguments)]
pub fn draw_content_image(
    nodes: &[RichTextNode],
    line_max_width: u32,
    text_scale: PxScale,
    emoji_scale: PxScale,
    resource: &Resource,
) -> Vec<RgbaImage> {
    let mut images = Vec::new();

    let mut current_image = RgbaImage::new(line_max_width, 40);

    let (mut x, mut y) = (0, 0u32);

    for node in nodes {
        if let RichTextNode::Text { text } = node {
            for c in clean_special_chars(text).chars() {
                if c == '\n' {
                    images.push(std::mem::replace(
                        &mut current_image,
                        RgbaImage::new(line_max_width, 40),
                    ));
                    x = 0;
                    y = 0;
                    continue;
                }

                let s = c.to_string();

                let cwidth = if is_emoji(c) {
                    let id = resource.emoji_font.glyph_id(c);

                    let image = match resource.emoji_font.glyph_raster_image2(id, u16::MAX) {
                        Some(glyph_image) => match glyph_to_rgba(&glyph_image) {
                            Ok(image) => image,
                            Err(e) => {
                                debug!("emoji {} 无法从字体图片创建RGBA图片，跳过...: {}", c, e);
                                continue;
                            }
                        },
                        None => {
                            debug!("字体无法找到emoji {} 的字体图片，跳过...", c);
                            continue;
                        }
                    };

                    let resized_image = imageops::resize(
                        &image,
                        emoji_scale.x as u32,
                        emoji_scale.y as u32,
                        FilterType::Lanczos3,
                    );

                    paste_image_with_alpha(&mut current_image, &resized_image, x, y);

                    emoji_scale.x as u32
                } else {
                    let (cwidth, _cheight) =
                        imageproc::drawing::text_size(text_scale, &resource.text_normal_font, &s);

                    if x + cwidth > line_max_width {
                        images.push(std::mem::replace(
                            &mut current_image,
                            RgbaImage::new(line_max_width, 40),
                        ));
                        x = 0;
                        y = 0;
                    }

                    imageproc::drawing::draw_text_mut(
                        &mut current_image,
                        Rgba::<u8>::black(),
                        x as i32,
                        y as i32,
                        text_scale,
                        &resource.text_normal_font,
                        &s,
                    );

                    cwidth
                };

                x += cwidth;
            }

            continue;
        }

        let image_to_draw = match node {
            RichTextNode::Emoji { img } => img,
            RichTextNode::Web => &resource.web_image,
            RichTextNode::Bv => &resource.bv_image,
            RichTextNode::Lottery => &resource.lottery_image,
            RichTextNode::Vote => &resource.vote_image,
            RichTextNode::Goods => &resource.goods_image,
            _ => unreachable!(),
        };

        let resized_image = if matches!(node, RichTextNode::Emoji { img: _ }) {
            imageops::resize(image_to_draw, 30, 30, imageops::FilterType::Lanczos3)
        } else {
            imageops::resize(image_to_draw, 40, 40, imageops::FilterType::Lanczos3)
        };

        let image_width = resized_image.width();

        if x + image_width > line_max_width {
            images.push(std::mem::replace(
                &mut current_image,
                RgbaImage::new(line_max_width, 40),
            ));
            x = 0;
            y = 0;
        }

        paste_image_with_alpha(&mut current_image, &resized_image, x, y);
    }

    images.push(current_image);

    images
}

fn glyph_to_rgba(glyph_image: &GlyphImage<'_>) -> Result<RgbaImage> {
    if !matches!(glyph_image.format, GlyphImageFormat::Png) {
        return Err(anyhow!(
            "Unsupported glyph image type: {:?}",
            glyph_image.format
        ));
    }

    let mut reader = ImageReader::new(BufReader::new(Cursor::new(&glyph_image.data)));
    reader.set_format(ImageFormat::Png);

    let image = reader.decode()?;

    Ok(image.to_rgba8())
}

// Paste an overlay image onto the base image starting at (x, y) of the base image
fn paste_image(base_image: &mut RgbaImage, overlay_image: &RgbaImage, x: u32, y: u32) {
    for (overlay_x, overlay_y, pixel) in overlay_image.enumerate_pixels() {
        // Calculate the position on the base image
        let base_x = x + overlay_x;
        let base_y = y + overlay_y;

        // Check if the pixel is within the bounds of the base image
        if base_x < base_image.width() && base_y < base_image.height() {
            // If the overlay pixel has alpha, blend it
            if pixel[3] > 0 {
                base_image.put_pixel(base_x, base_y, *pixel);
            }
        }
    }
}

// Paste an overlay image with transparent background, blending the alpha of the pixels
fn paste_image_with_alpha(base_image: &mut RgbaImage, overlay_image: &RgbaImage, x: u32, y: u32) {
    for (overlay_x, overlay_y, overlay_pixel) in overlay_image.enumerate_pixels() {
        let base_x = x + overlay_x;
        let base_y = y + overlay_y;

        // Check if the pixel is within the bounds of the base image
        if base_x < base_image.width() && base_y < base_image.height() {
            let base_pixel = base_image.get_pixel(base_x, base_y);

            // Alpha blending calculation
            let overlay_alpha = overlay_pixel[3] as f32 / 255.0;
            let base_alpha = base_pixel[3] as f32 / 255.0;

            // Combine alpha
            let out_alpha = overlay_alpha + base_alpha * (1.0 - overlay_alpha);

            // Blend colors
            let blend_color = |overlay: u8, base: u8| -> u8 {
                ((overlay as f32 * overlay_alpha
                    + base as f32 * base_alpha * (1.0 - overlay_alpha))
                    / out_alpha) as u8
            };

            let blended_pixel = Rgba([
                blend_color(overlay_pixel[0], base_pixel[0]),
                blend_color(overlay_pixel[1], base_pixel[1]),
                blend_color(overlay_pixel[2], base_pixel[2]),
                (out_alpha * 255.0) as u8,
            ]);

            base_image.put_pixel(base_x, base_y, blended_pixel);
        }
    }
}

// Helper function to check if a point is inside a circle
fn is_point_in_circle(x: u32, y: u32, center: (u32, u32), radius: u32) -> bool {
    let dx = x as i32 - center.0 as i32;
    let dy = y as i32 - center.1 as i32;

    // Use Pythagorean theorem to check if point is within circle
    (dx * dx + dy * dy) <= (radius * radius) as i32
}

fn clean_special_chars(s: &str) -> String {
    s.replace(
        [
            char::from_u32(8203).unwrap(),
            char::from_u32(65039).unwrap(),
        ],
        "",
    )
}

/// Checks if a given character is an emoji
///
/// # Arguments
/// * `character` - The character to check
///
/// # Returns
/// * `bool` - True if the character is an emoji, false otherwise
fn is_emoji(character: char) -> bool {
    let code = character as u32;
    matches!(code,
        0x1F600..=0x1F64F   // Emoticons
        | 0x1F300..=0x1F5FF // Misc Symbols and Pictographs
        | 0x1F680..=0x1F6FF // Transport and Map Symbols
        | 0x2600..=0x26FF   // Misc symbols
        | 0x2700..=0x27BF   // Dingbats
        | 0xFE00..=0xFE0F   // Variation Selectors
        | 0x1F900..=0x1F9FF // Supplemental Symbols and Pictographs
        | 0x1F1E6..=0x1F1FF // Regional indicator symbols
    )
}

#[cfg(test)]
mod tests {
    use crate::WHITE;

    use super::*;
    use image::ImageReader;

    #[test]
    fn test_paste_image() {
        let img = ImageReader::open("test_resources/image.JPG")
            .unwrap()
            .decode()
            .unwrap();

        let rgba_img = img.into_rgba8();

        let h = rgba_img.height();

        let w = rgba_img.width();

        let mut base_img = RgbaImage::new(w * 2, h * 2);

        paste_image(&mut base_img, &rgba_img, 0, 0);
        paste_image(&mut base_img, &rgba_img, w, 0);
        paste_image(&mut base_img, &rgba_img, 0, h);
        paste_image(&mut base_img, &rgba_img, w, h);

        base_img.save("test_data/combined_image.png").unwrap();
    }

    #[test]
    fn test_paste_image_alpha() {
        let mut base = ImageReader::open("test_resources/background.jpg")
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8();

        let overlay = ImageReader::open("test_resources/transparent.png")
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8();

        let x = base.width() / 2;
        let y = base.height() / 2;

        paste_image_with_alpha(&mut base, &overlay, x, y);

        base.save("test_data/combined_image_alpha.png").unwrap();
    }

    #[test]
    fn test_gen_emoji() {
        let node = vec![RichTextNode::Text {
            text: "你是脑残吗😀🥰👿💩😡🥰😸".to_string(),
        }];

        let res = Resource::load_from_dir("./resource").unwrap();

        let images = draw_content_image(&node, 1000, 40.0.into(), 35.0.into(), &res);

        let mut gen = PicGenerator::new(1000, 1000);

        gen.draw_rectangle(0, 0, 1000, 1000, WHITE);

        for i in images {
            gen.draw_img(&i, None);
        }

        gen.save("test_data/emoji.png").unwrap();
    }
}
