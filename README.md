# TelegramXMediaBot

[English](README.en.md)

将 X / Twitter、Pixiv、Bluesky、Misskey (`misskey.io`) 和 Bilibili 动态链接转换为 Telegram 媒体消息的机器人，附带标题、作者与标签。

## 功能

- **多平台媒体抓取**：支持图片、视频与 GIF；Pixiv ugoira 动图与 Bluesky HLS 视频自动转码为 MP4；Bilibili 动态支持图片/动图（内嵌视频发送封面）
- **智能排版与处理**：多图按每批 10 张发送，超限图片自动压缩；纯文字帖以 HTML 文本发送，长正文自动收起为可折叠引用块
- **内联模式与缓存**：支持 `@bot <链接>` 内联发送（需 Referer 或本地转码的媒体在未缓存时跳过）；发送成功后缓存 Telegram `file_id`，重复链接直接本地重发
- **频道转发与模板**：支持绑定转发频道、转发前编辑 caption、套用自定义模板，以及按站点自定义 caption 格式
- **失败重试**：发送失败自动持久化重试，耗尽后通知失败原因

## 快速开始

### Docker（推荐）

```bash
cp .env.example .env   # 填写 TELOXIDE_TOKEN 等配置（文件内含注释）

# 直接运行镜像（轮询模式）
docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data \
  yoursfunny/telegram-twitter-media-bot:latest

# 或使用仓库自带编排（含 nginx-proxy + acme-companion）
docker compose up -d
```

### 本地运行

```bash
export TELOXIDE_TOKEN=<token>          # 必填
export PIXIV_REFRESH_TOKEN=<token>     # 可选，未设置则禁用 Pixiv
cargo run -p xmedia-bot
```

> 本地运行需安装 `ffmpeg`（Docker 镜像已内置）。

### Webhook 部署

`docker-compose.yml` 内置 `nginx-proxy` + `acme-companion`，实例配置统一写在 `.env` 中，Telegram 仅支持 `443 / 80 / 88 / 8443` 端口，开启 `WEBHOOK=true` 时须设置 `WEBHOOK_SECRET_TOKEN`。

- **域名部署**：将 DNS A 记录指向服务器；在 `.env` 设置 `VIRTUAL_HOST`、`WEBHOOK_URL` 与 `ACME_HOST`，并取消 `docker-compose.yml` 中 `ACME_HOST` 行注释即可自动签发和续期证书。

<details>
<summary>仅公网 IP 部署（Let's Encrypt shortlived + acme.sh）</summary>

需公网 80 端口可达（`http-01` 验证），证书有效期约 7 天，通过 `acme.sh` 自动续期：

1. 在 `docker-compose.yml` 增加 `acme-ip` 服务：
   ```yaml
   acme-ip:
     image: neilpang/acme.sh
     container_name: acme-ip
     command: daemon
     restart: always
     volumes:
       - certs:/acme.sh
       - html:/usr/share/nginx/html
       - /var/run/docker.sock:/var/run/docker.sock:ro
     networks: [proxy]
   ```
2. 首次签发（替换 `<SERVER_IP>`）：
   ```bash
   docker compose exec acme-ip acme.sh --issue --server letsencrypt \
     -d <SERVER_IP> --cert-profile shortlived --days 3 \
     --webroot /usr/share/nginx/html \
     --install-cert --cert-file /acme.sh/<SERVER_IP>.crt \
     --key-file /acme.sh/<SERVER_IP>.key \
     --reloadcmd "curl --unix-socket /var/run/docker.sock -X POST http://localhost/containers/nginx-proxy/kill?signal=HUP"
   ```
3. `.env` 设置 `VIRTUAL_HOST=<SERVER_IP>`、`DEFAULT_HOST=<SERVER_IP>`、`WEBHOOK_URL=https://<SERVER_IP>/`（无需 `WEBHOOK_CERT`）。

</details>

<details>
<summary>环境变量说明</summary>

