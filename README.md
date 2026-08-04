# TelegramXMediaBot

Telegram 机器人，将 X / Twitter、Pixiv、Bluesky 的帖子链接转换为媒体消息发送，附带帖子标题、作者与标签。

## 功能

- 私聊发送链接后自动抓取并发送图片、视频与 GIF，超量图片自动分批
- 纯文字帖提示无媒体；不支持的链接静默忽略
- 支持内联查询（`@机器人 <链接>`）
- 可绑定转发频道自动转发；支持转发前编辑 caption 与自定义模板
- 发送失败自动重试并持久化，重试耗尽后通知用户
- Pixiv ugoira 动图自动转码为 MP4

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

环境变量：`TELOXIDE_TOKEN`（必填）、`PIXIV_REFRESH_TOKEN`、`BOT_ADMIN`、`EDIT_MESSAGE_TTL_SECONDS`、`RUST_LOG`、`WEBHOOK*`、`TWITTER_AUTH_TOKEN`（可选）。

NSFW 推文：公开的 syndication 接口不返回敏感内容。设置 `TWITTER_AUTH_TOKEN`（登录 x.com 后浏览器 Cookie 里的 `auth_token` 值）后，bot 会仅在遇到 NSFW 推文时以登录态获取媒体；未设置则提示无媒体。

### Webhook 部署（需要反向代理）

`docker-compose.yml.example` 内置了 [nginx-proxy](https://github.com/nginx-proxy/nginx-proxy) + [acme-companion](https://github.com/nginx-proxy/acme-companion) 反向代理编排，按部署环境二选一：

**有域名**
1. DNS A 记录指向服务器
2. compose 里设 `VIRTUAL_HOST`、`LETSENCRYPT_HOST` 为域名，`WEBHOOK_URL` 设为 `https://域名/`
3. 证书自动签发与续期，无需手动处理

**只有 IP**
1. 生成自签证书（PEM 格式，见第 3 步）：
   `openssl req -x509 -newkey rsa:2048 -nodes -days 365 -keyout nginx-certs/default.key -out nginx-certs/default.crt`
2. compose 里 nginx-proxy 设 `DEFAULT_HOST`，bot 设 `WEBHOOK_CERT: './cert/cert.pem'`（须与代理所服务的为同一张证书）
3. 证书必须是 PEM 编码（ASCII BASE64，以 `-----BEGIN CERTIFICATE-----` 开头）—— Telegram 只接受该格式；若现有证书是 DER 二进制，转换：
   `openssl x509 -in cert.der -inform DER -out cert.pem -outform PEM`
   （私钥同理：`openssl rsa -in key.der -inform DER -out key.pem -outform PEM`）

Telegram 只接受 443/80/88/8443 端口。

<details>
<summary>环境变量说明</summary>

| 变量 | 说明 |
|---|---|
| `TELOXIDE_TOKEN` | Bot token（必填） |
| `PIXIV_REFRESH_TOKEN` | Pixiv 刷新令牌；未设置则禁用 Pixiv |
| `BOT_ADMIN` | 管理员聊天 ID，逗号分隔；接收启动/停止通知 |
| `EDIT_MESSAGE_TTL_SECONDS` | 转发前编辑记录过期秒数，默认 86400 |
| `RUST_LOG` | 日志级别 |
| `LOCAL_USER_ID` | 容器内运行用户 UID，默认 9001 |
| `VIRTUAL_HOST` | 对外域名或 IP，nginx-proxy 按此路由 |
| `VIRTUAL_PORT` | bot 容器内监听端口，nginx-proxy 的转发目标 |
| `LETSENCRYPT_HOST` | 设为域名时由 acme-companion 自动签发/续期证书 |
| `DEFAULT_HOST` | nginx-proxy 将未知 Host 的请求路由到该 vhost（IP 访问时需要） |
| `DEFAULT_EMAIL` | acme-companion 证书通知邮箱 |
| `WEBHOOK` | `true` 启用 webhook 模式（默认轮询） |
| `WEBHOOK_LISTEN` / `WEBHOOK_PORT` | bot 容器内监听地址/端口 |
| `WEBHOOK_URL` | 对外公网 HTTPS 地址（`https://域名/`） |
| `WEBHOOK_CERT` | 自签证书路径（仅 IP 路径需要，须为 PEM 且与代理所服务的一致） |
| `WEBHOOK_SECRET_TOKEN` | 更新校验令牌（`X-Telegram-Bot-Api-Secret-Token`） |

</details>

## 命令

| 命令 | 说明 |
|---|---|
| `/set_forward_channel <频道>` | 设置转发频道 |
| `/remove_forward_channel` | 取消转发频道 |
| `/edit_before_forward` | 开关转发前编辑 |
| `/set_template <名称>` | 将回复的消息（含 `[]`）保存为模板 |
| `/set_format <站点> <格式>` | 自定义 caption 格式（占位符 `{url}` `{title}` `{tags}` 等） |
| `/bot_dict` | 查看聊天状态 |

链接处理仅限私聊；命令在任意聊天可用。

## 备注

- 数据持久化于 `data/task_queue.db`，容器部署需挂载该目录
- 运行环境需安装 ffmpeg（Docker 镜像已内置）
- 测试：`cargo test --workspace`
