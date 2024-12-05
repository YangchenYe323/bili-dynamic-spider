use std::path::Path;

use ab_glyph::FontArc;
use image::{ImageReader, RgbaImage};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref RESOURCE: Resource =
        Resource::load_from_dir("./resource").expect("加载资源失败");
}

#[derive(Debug)]
pub struct Resource {
    pub text_normal_font: FontArc,
    pub emoji_font: FontArc,
    pub no_face_image: RgbaImage,
    pub vip_image: RgbaImage,
    pub web_image: RgbaImage,
    pub bv_image: RgbaImage,
    pub lottery_image: RgbaImage,
    pub vote_image: RgbaImage,
    pub goods_image: RgbaImage,
}

struct ResourceLoader<P> {
    base_dir: P,
}

impl Resource {
    pub fn load_from_dir(dir: impl AsRef<Path>) -> anyhow::Result<Resource> {
        let loader = ResourceLoader { base_dir: dir };

        let text_normal_font = loader.load_font("normal.ttf")?;
        let emoji_font = loader.load_font("emoji.ttf")?;
        let no_face_image = loader.load_image("face.png")?;
        let web_image = loader.load_image("link.png")?;
        let bv_image = loader.load_image("video.png")?;
        let lottery_image = loader.load_image("box.png")?;
        let vote_image = loader.load_image("tick.png")?;
        let goods_image = loader.load_image("tb.png")?;
        let vip_image = loader.load_image("vip.png")?;

        Ok(Resource {
            text_normal_font,
            emoji_font,
            no_face_image,
            vip_image,
            web_image,
            bv_image,
            lottery_image,
            vote_image,
            goods_image,
        })
    }
}

impl<P: AsRef<Path>> ResourceLoader<P> {
    fn load_font(&self, relative_path: impl AsRef<Path>) -> anyhow::Result<FontArc> {
        let path = self.base_dir.as_ref().join(relative_path);
        let bytes = std::fs::read(path)?;
        Ok(FontArc::try_from_vec(bytes)?)
    }

    fn load_image(&self, relative_path: impl AsRef<Path>) -> anyhow::Result<RgbaImage> {
        let path = self.base_dir.as_ref().join(relative_path);
        Ok(ImageReader::open(path)?
            .with_guessed_format()?
            .decode()?
            .into_rgba8())
    }
}
