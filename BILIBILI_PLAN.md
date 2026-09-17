# Bilibili 动态支持：研究与实现记录

状态：已实现（`crates/x-media/src/site/bilibili/`）。本文记录上游调研、实测数据与最终设计；
长期契约以 `AGENTS.md` 为准。

范围：**只发动态里的图片与动图**。动态内嵌视频不发流，降级为封面图；`b23.tv` 短链不匹配；
视频页 / 番剧 / 直播间 / 专栏 / 音频均不支持。

---

## 1. 上游实现研究

### 1.1 nazurin（`nazurin/sites/bilibili/`，4 个文件 ~6 KB）

- 入口正则：`t\.bilibili\.com/(\d+)`、`t\.bilibili\.com/h5/dynamic/detail/(\d+)`、`bilibili\.com/opus/(\d+)`。
- 请求：`GET https://api.bilibili.com/x/polymer/web-dynamic/v1/detail?id={id}`，仅加 `Referer: https://t.bilibili.com/{id}`。
  **无 cookie、无 WBI 签名、无 `build` 参数**。
- 错误：`code == 4101147` → not found；`code != 0` 或缺 `data` → 报错。
- 媒体：只取 `item.modules.module_dynamic.major.draw.items[].src`；缩略图 `src + "@518w.jpg"`；
  `size` 字段单位是 **KB**。`major` 为空或 `draw.items` 为空 → "No image found"。
  **忽略视频、转发（forward）与纯文字动态**。
- caption：`"#" + module_author.name` + `module_dynamic.desc.text`，链接写死 `https://www.bilibili.com/opus/{id}`。

### 1.2 telegram-bili-feed-helper（`biliparser/provider/bilibili/`，9 个文件 ~57 KB）

- 9 个策略类（Video/Opus/Live/Audio/Read + Feed 基类 + Credential + api 工具）：门禁正则
  `bilibili\.com|b23\.tv|BV\w{10}|av\d+`，再分流，兜底 `client.head(url)` 跟随重定向后按子串分流。
- 动态：`GET /x/polymer/web-dynamic/desktop/v1/detail?id={id}&build=11605`（**单条，无分页**）；
  客户端带桌面 UA、随机 `buvid3={uuid}infoc`；登录态用 `bilibili-api-python` 的 `Credential`
  （Redis 持久化 `SESSDATA/bili_jct/buvid3/buvid4/ac_time_value/DedeUserID`，扫码登录）。
- **同样没有 WBI 签名 / appkey 签名**：playurl 用的是非 WBI 的 `/x/player/playurl`。
- 媒体：`major.type` 分派 —— DRAW 取全部 `items[].src`；ARCHIVE/PGC/ARTICLE/MUSIC/COMMON/LIVE
  只取一张 `cover`；FORWARD 取原动态作者/正文并递归进 `orig` 找媒体。
- 视频：仅独立 video 策略解析（`qn` 720P→480P→360P 试 durl，再退 DASH + ffmpeg 合并）；
  **动态内嵌视频只发封面**。
- 错误：要求 `status==200 && code==0`；风控 `-352`/`-412` 无特殊处理。

### 1.3 取舍

| 维度 | nazurin | bff | 本仓库 |
|---|---|---|---|
| 接口 | `v1/detail?id=` | `desktop/v1/detail?id=&build=` | `v1/detail?id=`（实测可用） |
| 认证 | 无 | buvid3 + SESSDATA | 默认匿名；可选 `BILIBILI_COOKIE` |
| WBI | 无 | 无 | 不实现（无需求） |
| 图片 | `major.draw.items` | 同 + forward 递归 | 同，加 `orig` 递归、`http→https`、`.gif → Animated` |
| 视频 | 完全忽略 | 动态内嵌视频发封面 | 发封面（不发流） |
| 短链 | 不匹配 | 跟随重定向 | 不匹配（多数短链是视频，会让"静默忽略"变成失败提示） |

---

## 2. 实测验证（2026-09-17，真实请求）