| 变量 | 默认值 | 说明 |
|---|---|---|
| `TELOXIDE_TOKEN` | — | Bot token（必填） |
| `PIXIV_REFRESH_TOKEN` | — | Pixiv 刷新令牌；未设置则禁用 Pixiv |
| `TWITTER_AUTH_TOKEN` | — | x.com 登录态 Cookie `auth_token`，仅用于获取 NSFW 推文 |
| `BILIBILI_COOKIE` | — | B 站完整 Cookie 串（`SESSDATA=…; bili_jct=…`），仅出口 IP 持续风控（`-352`/412）时需要 |
| `BOT_ADMIN` | — | 管理员聊天 ID（逗号分隔），用于接收启停通知与管理员命令 |
| `EDIT_MESSAGE_TTL_SECONDS` | `86400` | 转发前编辑提示的过期时间（秒） |
| `LINK_CACHE_TTL_SECONDS` | `604800` | 链接缓存过期时间（秒，默认 7 天） |
| `CAPTION_QUOTE_TEXT_CHARS` | `200` | 正文达到该字符数时用可折叠引用块展示，`0` 关闭 |
| `DATA_DIR` | `data` | SQLite 数据文件 `task_queue.db` 目录 |
| `RUST_LOG` | `info,hyper_util=warn,reqwest=warn` | 日志级别（排障可用 `info,xmedia_bot=debug,x_media=debug`） |
| `TELOXIDE_PROXY` | — | HTTP 代理（如 `http://127.0.0.1:10808`）；**不可留空值**，不用请删除该行；容器内需手动加入 `docker-compose.yml` 并改用 `host.docker.internal` |
| `LOCAL_USER_ID` | `9001` | 容器内运行用户 UID |
| `WEBHOOK` | `false` | 设为 `true` 启用 Webhook 模式（默认轮询） |
| `WEBHOOK_LISTEN` / `WEBHOOK_PORT` | `0.0.0.0` / `8443` | Webhook 容器内监听地址与端口 |
| `WEBHOOK_URL` | — | 对外公网 HTTPS 地址（`https://域名/` 或 `https://IP/`） |
| `WEBHOOK_SECRET_TOKEN` | — | Webhook 校验令牌（`WEBHOOK=true` 时必填） |
| `WEBHOOK_CERT` | — | 自签名证书路径（仅用于 Telegram 侧校验） |
| `VIRTUAL_HOST` / `VIRTUAL_PORT` | — / `8443` | `nginx-proxy` 路由域名/IP 与转发端口 |
| `ACME_HOST` / `DEFAULT_EMAIL` | — | `acme-companion` 自动签发域名与通知邮箱 |
| `DEFAULT_HOST` | — | `nginx-proxy` 默认 vhost（纯 IP 访问时需要） |

</details>

## 命令

| 命令 | 说明 |
|---|---|
| `/start` | 显示简介与支持的站点 |
| `/help` | 显示全部命令及用法 |
| `/set_forward_channel <频道>` | 绑定自动转发频道（`@频道名` 或频道 ID） |
| `/remove_forward_channel` | 取消转发频道 |
| `/edit_before_forward` | 开关「转发前编辑」：发送后弹出编辑提示，回复改文案或点模板按钮，点 `Confirm` 转发、`Skip` 放弃 |
| `/set_template <名称>` | 回复一条包含 `[]` 的消息保存为模板（转发时 `[]` 替换为原帖链接） |
| `/remove_template <名称>` | 删除指定模板 |
| `/settings` | 查看当前聊天的转发、格式与模板配置 |
| `/set_format <站点> <格式>` | 自定义站点 caption 格式（站点：`twitter`/`bsky`/`pixiv`/`misskey`/`bilibili`；占位符：`{url}` `{author}` `{author_url}` `{title}` `{content}` `{tags}`；填 `-` 恢复默认） |
| `/test <链接>` | 发送媒体但不触发频道转发或编辑提示 |
| `/debug <链接>` | 仅解析链接并返回元数据与 caption 预览，不发送媒体 |
| `/clear_cache [链接]` | 清空链接缓存（仅管理员；省略链接清空全部） |
| `/bot_dict` | 查看当前聊天状态（调试用，仅管理员） |

> 链接解析仅在私聊生效（群聊会提示使用私聊或内联模式，频道静默）；命令在任意聊天可用。

## 备注

- 数据持久化于 `data/task_queue.db`（Docker 部署挂载 `./data:/app/data`）
- 测试：`cargo test --workspace`
