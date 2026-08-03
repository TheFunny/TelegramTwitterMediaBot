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

环境变量：`TELOXIDE_TOKEN`（必填）、`PIXIV_REFRESH_TOKEN`、`BOT_ADMIN`、`EDIT_MESSAGE_TTL_SECONDS`、`RUST_LOG`、`WEBHOOK*`。

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
