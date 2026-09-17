# TelegramXMediaBot

Telegram 机器人，将 X / Twitter、Pixiv、Bluesky、Misskey (misskey.io)、Bilibili 动态的帖子链接转换为媒体消息发送，附带帖子标题、作者与标签。

## 功能

- 私聊发送链接后自动抓取并发送图片、视频与 GIF，超量图片自动分批
- 纯文字帖提示无媒体；不支持的链接静默忽略
- 支持内联查询（`@机器人 <链接>`）
- 可绑定转发频道自动转发；支持转发前编辑 caption 与自定义模板
- 发送失败自动重试并持久化，重试耗尽后通知用户
- Pixiv ugoira 动图自动转码为 MP4；Bluesky 视频自动转码（HLS 流 → MP4）
- 超过 Telegram 尺寸/大小限制的图片自动压缩（保持原格式，必要时转 JPEG）
- 链接结果本地缓存：成功发送后缓存 Telegram file id 与 caption 等，再次收到相同链接直接本地重发，不再请求源站、不保存媒体文件（`LINK_CACHE_TTL_SECONDS` 控制过期，默认 7 天）

## 快速开始

```bash
# 必填：BotFather 的 token；可选：PIXIV_REFRESH_TOKEN（未设置则禁用 Pixiv）
export TELOXIDE_TOKEN=<token>
export PIXIV_REFRESH_TOKEN=<token>

cargo run -p xmedia-bot
```

Docker 部署（参考 `docker-compose.yml.example`）：

```bash
docker build -t tgxmb .
docker run --rm -d --name tgxmb --env-file .env -v ./data:/app/data tgxmb
```

环境变量：`TELOXIDE_TOKEN`（必填）、`PIXIV_REFRESH_TOKEN`、`BOT_ADMIN`、`EDIT_MESSAGE_TTL_SECONDS`、`LINK_CACHE_TTL_SECONDS`、`RUST_LOG`、`TELOXIDE_PROXY`、`WEBHOOK*`、`TWITTER_AUTH_TOKEN`（可选）、`BILIBILI_COOKIE`（可选）。

NSFW 推文：公开的 syndication 接口不返回敏感内容。设置 `TWITTER_AUTH_TOKEN`（登录 x.com 后浏览器 Cookie 里的 `auth_token` 值）后，bot 会仅在遇到 NSFW 推文时以登录态获取媒体；未设置则提示无媒体。

Bilibili 动态默认匿名抓取（无需登录，bot 会自动从 B 站的匿名指纹接口取 `buvid3`/`buvid4` 设备 cookie 以提高成功率）。若服务器出口 IP 被 B 站重度风控（日志里的 `risk control (-352)` 或 HTTP 412，且持续出现），设置 `BILIBILI_COOKIE`（登录后浏览器里整条 Cookie 串，如 `SESSDATA=…; bili_jct=…`）可恢复访问。当前只发送动态里的图片与动图，动态内嵌视频发送其封面。

### Webhook 部署（需要反向代理）

