mod config;
mod painter;

use std::{
    cmp,
    io::{BufReader, Cursor},
    path::Path,
    str::FromStr,
};

use ab_glyph::{FontArc, PxScale};
use anyhow::{anyhow, Context};
use base64::Engine;
use config::{get_config_from_file, BiliConfig, Config, MiraiConfig, TargetConfig};
use image::{
    imageops::{self, FilterType},
    ImageReader, Rgba, RgbaImage,
};
use lazy_static::lazy_static;
use painter::{create_circular_image, draw_content_image, PicGenerator};
use reqwest::IntoUrl;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info};
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

const TEXT_SCALE: PxScale = uniform_scale(30.0);
const TIP_SCALE: PxScale = uniform_scale(25.0);
const EMOJI_SCALE: PxScale = uniform_scale(109.0);

const fn uniform_scale(s: f32) -> PxScale {
    PxScale { x: s, y: s }
}

#[derive(Debug)]
pub struct Resource {
    pub text_normal_font: FontArc,
    pub emoji_font: FontArc,
    pub vip_image: RgbaImage,
    pub web_image: RgbaImage,
    pub bv_image: RgbaImage,
    pub lottery_image: RgbaImage,
    pub vote_image: RgbaImage,
    pub goods_image: RgbaImage,
}

lazy_static! {
    static ref RESOURCE: Resource = load_resource("./resource").expect("加载资源失败");
    static ref WHITE: Rgba<u8> = [255, 255, 255, 255].into();
    static ref BLACK: Rgba<u8> = [0, 0, 0, 255].into();
    static ref GRAY: Rgba<u8> = [169, 169, 169, 255].into();
    static ref PINK: Rgba<u8> = [251, 114, 153, 255].into();
}

#[derive(Debug)]
enum RichTextNode {
    // RICH_TEXT_NODE_TYPE_TEXT
    Text { text: String },
    // RICH_TEXT_NODE_TYPE_EMOJI
    Emoji { img: RgbaImage },
    // RICH_TEXT_NODE_TYPE_WEB
    Web,
    // RICH_TEXT_NODE_TYPE_BV
    Bv,
    // RICH_TEXT_NODE_TYPE_LOTTERY
    Lottery,
    // RICH_TEXT_NODE_TYPE_VOTE
    Vote,
    // RICH_TEXT_NODE_TYPE_GOODS
    Goods,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set log collector
    let filter_layer =
        Targets::from_str(std::env::var("RUST_LOG").as_deref().unwrap_or("info")).unwrap();
    let format_layer = tracing_subscriber::fmt::layer();
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(format_layer)
        .init();

    info!("tracing配置完成, 从spider.toml中读取爬虫配置");

    let Config {
        mirai,
        bili,
        target,
    } = get_config_from_file("spider.toml")
        .await
        .context("Get config for spider")?;

    let mut handles = Vec::new();

    for t in target {
        let m = mirai.clone();
        let b = bili.clone();
        handles.push(tokio::spawn(run_target(m, b, t)));
    }

    futures::future::join_all(handles.into_iter()).await;

    Ok(())
}