| 验证项 | 结果 |
|---|---|
| `v1/detail?id=`（无 cookie、UA `Mozilla/5.0`、带 Referer） | `200 {"code":0}` ✅ |
| 同上，不带 cookie 也不带 Referer | `200 {"code":0}` ✅（无强制鉴权） |
| bff 的 `bilibili_pc/…Electron/22.3.27` UA | `code:-352` ❌ → **不要抄它的 UA** |
| `desktop/v1/detail?build=11605` | `code:-352` ❌ |
| `feed/space?host_mid=`（用户时间线） | 首次成功、随后 `-352`，也见过 HTTP 412 → **不碰** |
| 不存在 / 已删除的动态 | `code:500` "Cannot read property 'only_fans' of undefined"（nazurin 的 4101147 已失效） |
| 非数字 id | `code:-400` param parsing failed |
| 图片 `i0.hdslb.com/bfs/new_dyn/*.jpg` | `HEAD 200 image/jpeg`，带/不带 Referer 均可；`+@518w.jpg` → 25–42 KB ✅ |
| `t.bilibili.com/h5/dynamic/detail/<id>` | `200` ✅ |
| `m.bilibili.com/dynamic/<id>` | `302 → t.bilibili.com/<id>` ✅ |
| `www.bilibili.com/opus/<id>` | `200`，转发动态 `302 → t.bilibili.com/<id>` ✅ |
| `b23.tv/BV1JTtt6JEZu` | `302 → www.bilibili.com/video/BV…`（视频） |
| `b23.tv/<无效码>` | **HTTP 200** + `{"code":-404}` ⚠️ 短链判定不能只看状态码 |
| `playurl`（仅调研用，未采用） | `fnval=1` 匿名给 durl：720P=9.18 MiB / 360P=2.97 MiB；`fnval=4048` 匿名 DASH 上限仅 480P |
| `dyn_archive` 字段 | 有 `aid/bvid/cover/title/duration_text`，**没有 `cid`**（所以发流要再来一次 `view` 请求） |
| **风控阶梯（同一 IP 连续请求后实测）** | ① 无 cookie → `-352`；② 仅 `buvid3` → 仍 `-352`；③ `buvid3`+`buvid4`（取自匿名 `/x/frontend/finger/spi`）→ **`code:0` 恢复**；④ 继续高频请求后 → 连同 buvid 一起 `-352`（此时只有登录 cookie 或换 IP） |

测试样本（live 测试用）：

| 样本 | id | 期望 |
|---|---|---|
| 图片动态（2 图 + 话题） | `1245284537985925159` | 2 个 `Illustration`，`{tags}` = `ALin出道20周年快乐` |
| 转发动态 | `1248982077447077907` | 媒体来自 `orig`（1 图），正文可含 `//@` |
| 视频动态 | `1248717597691609105` | 封面 1 张 `Illustration` |
| 纯文字动态 | `1246767523595026450` | `media` 为空 |

关键字段路径：

```
data.item.id_str
data.item.modules.module_author.{name,mid}
data.item.modules.module_dynamic.desc.text
data.item.modules.module_dynamic.topic.{id,name}            # 单话题，{tags} 来源
data.item.modules.module_dynamic.major.{draw.items[].src, archive.cover}
data.item.orig                                              # 转发时存在，结构与 item 相同
```

---

## 3. 实现

```
crates/x-media/src/site/bilibili/mod.rs        # re-export
crates/x-media/src/site/bilibili/interface.rs  # PATTERN / cache_key / enabled / is_retryable /
                                               # media_headers / BilibiliSite / fetch / code_error /
                                               # From<Item> for Fetched / caption / 12 单测 + 2 live
crates/x-media/src/site/bilibili/model.rs      # 纯 Deserialize DTO（全 Option）
```

- **正则**（同时用于分发、抽 id、缓存键，一个正则三用）：
  `^(?:https?://)?(?:www|t|m)\.bilibili\.com/(?:opus/|dynamic/|h5/dynamic/detail/)?(\d+)`
- **缓存键**：`bilibili:<动态 id>`；`source_url` 统一 `https://www.bilibili.com/opus/{id}`。
- **请求**：`GET /x/polymer/web-dynamic/v1/detail?id=` + `Referer: https://www.bilibili.com/`；
  `Cookie` 头按优先级取：`BILIBILI_COOKIE` → 缓存的设备 cookie（`GET /x/frontend/finger/spi` 取 `buvid3`/`buvid4`，
  进程内缓存一次；取不到就不带 cookie，仅 debug 日志）→ 无。指纹接口本身失败**不**让抓取失败。
  走共享 `CLIENT`（UA `Mozilla/5.0`，30s 超时，`TELOXIDE_PROXY` 透传）。
