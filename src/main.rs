mod config;
mod painter;
mod resource;

use std::{
    cmp,
    io::{BufReader, Cursor},
    str::FromStr,
    time::Duration,
};

use ab_glyph::PxScale;
use anyhow::{anyhow, Context};
use base64::Engine;
use config::{get_config_from_file, BiliConfig, Config, MiraiConfig, TargetConfig};
use image::{
    imageops::{self, FilterType},
    ImageReader, Rgba, RgbaImage,
};
use jiff::{
    fmt::strtime,
    tz::{Offset, TimeZone},
    Timestamp,
};
use painter::{create_circular_image, draw_content_image, PicGenerator};
use reqwest::{Client, IntoUrl};
use resource::RESOURCE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sled::Tree;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{filter::Targets, layer::SubscriberExt, util::SubscriberInitExt};

const TEXT_SCALE: PxScale = uniform_scale(30.0);
const TIP_SCALE: PxScale = uniform_scale(25.0);
const EMOJI_SCALE: PxScale = uniform_scale(25.0);

const fn uniform_scale(s: f32) -> PxScale {
    PxScale { x: s, y: s }
}

const WHITE: Rgba<u8> = Rgba::<u8>([255, 255, 255, 255]);
const BLACK: Rgba<u8> = Rgba::<u8>([0, 0, 0, 255]);
const GRAY: Rgba<u8> = Rgba::<u8>([169, 169, 169, 255]);
const LIGHT_GRAY: Rgba<u8> = Rgba::<u8>([244, 244, 244, 255]);
const PINK: Rgba<u8> = Rgba::<u8>([251, 114, 153, 255]);
const DEEP_BLUE: Rgba<u8> = Rgba::<u8>([175, 238, 238, 255]);

const DYNAMIC_TYPE_DRAW: &str = "DYNAMIC_TYPE_DRAW"; // 带图动态
const DYNAMIC_TYPE_FORWARD: &str = "DYNAMIC_TYPE_FORWARD"; //转发动态
const DYNAMIC_TYPE_WORD: &str = "DYNAMIC_TYPE_WORD"; // 纯文字动态
const DYNAMIC_TYPE_LIVE: &str = "DYNAMIC_TYPE_LIVE"; // 直播动态

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DbEntry {
    // 当前动态是否发送过
    sent: bool,
    // 当前动态类型(`https://github.com/SocialSisterYi/bilibili-API-collect/blob/master/docs/dynamic/card_info.md`)
    #[serde(rename = "type")]
    type_: i32,
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

    info!("日志配置完成, 从spider.toml中读取爬虫配置");

    let Config {
        db: db_config,
        mirai,
        bili,
        target,
    } = get_config_from_file("spider.toml")
        .await
        .context("Get config for spider")?;

    let db = sled::open(db_config.path)?;

    let mut target_set = JoinSet::new();

    for t in target {
        let tree = db.open_tree(format!("{}", t.uid))?;
        let m = mirai.clone();
        let b = bili.clone();
        target_set.spawn(run_target(tree, m, b, t));
    }

    while let Some(res) = target_set.join_next().await {
        res??;
    }

    Ok(())
}

