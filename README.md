# bili-dynamic-spider

爬取用户动态并推送到qq，使用[mirai-http-v2](https://github.com/project-mirai/mirai-api-http)。

这个项目的功能是 [Starlwr/StarBot](https://github.com/Starlwr/StarBot) 的一个子集，用于个人需求，综合性的bilibili推送请参考原项目。

## 使用

1. 部署Mirai HTTP ([Starbot 部署文档前两步](https://bot.starlwr.com/depoly/document))
2. 在本项目根目录下创建`spider.toml`, 配置文件内容参考 [spider.toml.example](spider.toml.example)

```Toml
# 存储本地数据
[db]
path = "spider.db"

# mirai-http-api配置
[mirai]
http_url = "http://localhost:7827"
verify_key = "INITKEYLunaRyu"

# bilibili登陆账号，可使用[biliup](https://github.com/biliup/biliup-rs)登录获取
[bili]
sess_data = "SESSDATA"

# 监听目标
[[target]]
uid = 1234
interval_sec = 10
receiver_qq = 1234
sender_qq = 1234
```

3. `cargo run`