- **错误映射**：`0` → 成功；`-352/-412` 与 HTTP 412 → `Transient`（可重试，队列退避；首次记一条 warn 提示
  `BILIBILI_COOKIE`）；`500`/`4101147` → `NotFound`（永久）；其他 code → `Site`（永久）。
- **媒体**：
  - `major.draw.items[]` → 每张一张图（`http://` / `//` → `https://`，非 https 开头直接丢弃）；
    `.gif` → `Media::Animated`（`thumbnail_url` 留空，Telegram 自己取首帧——`@518w.jpg` 只对 jpg/webp 实测过），
    其余 → `Media::Illustration`（`thumbnail_url = url + "@518w.jpg"`，兼作超大时的降级 URL）。
  - `major.archive.cover` → 1 张 `Illustration`（视频不发流）。
  - 转发且自身无媒体 → 递归取 `orig` 的媒体；正文拼 `//@{原作者}:\n{原文}`。
  - 其他 major（PGC/ARTICLE/MUSIC/LIVE/COMMON）不建模 → 无媒体，走既有 "No media found"。
- **caption**（与 misskey 同形）：`{opus 链接}\n<a href="space.bilibili.com/{mid}">{name}</a>: {正文}`；
  `RenderData` 的 `{tags}` 来自话题名；正文由既有 `truncate_caption` 截断。
- **注册表**：`SITES` 末尾追加 → `/set_format` 白名单、链接缓存、启动校验、日志前缀全部自动生效。
- **bot 侧仅文案**：`handlers/commands.rs` 三处站点清单字符串 + `state.rs`/`handlers/mod.rs` 注释。

### 与原计划的偏差（及原因）

| 原计划 | 实际 | 原因 |
|---|---|---|
| `x/web-interface/view` + `playurl` 发视频 | 不做 | 需求收窄为图片/动图；视频只发封面 |
| `site/mod.rs` 加 `MAX_MEDIA_UPLOAD_BYTES` 常量 | 不加 | 没有视频尺寸决策就不需要该常量，避免跨 crate 耦合 |
| `b23.tv` 短链（跟随重定向） | 不匹配 | 多数短链指向视频，匹配后会把"静默忽略"变成用户的 "Failed to fetch media" |
| `validate()` 校验 cookie | 不做 | 匿名可用，cookie 失效不致命；校验要额外请求一个端点，收益低 |
| `media_headers` 给 hdslb 加 Referer | 返回 `None` | 实测图片与 durl 均无需 Referer（注释里记了这条验证） |
| 计划阶段认为设备 cookie 是 YAGNI，不实现 | **实现**（`buvid3`+`buvid4`） | 计划之后做了对照实验：同一 IP 上"无 cookie → -352、只有 buvid3 → -352、buvid3+buvid4 → code:0"，说明这是对本适配器主要失败模式的直接修复，而不是冗余保险 |

---

## 4. 测试与验证

- 单元（13）：正则匹配/拒绝/忽略短链、缓存键归一、图片映射（https 归一 + 缩略图 + `.gif → Animated`）、
  封面、转发取 `orig` 媒体与正文拼接、纯文字无媒体、caption 转义、业务 code 分类（可重试性）、URL 归一、
  设备 cookie 拼装。
- live（3，`#[ignore = "live network: …"]`）：设备 cookie 可取、图片动态 2 图、纯文字动态无媒体。
  CI 的 `live` job 已覆盖。动态接口被风控时这两条 live 测试打印 `skipping:` 并提前返回（与 pixiv 的
  token 门控同款约定），设备 cookie 那条仍会真实执行。
- 实测命令：
  `cargo run -p x-media --example fetch -- https://www.bilibili.com/opus/1245284537985925159`
  （输出 2 张 `https://i0.hdslb.com/…jpg` + `@518w.jpg` 缩略图 + 话题 tags）。
- 全套：`cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 全绿。

## 5. 已知限制

- 风控按 IP/请求量漂移，阶梯见 §2 最后一行：轻度靠设备 cookie 自愈，重度需 `BILIBILI_COOKIE` 或换 IP。
  被拦时按**可重试**失败处理（队列退避）+ 一条 warn，不会静默丢帖。
- 接口 schema 会漂移（`module_dynamic.major` 实测可为 `null` 而正文留在 `desc`）；DTO 全 `Option`，
  未知形态降级为"无媒体"，不 panic。
- 动态内嵌视频只发封面图（与 bff 同策略），不下载流。
- 纯文字动态复用既有 "No media found" 回复。
- `b23.tv` 短链不被匹配（见上表）。