async fn run_target(
    mirai: MiraiConfig,
    bili: BiliConfig,
    target: TargetConfig,
) -> anyhow::Result<()> {
    info!(
        "开始监听b站用户UID {} 的动态并发送给 QQ 号{}",
        target.uid, target.receiver_qq
    );

    info!(
        "开始获取MIRAI HTTP会话并绑定发送者QQ号 {}",
        target.sender_qq
    );

    let tz = jiff::tz::TimeZone::get("Asia/Chongqing").unwrap();

    let client = reqwest::Client::new();

    let verify_request = VerifyRequest {
        verify_key: mirai.verify_key.clone(),
    };

    let verify_response: VerifyResponse = client
        .post(format!("{}/verify", mirai.http_url))
        .json(&verify_request)
        .send()
        .await?
        .json()
        .await?;

    let session_key = verify_response.session;

    let bind_request = BindRequest {
        session_key: session_key.clone(),
        qq: target.sender_qq,
    };

    let bind_response: BindResponse = client
        .post(format!("{}/bind", mirai.http_url))
        .json(&bind_request)
        .send()
        .await?
        .json()
        .await?;

    assert_eq!(
        0, bind_response.code,
        "绑定发送者QQ失败: {}",
        bind_response.msg
    );

    info!("成功获取MIRAI会话并绑定发送者QQ");

    let mut last_ts = jiff::Timestamp::now();

    info!(
        "开始监听UID {} 自 {} 之后的动态消息",
        target.uid,
        print_ts(last_ts, tz.clone()),
    );

    let cookie = format!("SESSDATA={}", bili.sess_data);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(target.interval_sec)).await;

        let response: Value = client
            .get("https://api.vc.bilibili.com/dynamic_svr/v1/dynamic_svr/space_history")
            .header("COOKIE", &cookie)
            .query(&[
                ("host_uid", target.uid),
                ("offset_dynamic_id", 0),
                ("need_top", 0),
            ])
            .send()
            .await
            .context("Request dynamic from Bilibili")?
            .json()
            .await
            .context("Parse dynamic response from Bilibili")?;

        let code = response["code"].as_i64().unwrap();

        if code != 0 {
            error!("获取用户动态失败: {:?}", response);
            continue;
        }

        let Some(cards) = response["data"]["cards"].as_array() else {
            error!("没有获取到任何动态: {:?}", response);
            continue;
        };

        for card in cards.iter().rev() {
            let desc = &card["desc"];
            let uname = desc["user_profile"]["info"]["uname"].as_str().unwrap();

            let dynamic_id = desc["dynamic_id"].as_i64().unwrap();

            if dynamic_id != 979492889902972978 {
                continue;
            }

            let ts = jiff::Timestamp::from_second(desc["timestamp"].as_i64().unwrap()).unwrap();

            if ts <= last_ts {
                continue;
            }

            last_ts = ts;

            let dynamic_type = desc.get("type").unwrap().as_i64().unwrap();

            match dynamic_type {
                2 | 4 => {
                    info!("监听到 {} 新动态 {}", uname, dynamic_id);

                    // 构建动态内容图片
                    let mut generator = PicGenerator::new(740, 10000);

                    generator.draw_rectangle(0, 0, 10000, 740, *WHITE);

                    // TODO: what if user has no face?
                    let face = desc["user_profile"]["info"]["face"].as_str().unwrap();
                    let face_image = download_image(face).await?;
                    let vip = desc["user_profile"]["vip"]["nickname_color"]
                        .as_str()
                        .unwrap()
                        != "";
                    let ts_str = print_ts(ts, tz.clone());

                    // 绘制用户头像 (100 x 100放在圆形框框内)
                    let resized_face =
                        imageops::resize(&face_image, 100, 100, FilterType::Lanczos3);
                    let circular_face = create_circular_image(&resized_face, 100);
                    generator.draw_img_alpha(&circular_face, Some((50, 50)));
                    // 绘制大会员下标
                    if vip {
                        generator.draw_img_alpha(&RESOURCE.vip_image, Some((118, 118)));
                    }

                    generator.set_pos(175, 60);

                    let uname_color = if vip { *PINK } else { *BLACK };

                    // 绘制用户名和动态时间戳
                    generator.draw_text(
                        &[uname],
                        &[uname_color],
                        &RESOURCE.text_normal_font,
                        TEXT_SCALE,
                        None,
                    );
                    generator.draw_text(
                        &[&ts_str],
                        &[*GRAY],
                        &RESOURCE.text_normal_font,
                        TIP_SCALE,
                        None,
                    );

                    generator.set_x(25);
                    generator.set_row_space(10);

                    // 获取动态内容
                    let get_detail_response: Value =  client
                        .get(format!("https://api.bilibili.com/x/polymer/web-dynamic/v1/detail?timezone_offset=-480&id={}&features=itemOpusStyle,opusBigCover,onlyfansVote", dynamic_id))
                        .header("COOKIE", format!("SESSDATA={}", bili.sess_data))
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();

                    let opus = &get_detail_response["data"]["item"]["modules"]["module_dynamic"]
                        ["major"]["opus"];

                    let title = opus
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let raw_text_nodes = opus
                        .get("summary")
                        .and_then(|summary| summary.get("rich_text_nodes"))
                        .and_then(Value::as_array);

                    let rich_text_nodes = if let Some(raw_text_nodes) = raw_text_nodes {
                        build_text_nodes(title, raw_text_nodes)
                    } else {
                        build_text_nodes(title, &[])
                    }
                    .await?;

                    // 绘制动态内容
                    let content_images = draw_content_image(
                        &rich_text_nodes,
                        generator.width() - 50,
                        &RESOURCE.text_normal_font,
                        &RESOURCE.emoji_font,
                        TEXT_SCALE,
                        EMOJI_SCALE,
                        &RESOURCE.web_image,
                        &RESOURCE.bv_image,
                        &RESOURCE.lottery_image,
                        &RESOURCE.vote_image,
                        &RESOURCE.goods_image,
                    );

                    for img in content_images {
                        generator.draw_img_alpha(&img, None);
                    }

                    generator.set_x(10);

                    if dynamic_type == 2 {
                        let card: Value = serde_json::from_str(card["card"].as_str().unwrap())?;
                        // 获取动态图片
                        let pictures = &card["item"]["pictures"];

                        let images =
                            download_dynamic_images(pictures, generator.width(), 10).await?;

                        let num_pics_per_line = match images.len() {
                            1 => 1,
                            2 | 4 => 2,
                            _ => 3,
                        };

                        let image_lines: Vec<&[RgbaImage]> =
                            images.chunks(num_pics_per_line).collect();

                        let (mut x, mut y) = (generator.x(), generator.y());

                        for line in image_lines {
                            for (i, img) in line.iter().enumerate() {
                                generator.draw_img(img, Some((x, y)));

                                x += img.width() + 10;
                                generator.set_x(x);

                                if i == line.len() - 1 {
                                    y += img.height() + 10;
                                    generator.set_y(y);
                                    x = 10;
                                    generator.set_x(x);
                                }
                            }
                        }

                        // bottom margin
                        generator.set_y(y + 20);
                    }

                    generator.crop_bottom();

                    let image = generator.into_image();

                    let mut png_buffer = Vec::new();
                    let mut cursor = Cursor::new(&mut png_buffer);
                    image.write_to(&mut cursor, image::ImageFormat::Png)?;
                    let image_b64 = base64::engine::general_purpose::STANDARD.encode(&png_buffer);

                    // 构造QQ消息链
                    let mut messages = Vec::new();

                    messages.push(Message::Plain {
                        text: format!(
                            "{} 发表了新动态:\n\nhttps://t.bilibili.com/{}\n\n",
                            uname, dynamic_id,
                        ),
                    });

                    messages.push(Message::Image { base64: image_b64 });

                    info!("发送QQ消息");

                    let send_request = SendFriendMessageRequest {
                        session_key: session_key.clone(),
                        target: target.receiver_qq,
                        message_chain: messages,
                    };

                    let send_response: SendFriendMessageResponse = client
                        .post(format!("{}/sendFriendMessage", mirai.http_url))
                        .json(&send_request)
                        .send()
                        .await
                        .context("Request MIRAI /sendFriendMessage")?
                        .json()
                        .await?;

                    if send_response.code != 0 {
                        error!(
                            "发送qq消息失败: {}: {}",
                            send_response.code, send_response.msg
                        );
                    }
                }

                x => {
                    debug!("跳过不支持的动态类型: {}", x);
                }
            }
        }

        // Refresh session to keep it alive
        let _ = client
            .get(format!("{}/sessionInfo", mirai.http_url))
            .query(&[("sessionKey", &session_key)])
            .send()
            .await?
            .bytes()
            .await?;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifyRequest {
    #[serde(rename = "verifyKey")]
    verify_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

struct VerifyResponse {
    code: i32,
    session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindRequest {
    #[serde(rename = "sessionKey")]
    session_key: String,
    qq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindResponse {
    code: i32,
    msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseRequest {
    #[serde(rename = "sessionKey")]
    session_key: String,
    qq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseResponse {
    code: i32,
    msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendFriendMessageRequest {
    session_key: String,
    target: i64,
    message_chain: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendFriendMessageResponse {
    code: i32,
    msg: String,
}

/// `https://github.com/project-mirai/mirai-api-http/blob/e9d5609b1cd580217a868f2daa789360283ba289/docs/api/MessageType.md`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum Message {
    Plain { text: String },
    Image { base64: String },
}

#[tokio::test]
async fn test_send_qq() {
    const MIRAI_URL: &str = "http://localhost:7827";
    const MIRAI_VERIFY_KEY: &str = "INITKEYLunaRyu";
    const BOT_QQ: i64 = 1320117484;
    const TARGET_QQ: i64 = 3922347898;

    let client = reqwest::Client::new();

    let verify_request = VerifyRequest {
        verify_key: MIRAI_VERIFY_KEY.to_string(),
    };

    let verify_response: VerifyResponse = client
        .post(format!("{}/verify", MIRAI_URL))
        .json(&verify_request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(0, verify_response.code, "verify failed");

    let session_key = verify_response.session;

    println!("Got session key: {}", session_key);

    let bind_request = BindRequest {
        session_key: session_key.clone(),
        qq: BOT_QQ,
    };

    let bind_response: BindResponse = client
        .post(format!("{}/bind", MIRAI_URL))
        .json(&bind_request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(0, bind_response.code, "bind failed: {}", bind_response.msg);

    println!("bind session key {} to qq {}", session_key, BOT_QQ);

    let send_request = SendFriendMessageRequest {
        session_key: session_key.clone(),
        target: TARGET_QQ,
        message_chain: vec![Message::Plain {
            text: "Hello world".to_string(),
        }],
    };

    let send_response: serde_json::Value = client
        .post(format!("{}/sendFriendMessage", MIRAI_URL))
        .json(&send_request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let ss = serde_json::to_string_pretty(&send_response).unwrap();

    println!("{}", ss);

    let release_request = ReleaseRequest {
        session_key: session_key.clone(),
        qq: BOT_QQ,
    };

    let release_response: ReleaseResponse = client
        .post(format!("{}/release", MIRAI_URL))
        .json(&release_request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        0, release_response.code,
        "release failed: {}",
        release_response.msg
    );

    println!("released session key {}", session_key);
}

fn print_ts(ts: jiff::Timestamp, tz: jiff::tz::TimeZone) -> String {
    let zoned = ts.to_zoned(tz);

    jiff::fmt::strtime::format("%Y-%m-%d %H:%M", &zoned).unwrap()
}

fn load_resource(dir: impl AsRef<Path>) -> anyhow::Result<Resource> {
    let text_normal_font = load_font_from_file(dir.as_ref().join("normal.ttf"))?;
    let emoji_font = load_font_from_file(dir.as_ref().join("emoji.ttf"))?;
    let web_image = load_image_from_file(dir.as_ref().join("link.png"))?;
    let bv_image = load_image_from_file(dir.as_ref().join("video.png"))?;
    let lottery_image = load_image_from_file(dir.as_ref().join("box.png"))?;
    let vote_image = load_image_from_file(dir.as_ref().join("tick.png"))?;
    let goods_image = load_image_from_file(dir.as_ref().join("tb.png"))?;
    let vip_image = load_image_from_file(dir.as_ref().join("vip.png"))?;

    Ok(Resource {
        text_normal_font,
        emoji_font,
        vip_image,
        web_image,
        bv_image,
        lottery_image,
        vote_image,
        goods_image,
    })
}

fn load_font_from_file(path: impl AsRef<Path>) -> anyhow::Result<FontArc> {
    let bytes = std::fs::read(path)?;
    Ok(FontArc::try_from_vec(bytes)?)
}

fn load_image_from_file(path: impl AsRef<Path>) -> anyhow::Result<RgbaImage> {
    Ok(ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .into_rgba8())
}

async fn download_image(url: impl IntoUrl) -> anyhow::Result<RgbaImage> {
    let response = reqwest::get(url).await?;

    let bytes = response.bytes().await?;

    let cursor = Cursor::new(&*bytes);

    let image = ImageReader::new(BufReader::new(cursor))
        .with_guessed_format()?
        .decode()?
        .into_rgba8();

    Ok(image)
}

async fn build_text_nodes(
    title: Option<String>,
    raw_text_nodes: &[Value],
) -> anyhow::Result<Vec<RichTextNode>> {
    let mut res = Vec::with_capacity(raw_text_nodes.len() + 1);

    if let Some(title) = title {
        res.push(RichTextNode::Text { text: title });
    }

    for node in raw_text_nodes {
        let type_ = node.get("type").unwrap().as_str().unwrap();

        match type_ {
            "RICH_TEXT_NODE_TYPE_EMOJI" => match download_emoji(node).await {
                Ok(img) => res.push(RichTextNode::Emoji { img }),
                Err(e) => {
                    error!("无法下载emoji, 使用文字代替: {}", e);
                    if let Some(Some(text)) = node.get("text").map(Value::as_str) {
                        res.push(RichTextNode::Text {
                            text: text.to_string(),
                        });
                    }
                }
            },
            "RICH_TEXT_NODE_TYPE_WEB" => res.push(RichTextNode::Web),
            "RICH_TEXT_NODE_TYPE_BV" => res.push(RichTextNode::Bv),
            "RICH_TEXT_NODE_TYPE_LOTTERY" => res.push(RichTextNode::Lottery),
            "RICH_TEXT_NODE_TYPE_VOTE" => res.push(RichTextNode::Vote),
            "RICH_TEXT_NODE_TYPE_GOODS" => res.push(RichTextNode::Goods),
            _ => {
                if let Some(Some(text)) = node.get("text").map(Value::as_str) {
                    res.push(RichTextNode::Text {
                        text: text.to_string(),
                    });
                }
            }
        }
    }

    Ok(res)
}

async fn download_emoji(emoji_node: &Value) -> anyhow::Result<RgbaImage> {
    if let Some(emoji) = emoji_node.get("emoji") {
        if let Some(Some(icon_url)) = emoji.get("icon_url").map(Value::as_str) {
            let response = reqwest::get(icon_url).await?;

            let bytes = response.bytes().await?;

            let cursor = Cursor::new(&*bytes);

            let image = ImageReader::new(BufReader::new(cursor))
                .with_guessed_format()?
                .decode()?
                .into_rgba8();

            return Ok(image);
        }
    }

    Err(anyhow!("No emoji icon url found"))
}

pub async fn download_dynamic_images(
    pictures: &Value,
    image_area_width: u32,
    image_margin: u32,
) -> anyhow::Result<Vec<RgbaImage>> {
    let pictures = pictures.as_array().unwrap();

    let num_pictures = pictures.len();

    // - 1 picture -> Just show one
    // - 2 or 4 picture -> show 2 pictures in a line and do 1 or 2 lines
    // - other -> show 3 pictures in a line
    let (num_pictures_in_line, picture_square_size) = match num_pictures {
        1 => (1, image_area_width - image_margin * 2),
        2 | 4 => (2, (image_area_width - image_margin * 3) / 2),
        _ => (3, (image_area_width - image_margin * 4) / 3),
    };

    // https://github.com/Starlwr/StarBot/blob/f92b4d71366e19046f5c1ae87fe85f2f2461cd69/starbot/painter/DynamicPicGenerator.py#L452-L469
    let mut set = Vec::with_capacity(pictures.len());
    for pic in pictures {
        let src = pic["img_src"].as_str().unwrap().to_string();
        let height = pic["img_height"].as_f64().unwrap();
        let width = pic["img_width"].as_f64().unwrap();

        if num_pictures_in_line == 1 {
            set.push(download_image(format!("{}@518w.webp", src)));
        } else if height / width >= 3.0 {
            set.push(download_image(format!(
                "{}@{}w_{}h_!header.webp",
                src, picture_square_size, picture_square_size
            )));
        } else {
            set.push(download_image(format!(
                "{}@{}w_{}h_1e_1c.webp",
                src, picture_square_size, picture_square_size
            )));
        }
    }

    let results = futures::future::join_all(set.into_iter()).await;

    let mut images = Vec::with_capacity(results.len());

    for result in results {
        match result {
            Ok(img) => {
                let resized_image = match img.height().cmp(&img.width()) {
                    cmp::Ordering::Equal => imageops::resize(
                        &img,
                        picture_square_size,
                        picture_square_size,
                        FilterType::Lanczos3,
                    ),
                    cmp::Ordering::Less => {
                        // Image is wider, make height -> size and crop width from left and right
                        let nheight = picture_square_size;
                        let nwidth = ((picture_square_size as f64) * (img.width() as f64)
                            / (img.height() as f64))
                            .round() as u32;

                        let resized = imageops::resize(&img, nwidth, nheight, FilterType::Lanczos3);

                        imageops::crop_imm(
                            &resized,
                            (nwidth - picture_square_size) / 2,
                            0,
                            picture_square_size,
                            picture_square_size,
                        )
                        .to_image()
                    }
                    cmp::Ordering::Greater => {
                        // Image is longer, make width -> size and crop height from top and bottom
                        let nwidth = picture_square_size;
                        let nheight = ((picture_square_size as f64) * (img.height() as f64)
                            / (img.width() as f64))
                            .round() as u32;

                        let resized = imageops::resize(&img, nwidth, nheight, FilterType::Lanczos3);

                        imageops::crop_imm(
                            &resized,
                            0,
                            (nheight - picture_square_size) / 2,
                            picture_square_size,
                            picture_square_size,
                        )
                        .to_image()
                    }
                };

                images.push(resized_image);
            }
            Err(e) => {
                println!("{}", e);
                continue;
            }
        }
    }

    Ok(images)
}
