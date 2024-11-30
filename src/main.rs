mod config;

use anyhow::Context;
use config::{get_config_from_file, BiliConfig, Config, MiraiConfig, TargetConfig};
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    for h in handles {
        h.await.unwrap().unwrap();
    }

    Ok(())
}

async fn run_target(
    mirai: MiraiConfig,
    bili: BiliConfig,
    target: TargetConfig,
) -> anyhow::Result<()> {
    println!(
        "开始监听b站用户UID {} 的动态并发送给 QQ 号{}",
        target.uid, target.receiver_qq
    );

    println!(
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
        .await
        .context("Request /verify")?
        .json()
        .await
        .context("Parse response from /verify")?;

    let session_key = verify_response.session;

    let bind_request = BindRequest {
        session_key: session_key.clone(),
        qq: target.sender_qq,
    };

    let bind_response: BindResponse = client
        .post(format!("{}/bind", mirai.http_url))
        .json(&bind_request)
        .send()
        .await
        .context("Request /bind")?
        .json()
        .await
        .context("Parse response from /bind")?;

    assert_eq!(0, bind_response.code, "bind failed: {}", bind_response.msg);

    println!("成功获取MIRAI会话并绑定发送者QQ");

    let mut last_ts = jiff::Timestamp::now();

    println!(
        "开始监听UID {} 自 {} 之后的动态消息",
        target.uid,
        print_ts(last_ts, tz.clone()),
    );

    let cookie = format!("SESSDATA={}", bili.sess_data);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(target.interval_sec)).await;

        let response: serde_json::Value = client
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

        let code = response
            .get("code")
            .expect("unreachable")
            .as_i64()
            .expect("unreachable");

        if code != 0 {
            println!("获取用户动态失败: {:?}", response);
            continue;
        }

        let cards = response
            .as_object()
            .unwrap()
            .get("data")
            .unwrap()
            .as_object()
            .unwrap()
            .get("cards")
            .unwrap()
            .as_array()
            .unwrap();

        for card in cards.into_iter().rev() {
            let desc = card.get("desc").unwrap();
            let uname = desc
                .get("user_profile")
                .unwrap()
                .get("info")
                .unwrap()
                .get("uname")
                .unwrap()
                .as_str()
                .unwrap();
            let dynamic_id = desc.get("dynamic_id").unwrap().as_i64().unwrap();

            let ts = jiff::Timestamp::from_second(desc.get("timestamp").unwrap().as_i64().unwrap())
                .expect("Malformed dynamic timestamp");

            if ts <= last_ts {
                continue;
            }

            last_ts = ts;

            let dynamic_type = desc.get("type").unwrap().as_i64().unwrap();
            match dynamic_type {
                2 | 4 => {
                    // construct message chain
                    let mut messages = Vec::new();

                    messages.push(Message::Plain {
                        text: format!(
                            "{} 发表了新动态:\n\n{}\n\nhttps://t.bilibili.com/{}\n\n",
                            uname,
                            print_ts(ts, tz.clone()),
                            dynamic_id,
                        ),
                    });

                    // get dynamic details
                    let get_detail_response: serde_json::Value =  client
                        .get(format!("https://api.bilibili.com/x/polymer/web-dynamic/v1/detail?timezone_offset=-480&id={}&features=itemOpusStyle,opusBigCover,onlyfansVote", dynamic_id))
                        .header("COOKIE", "SESSDATA=6ad06723%2C1748386869%2C9e5990b2")
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();

                    let modules = get_detail_response
                        .get("data")
                        .unwrap()
                        .get("item")
                        .unwrap()
                        .get("modules")
                        .unwrap()
                        .get("module_dynamic")
                        .unwrap()
                        .get("major")
                        .unwrap()
                        .get("opus")
                        .unwrap();

                    let title = modules.get("title").map(|v| v.as_str()).flatten();

                    if let Some(title) = title {
                        messages.push(Message::Plain {
                            text: title.to_string(),
                        });
                        messages.push(Message::Plain {
                            text: "\n".to_string(),
                        });
                    }

                    if let Some(summary) = modules.get("summary") {
                        let text = summary.get("text").unwrap().as_str().unwrap();
                        messages.push(Message::Plain {
                            text: text.to_string(),
                        });
                    }

                    println!("监听到 {} 新动态，发送qq消息", uname,);

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
                        println!(
                            "发送qq消息失败: {}: {}",
                            send_response.code, send_response.msg
                        );
                    }
                }
                _ => (),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum Message {
    Plain { text: String },
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