`docker-compose.yml.example` 内置了 [nginx-proxy](https://github.com/nginx-proxy/nginx-proxy) + [acme-companion](https://github.com/nginx-proxy/acme-companion) 反向代理编排，按部署环境二选一：

**有域名**
1. DNS A 记录指向服务器
2. compose 里设 `VIRTUAL_HOST`、`WEBHOOK_URL` 为域名，并取消注释 `ACME_HOST`（设为域名）
3. acme-companion 自动签发与续期证书，无需手动处理

**只有 IP**
Let's Encrypt 支持为公网 IP 签发证书（2026 年起可用，有效期约 7 天，须 `shortlived` profile）。用 [acme.sh](https://github.com/acmesh-official/acme.sh) 自动签发与续期，无需手动证书：

1. compose 里增加 acme-ip 服务（签发 + 每日检查自动续期）：
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
2. 首次签发（把 `<SERVER_IP>` 换成服务器公网 IP，IPv6 同样支持，多个 `-d` 可并列）：
   ```bash
   docker compose exec acme-ip acme.sh --issue --server letsencrypt \
     -d <SERVER_IP> --cert-profile shortlived --days 3 \
     --webroot /usr/share/nginx/html \
     --install-cert --cert-file /acme.sh/<SERVER_IP>.crt \
     --key-file /acme.sh/<SERVER_IP>.key \
     --reloadcmd "curl --unix-socket /var/run/docker.sock -X POST http://localhost/containers/nginx-proxy/kill?signal=HUP"
   ```
3. compose 里设 `VIRTUAL_HOST: '<SERVER_IP>'`、`WEBHOOK_URL: 'https://<SERVER_IP>/'`，无需 `WEBHOOK_CERT`。续期由 acme.sh daemon 自动完成（`--days 3` = 每 3 天续一次，证书 7 天有效有缓冲），续期成功后自动 HUP 通知 nginx-proxy 加载新证书。

   限制：证书约 7 天有效；验证仅支持 http-01/tls-alpn-01（80 端口必须公网可达）；不支持 DNS-01、私有 IP 与 IP 段；同一 IP 集合每 168 小时限签发 5 张。建议先用 `--server letsencrypt_test` 试签，成功后再切正式服务器。

Telegram 只接受 443/80/88/8443 端口。

<details>
<summary>环境变量说明</summary>

| 变量 | 说明 |
|---|---|
| `TELOXIDE_TOKEN` | Bot token（必填） |
| `PIXIV_REFRESH_TOKEN` | Pixiv 刷新令牌；未设置则禁用 Pixiv |
| `BILIBILI_COOKIE` | 可选的 B 站 Cookie 串（`SESSDATA=…; bili_jct=…`），仅在出口 IP 被持续风控时才需要（设备 cookie 由 bot 自动获取） |
| `BOT_ADMIN` | 管理员聊天 ID，逗号分隔；接收启动/停止通知 |
| `EDIT_MESSAGE_TTL_SECONDS` | 转发前编辑记录过期秒数，默认 86400 |
| `LINK_CACHE_TTL_SECONDS` | 链接结果缓存过期秒数，默认 604800（7 天） |
| `DATA_DIR` | 数据目录（SQLite 数据库 `task_queue.db` 所在目录），默认 `data`（相对工作目录，会自动创建） |
| `RUST_LOG` | 日志级别 |
| `TELOXIDE_PROXY` | HTTP 代理（如 `http://127.0.0.1:10808`）；同时作用于 Telegram Bot API 与站点抓取请求，网络受限环境（如 GFW）必需 |
| `LOCAL_USER_ID` | 容器内运行用户 UID，默认 9001 |
| `VIRTUAL_HOST` | 对外域名或 IP，nginx-proxy 按此路由 |
| `VIRTUAL_PORT` | bot 容器内监听端口，nginx-proxy 的转发目标 |
| `ACME_HOST` | 域名部署：设为域名时由 acme-companion 自动签发/续期证书 |
| `DEFAULT_HOST` | nginx-proxy 将未知 Host 的请求路由到该 vhost（IP 访问时需要） |
| `DEFAULT_EMAIL` | acme-companion 证书通知邮箱 |
| `WEBHOOK` | `true` 启用 webhook 模式（默认轮询） |
| `WEBHOOK_LISTEN` / `WEBHOOK_PORT` | bot 容器内监听地址/端口 |
| `WEBHOOK_URL` | 对外公网 HTTPS 地址（`https://域名/` 或 `https://IP/`） |
| `WEBHOOK_CERT` | 可选；自签名证书路径，仅用于 Telegram 侧验证（TLS 由反向代理终止） |
| `WEBHOOK_SECRET_TOKEN` | 更新校验令牌（`X-Telegram-Bot-Api-Secret-Token`） |

</details>

## 命令

| 命令 | 说明 |
|---|---|
| `/start` | 欢迎语 |
| `/help` | 查看全部命令及用法（即本文档的命令表） |
| `/set_forward_channel <频道>` | 设置转发频道，参数为 `@频道名` 或频道 ID；设置后发送的媒体消息会自动转发到该频道 |
| `/remove_forward_channel` | 取消转发频道 |
| `/edit_before_forward` | 开关「转发前编辑」：开启后，转发成功后 bot 会发一条提示消息，回复它可修改第一条转发消息的 caption（或点击模板按钮套用模板） |
| `/set_template <名称>` | 回复一条含 `[]` 的消息，将其保存为命名模板；转发时 `[]` 会被替换为原帖链接（配合「转发前编辑」使用） |
| `/set_format <站点> <格式>` | 自定义某站点的 caption 格式。站点：`twitter` / `bsky` / `pixiv` / `misskey` / `bilibili`。占位符：`{url}` `{author}` `{author_url}` `{title}` `{content}` `{tags}` |
| `/clear_cache [链接]` | 清空链接缓存（仅管理员）；带链接只清该条，否则清空全部 |
| `/bot_dict` | 查看当前聊天状态（调试用；仅管理员） |
| `/test <链接>` | 解析链接并发送媒体；不转发到频道、不弹转发前编辑提示（仅发送） |
| `/debug <链接>` | 调试：只解析链接并返回解析结果（站点、标题、作者、标签、媒体列表），不发送任何媒体 |

链接处理仅限私聊；命令在任意聊天可用。

## 备注

- 数据持久化于 `data/task_queue.db`，compose 部署使用 bind mount `./data`（保持目录形式便于备份）
- 运行环境需安装 ffmpeg（Docker 镜像已内置）
- 测试：`cargo test --workspace`