async fn run_target(
    db: Tree,
    mirai: MiraiConfig,
    bili: BiliConfig,
    target: TargetConfig,
) -> anyhow::Result<()> {
    info!(
        "开始监听b站用户UID {} 的动态并发送给 QQ 号{}",
        target.uid, target.receiver_qq
    );

    let client = reqwest::Client::new();

    let cookie = format!("SESSDATA={}", bili.sess_data);

    loop {
        let mut resent_entries = Vec::new();

        let mut it = db.iter();
        // 首先尝试重发在数据库中但是并未发出过的消息
        while let Some(Ok((k, v))) = it.next() {
            let dynamic_id: i64 = serde_json::from_slice(&k).unwrap();
            let mut entry: DbEntry = serde_json::from_slice(&v).unwrap();

            if !entry.sent {
                info!("重发动态 {}", dynamic_id);

                match create_message_from_dynamic(&bili, &client, dynamic_id).await {
                    Ok(msg) => match send_qq_message(&mirai, &target, &client, msg).await {
                        Ok(_) => {
                            entry.sent = true;

                            let v = serde_json::to_vec(&entry).unwrap();

                            resent_entries.push((k, v));
                        }
                        Err(e) => {
                            error!("重发错过的动态失败: {}", e)
                        }
                    },
                    Err(e) => {
                        error!("无法创建消息: {}", e);
                    }
                }
            }
        }

        for (k, v) in resent_entries {
            db.insert(k, v)?;
        }

        // 获取新动态并发送
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

        // 获取三条最新动态, 按照时间戳从小到大排列
        for card in cards.iter().take(3).rev() {
            let desc = &card["desc"];
            let uname = desc["user_profile"]["info"]["uname"].as_str().unwrap();

            let dynamic_id = desc["dynamic_id"].as_i64().unwrap();
            let dynamic_type = desc.get("type").unwrap().as_i64().unwrap();

            if dynamic_type != 2 && dynamic_type != 4 && dynamic_type != 1 && dynamic_type != 4200 {
                debug!("跳过不支持的动态类型 {} ({})", dynamic_id, dynamic_type);
                continue;
            }

            let dynamic_key = serde_json::to_vec(&dynamic_id).unwrap();
            if db.contains_key(&dynamic_key)? {
                debug!("跳过已经收录过的动态 {}", dynamic_id);
                continue;
            }

            let mut entry = DbEntry {
                sent: false,
                type_: dynamic_type as i32,
            };

            db.insert(&dynamic_key, serde_json::to_vec(&entry).unwrap())?;

            info!("监听到 {} 新动态 {}", uname, dynamic_id);

            match create_message_from_dynamic(&bili, &client, dynamic_id).await {
                Ok(messages) => match send_qq_message(&mirai, &target, &client, messages).await {
                    Ok(_) => {
                        entry.sent = true;
                        db.insert(&dynamic_key, serde_json::to_vec(&entry).unwrap())
                            .unwrap();
                    }
                    Err(e) => {
                        error!("发送qq消息失败: {}", e);
                    }
                },
                Err(e) => {
                    error!("{}", e)
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(target.interval_sec)).await;
    }
}

async fn create_message_from_dynamic(
    bili: &BiliConfig,
    client: &Client,
    dynamic_id: i64,
) -> anyhow::Result<Vec<Message>> {
    // 访问网络获取动态数据结构
    let dynamic = BiliDynamic::fetch(bili, client, dynamic_id).await?;
    // 画一张动态图
    let image = draw_dynamic(&dynamic);

    // 图片base64编码传到qq API
    let mut png_buffer = Vec::new();
    let mut cursor = Cursor::new(&mut png_buffer);
    image.write_to(&mut cursor, image::ImageFormat::Png)?;
    let image_b64 = base64::engine::general_purpose::STANDARD.encode(&png_buffer);

    // 构造QQ消息链
    let mut messages = Vec::new();

    let header = match &dynamic.content {
        Content::Forward {
            texts: _,
            original_author: _,
            original: _,
        } => {
            format!(
                "{} 转发了动态\nhttps://t.bilibili.com/{}\n",
                dynamic.author.uname, dynamic_id
            )
        }
        Content::Draw { texts: _, pics: _ } => format!(
            "{} 发表了新动态\nhttps://t.bilibili.com/{}\n",
            dynamic.author.uname, dynamic_id
        ),
        Content::Word { texts: _ } => format!(
            "{} 发表了新动态\nhttps://t.bilibili.com/{}\n",
            dynamic.author.uname, dynamic_id
        ),
        Content::Live {
            live_id,
            live_title: _,
            live_cover: _,
        } => format!(
            "{} 直播了\nhttps://live.bilibili.com/{}\n",
            dynamic.author.uname, live_id
        ),
    };
    messages.push(Message::Plain { text: header });
    messages.push(Message::Image { base64: image_b64 });

    Ok(messages)
}

async fn send_qq_message(
    mirai: &MiraiConfig,
    target: &TargetConfig,
    client: &Client,
    messages: Vec<Message>,
) -> anyhow::Result<()> {
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

    if verify_response.code != 0 {
        return Err(anyhow!(
            "{}: {}",
            verify_response.code,
            verify_response.msg.unwrap()
        ));
    }

    let session_key = verify_response.session.unwrap();

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

    if bind_response.code != 0 {
        return Err(anyhow!("{}: {}", bind_response.code, bind_response.msg));
    }

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
        return Err(anyhow!("{}: {}", send_response.code, send_response.msg));
    }

    let release_request = ReleaseRequest {
        session_key: session_key.clone(),
        qq: target.sender_qq,
    };

    let release_response: ReleaseResponse = client
        .post(format!("{}/release", mirai.http_url))
        .json(&release_request)
        .send()
        .await?
        .json()
        .await?;

    if release_response.code != 0 {
        return Err(anyhow!(
            "{}: {}",
            release_response.code,
            release_response.msg
        ));
    }

    Ok(())
}

#[derive(Debug)]
struct BiliDynamic {
    author: AuthorInfo,
    content: Content,
}

#[derive(Debug)]
struct AuthorInfo {
    uname: String,
    vip: bool,
    publish_timestamp: i64,
    avatar_image: RgbaImage,
}

#[derive(Debug)]
enum Content {
    // 转发动态
    Forward {
        texts: Vec<RichTextNode>,
        original_author: String,
        original: Box<Content>,
    },
    // 带图动态
    Draw {
        texts: Vec<RichTextNode>,
        pics: Vec<RgbaImage>,
    },
    // 纯文字动态
    Word {
        texts: Vec<RichTextNode>,
    },
    // 直播动态
    Live {
        live_id: i64,
        live_title: String,
        live_cover: RgbaImage,
    },
}

impl BiliDynamic {
    async fn fetch(
        bili: &BiliConfig,
        client: &Client,
        dynamic_id: i64,
    ) -> anyhow::Result<BiliDynamic> {
        let detail_response: Value =  client
                        .get(format!("https://api.bilibili.com/x/polymer/web-dynamic/v1/detail?timezone_offset=-480&id={}&features=itemOpusStyle,opusBigCover,onlyfansVote", dynamic_id))
                        .header("COOKIE", format!("SESSDATA={}", bili.sess_data))
                        .send()
                        .await?
                        .json()
                        .await?;

        let item = &detail_response["data"]["item"];

        // 构建作者
        let author_info = &item["modules"]["module_author"];
        let uname = author_info["name"].as_str().unwrap().to_string();
        let face_url = author_info.get("face").and_then(Value::as_str);
        let face_image = if let Some(face_url) = face_url {
            download_image(face_url).await?
        } else {
            RESOURCE.no_face_image.clone()
        };
        let vip = author_info
            .get("vip")
            .and_then(|v| v.get("nickname_color"))
            .and_then(|c| c.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or_default();
        let timestamp = author_info
            .get("pub_ts")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        let author = AuthorInfo {
            uname,
            vip,
            publish_timestamp: timestamp,
            avatar_image: face_image,
        };

        // 构建内容
        let content = Content::from_detail_json(bili, client, item).await?;

        Ok(BiliDynamic { author, content })
    }
}

impl Content {
    /// * `response["data"]["item"]` field of response from dynamic detail API https://api.bilibili.com/x/polymer/web-dynamic/v1/detail
    async fn from_detail_json(
        bili: &BiliConfig,
        client: &Client,
        item: &Value,
    ) -> anyhow::Result<Content> {
        let dynamic_type = item["type"].as_str().unwrap();
        match dynamic_type {
            DYNAMIC_TYPE_FORWARD => {
                let raw_text_nodes = item["modules"]["module_dynamic"]["desc"]["rich_text_nodes"]
                    .as_array()
                    .unwrap();
                let texts = build_text_nodes(None, raw_text_nodes).await?;

                let orig_author = item["orig"]["modules"]["module_author"]["name"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let orig = Box::pin(Content::from_detail_json(bili, client, &item["orig"])).await?;

                Ok(Content::Forward {
                    texts,
                    original_author: orig_author,
                    original: Box::new(orig),
                })
            }
            DYNAMIC_TYPE_DRAW => {
                let opus = &item["modules"]["module_dynamic"]["major"]["opus"];
                let title = opus["title"].as_str().map(str::to_string);
                let raw_text_nodes = opus["summary"]["rich_text_nodes"].as_array().unwrap();
                let texts = build_text_nodes(title, raw_text_nodes).await?;

                let pics = match opus["pics"].as_array() {
                    Some(pics) => download_dynamic_images(pics, 740, 10).await?,
                    None => Vec::new(),
                };

                Ok(Content::Draw { texts, pics })
            }
            DYNAMIC_TYPE_WORD => {
                let opus = &item["modules"]["module_dynamic"]["major"]["opus"];
                let title = opus["title"].as_str().map(str::to_string);
                let raw_text_nodes = opus["summary"]["rich_text_nodes"].as_array().unwrap();
                let texts = build_text_nodes(title, raw_text_nodes).await?;

                Ok(Content::Word { texts })
            }
            DYNAMIC_TYPE_LIVE => {
                let live = &item["modules"]["module_dynamic"]["major"]["live"];
                let live_id = live["id"].as_i64().unwrap();
                let live_title = live["title"].as_str().unwrap().to_string();
                let live_cover_url =
                    format!("{}@203w_127h_1e_1c.webp", live["cover"].as_str().unwrap());
                let live_cover = download_image(live_cover_url).await?;

                Ok(Content::Live {
                    live_id,
                    live_title,
                    live_cover,
                })
            }
            _ => Err(anyhow!("不支持的动态类型: {}", dynamic_type)),
        }
    }
}

fn draw_dynamic(dynamic: &BiliDynamic) -> RgbaImage {
    let mut generator = PicGenerator::new(740, 10000);
    generator.draw_rectangle(0, 0, 10000, 740, WHITE);

    // 绘制用户头像
    let resized_face =
        imageops::resize(&dynamic.author.avatar_image, 100, 100, FilterType::Lanczos3);
    let circular_face = create_circular_image(&resized_face, 100);
    generator.draw_img_alpha(&circular_face, Some((50, 50)));
    // 绘制大会员下标
    if dynamic.author.vip {
        generator.draw_img_alpha(&RESOURCE.vip_image, Some((118, 118)));
    }
    generator.set_pos(175, 60);
    let uname_color = if dynamic.author.vip { PINK } else { BLACK };
    let ts = {
        // 东8区时间
        let tz = TimeZone::fixed(Offset::constant(8));
        let ts = Timestamp::from_second(dynamic.author.publish_timestamp).unwrap();
        let zoned_ts = ts.to_zoned(tz);
        strtime::format("%Y-%m-%d %H:%M", &zoned_ts).unwrap()
    };
    // 绘制用户名和动态时间戳
    generator.draw_text(
        &[&dynamic.author.uname],
        &[uname_color],
        &RESOURCE.text_normal_font,
        TEXT_SCALE,
        None,
    );
    generator.draw_text(&[&ts], &[GRAY], &RESOURCE.text_normal_font, TIP_SCALE, None);

    // 开始绘制动态内容
    generator.set_x(25);
    generator.set_row_space(10);

    draw_content(&mut generator, &dynamic.content);

    generator.crop_bottom();

    generator.into_image()
}

fn draw_content(generator: &mut PicGenerator, content: &Content) {
    match content {
        Content::Forward {
            texts,
            original_author,
            original,
        } => {
            let text_images = draw_content_image(
                texts,
                generator.width() - 50,
                TEXT_SCALE,
                EMOJI_SCALE,
                &RESOURCE,
            );
            for image in text_images {
                generator.draw_img_alpha(&image, None);
            }

            // 绘制原动态的灰色背景
            let y = generator.y();
            generator.draw_rectangle(0, y, generator.height() - y, generator.width(), LIGHT_GRAY);
            // 绘制原作者AT
            let orig_author_at = format!("@{}", original_author);
            generator.draw_text(
                &[&orig_author_at],
                &[DEEP_BLUE],
                &RESOURCE.text_normal_font,
                TEXT_SCALE,
                None,
            );
            // 绘制原动态内容
            draw_content(generator, original);
        }
        Content::Draw { texts, pics } => {
            let text_images = draw_content_image(
                texts,
                generator.width() - 50,
                TEXT_SCALE,
                EMOJI_SCALE,
                &RESOURCE,
            );
            for image in text_images {
                generator.draw_img_alpha(&image, None);
            }

            let num_pics_per_line = match pics.len() {
                1 => 1,
                2 | 4 => 2,
                _ => 3,
            };

            let image_lines: Vec<&[RgbaImage]> = pics.chunks(num_pics_per_line).collect();

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
        Content::Word { texts } => {
            let text_images = draw_content_image(
                texts,
                generator.width() - 50,
                TEXT_SCALE,
                EMOJI_SCALE,
                &RESOURCE,
            );
            for image in text_images {
                generator.draw_img_alpha(&image, None);
            }
        }
        Content::Live {
            live_id: _,
            live_title,
            live_cover,
        } => {
            generator.draw_text(
                &[live_title],
                &[BLACK],
                &RESOURCE.text_normal_font,
                TEXT_SCALE,
                None,
            );
            generator.draw_img(live_cover, None);
        }
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
    msg: Option<String>,     // When fail
    session: Option<String>, // When success
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

    let session_key = verify_response.session.unwrap();

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
    pictures: &[Value],
    image_area_width: u32,
    image_margin: u32,
) -> anyhow::Result<Vec<RgbaImage>> {
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
        let (src, height, width) = match (
            pic.get("url").and_then(Value::as_str).map(str::to_string),
            pic.get("height").and_then(Value::as_f64),
            pic.get("width").and_then(Value::as_f64),
        ) {
            (Some(src), Some(height), Some(width)) => (src, height, width),
            _ => {
                warn!("不合法的图片定义: {}，请检查API变动", pic);
                continue;
            }
        };

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

// I use this as a quick hack to look up dynamic details
#[tokio::test]
async fn test_get_detail() {
    let client = reqwest::Client::new();
    let dynamic_id: i64 = 729922047097962504;
    let sess_data = "SESSDATA";
    let detail_response: Value =  client
                        .get(format!("https://api.bilibili.com/x/polymer/web-dynamic/v1/detail?timezone_offset=-480&id={}&features=itemOpusStyle,opusBigCover,onlyfansVote", dynamic_id))
                        .header("COOKIE", format!("SESSDATA={}", sess_data))
                        .send()
                        .await.unwrap()
                        .json()
                        .await.unwrap();

    let s = serde_json::to_string_pretty(&detail_response).unwrap();
    println!("{}", s)
}
