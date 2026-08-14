# 站点适配器重构方案：让新增站点变成"新模块 + 注册一行"

> 状态：设计稿（未实施）。目标：把"加一个新站点"从改 8-9 处收敛到 3 处，
> 并让站点身份、重试策略、下载 header 等站点能力归位到站点模块自身。
> 本文只改文档，不动代码；每阶段均可独立合入、独立回滚。

---

## 1. 现状摩擦清单

以现有三站（twitter / bsky / pixiv）为基线，新增第 4 个站点（代号 `example`）
今天需要触碰的位置：

| # | 位置（当前行号） | 改动 | 必改? |
|---|---|---|---|
| 1 | 新目录 `crates/x-media/src/site/example/{mod,interface,model}.rs` | 新模块 | 必改 |
| 2 | `site/mod.rs:354-365` `fetch_once` | 加一个 `if` 分派分支 | 必改 |
| 3 | `site/mod.rs:167-178` `cache_key` | 加一个 `if` 分支 + 约定 key 前缀 `"example:..."` | 必改 |
| 4 | `site/mod.rs:53-63` `site_name()` | 加一个 URL `contains` 嗅探分支 | 必改 |
| 5 | `handlers.rs:405` `SetFormat` 白名单 | `["twitter","bsky","pixiv"]` 加字符串 | 必改 |
| 6 | `config.rs` / `main.rs:74-84` | 仿 pixiv 加启动校验（token、`disable()`） | 视站点 |
| 7 | `site/mod.rs:312-326` `fetch_error_is_retryable` | 若重试策略特殊，改中央分类函数 | 视站点 |
| 8 | `site/mod.rs:377/391/430` 三个下载函数 | 若媒体有防盗链，加 header（现在是硬编码 pximg 判断） | 视站点 |
| 9 | `site/mod.rs:16,180-197` `FetchError` | 若错误类型特殊，加嵌套 variant（仿 `Pixiv(PixivError)`） | 视站点 |

**根因**：仓库里没有"站点"这个实体。站点的四类能力——URL 识别（PATTERN +
cache_key）、抓取、重试策略、下载 header——分别散落在中央 if 链、URL 字符串嗅探、
魔法字符串 key 和 bot crate 的白名单里。`AGENTS.md` 现行约定 "no trait, no enum
dispatch" 是刻意的简单性选择；本方案的目标是在**不推翻它精神的前提下**收敛摩擦，
并在阶段 3 提供完整的 trait 注册表选项。

## 2. 目标架构

```
crates/x-media/src/site/mod.rs
  ├─ SITES: LazyLock<Vec<Box<dyn Site>>>      ← 注册表（唯一的"站点列表"）
  ├─ find_site(url) / fetch(url) / cache_key(url) / site_ids()
  └─ 通用类型：Fetched { site_id, ... } / FetchError（通用类 + Site 变体）
        │
        ├─ site/twitter/{mod,interface,model}.rs  impl Site
        ├─ site/bsky/…                            impl Site
        └─ site/pixiv/…                           impl Site  (download_headers: pximg Referer)
                                                    (validate: token 校验)

crates/xmedia-bot
  ├─ handlers.rs  SetFormat 白名单 ← x_media::site::ids()（不再写死）
  ├─ handlers.rs  site 格式查找 ← fetched.site_id（缓存/新鲜两条路径同口径）
  └─ main.rs      启动校验 ← site::validate_all()（不再特判 pixiv）
```

## 3. 分阶段迁移

每个阶段是一个独立提交，保持 `cargo fmt` / `cargo clippy -- -D warnings` /
`cargo test --workspace` 全绿；行为完全不变，只挪代码、不换语义。

### 阶段 1：站点身份单一来源（低风险，推荐先做）

**动机**：同一概念目前有两个来源——缓存命中路径用 `key.split(':').next()`
（`handlers.rs:639`），新鲜抓取路径用 `fetched.site_name()`（`handlers.rs:724`）；
`site_name()` 又是对 `source_url` 的 `contains` 字符串嗅探，还有 `"unknown"`
兜底分支。

**改动**：

1. `site/mod.rs`：`Fetched` 增加字段 `site_id: &'static str`（由各站点的
   `impl From<SiteStruct> for Fetched` 填充；`empty_fetched` 同步填）。
   `Fetched::site_name()` 改为 `return self.site_id`（保留方法名，删除
   `source_url.contains` 嗅探与 `"unknown"` 分支）。
2. `site/mod.rs`：新增 `pub fn site_id_from_key(key: &str) -> &'static str`
   （解析 `"example:..."` 前缀，未知前缀返回 `"unknown"`），bot 缓存命中路径改用它，
   与 `fetched.site_id` 口径统一。
3. `handlers.rs:405`：`SetFormat` 白名单改为 `x_media::site::ids()`——阶段 1 先实现
   `ids()` 为 `["twitter","bsky","pixiv"]` 的常量函数（数据源仍集中，行为不变），
   阶段 3 再改为遍历注册表。
4. `twitter/interface.rs:48-60` / `bsky` / `pixiv` 的 `From<SiteStruct> for Fetched`
   各补 `site_id` 字段。

**风险**：低。纯增量字段；`site_name()` 语义不变（测试 `pixiv/interface.rs:355`
  已断言 `"pixiv"`）。
**验证**：现有全部单测；`cache_key_normalizes_domain_variants` 等不变。
**回滚**：revert 该提交。

### 阶段 2：站点能力下沉（不引入 trait，静态分派）

**动机**：把"每个站点自己才知道"的逻辑搬回站点模块，中央只做迭代。这是
`AGENTS.md` 现有约定（无 trait）与完整注册表之间的折中，可独立交付。

**改动**：每个站点模块新增并 `mod.rs` 重新导出：

```rust
// site/twitter/interface.rs（bsky/pixiv 同构）
pub fn cache_key(url: &str) -> Option<String>;   // 用自身 PATTERN，返回 "twitter:<id>"
pub fn is_retryable(err: &FetchError) -> bool;   // 默认 Http|Transient；pixiv 覆盖 PixivError 分支
pub fn media_headers(url: &str) -> Option<Vec<(&'static str, String)>>;
                                                 // pixiv: url 含 "pximg.net" → Referer
```

`site/mod.rs` 相应改为迭代三站：

- `cache_key`：逐个调 `site::cache_key`，不再自己写 key 格式；
- `fetch_error_is_retryable`：删除，`fetch()` 重试循环改调 `current_site::is_retryable`
  （`fetch_once` 已能确定站点，把站点传下去）；
- `media_size` / `download_media_limited` / `download_media_to_file` 里的
  `pximg.net → Referer` 硬编码删除，改为遍历 `SITES`（阶段 2 是遍历
  `[twitter, bsky, pixiv]` 静态列表）取 `media_headers(url)` 合并。

**注意**：Referer 判定依据是媒体 URL 的 host（`pximg.net`），**不是**站点
PATTERN（pixiv 的 PATTERN 只匹配 `pixiv.net/artworks/...`），所以 `media_headers`
不能挂在 PATTERN 匹配上，必须按 URL 独立匹配——这正是把它做成独立函数的原因。

**风险**：中。下载函数签名不变，行为必须逐字节不变；新增单元测试覆盖
`media_headers("https://i.pximg.net/...") == Some(Referer)` 与
`cache_key` 等价性（对全部既有用例断言新旧结果一致）。
**回滚**：revert。

### 阶段 3：Site trait + SITES 注册表（完整方案，可选）

**动机**：加站点时 bot crate 与中央分派零改动；站点列表成为唯一注册点。

**新增**（`site/mod.rs`）：

```rust
pub trait Site: Send + Sync {
    fn id(&self) -> &'static str;
    fn pattern(&self) -> &'static Regex;
    fn enabled(&self) -> bool;
    fn cache_key(&self, url: &str) -> Option<String>;   // 默认: id + 捕获组1
    // 原生 AFIT（async fn in trait，Rust 1.75+）。dyn 调用需 Send future：
    // 见下方 "async 形态" 选项 (c)，必要时反糖为
    // `fn fetch_from_url(&self, url: &str) -> impl Future<Output = ...> + Send + '_`
    async fn fetch_from_url(&self, url: &str) -> Result<Fetched, FetchError>;
    fn is_retryable(&self, err: &FetchError) -> bool;   // 默认: Http|Transient
    fn media_headers(&self, url: &str) -> Option<Vec<(&'static str, String)>>; // 默认: None
    async fn validate(&self) -> Result<(), String>;     // 默认: Ok(())
}

static SITES: LazyLock<Vec<Box<dyn Site>>> = LazyLock::new(|| vec![
    Box::new(twitter::TwitterSite), Box::new(bsky::BskySite), Box::new(pixiv::PixivSite),
]);
```

- `fetch_once` → `find_site(url)`（首个 PATTERN 命中且 `enabled()` 的站点）
  → `site.fetch_from_url(url).await`；
- `cache_key` / `site_ids()` / `media_headers` / `validate_all()` 全部遍历 `SITES`；
- `fetch_error_is_retryable` 删除，重试判定走 `site.is_retryable`；
- `main.rs:74-84` 的 pixiv 特判 → `site::validate_all()`（pixiv 的 `validate` 失败时
  内部调用现有 `pixiv::disable()`，行为保持）；
- 保留各站点的 `PATTERN`/`enabled()`/`fetch_from_url()` 顶层导出（兼容现有
  `fetch_once` 及测试），trait 只是包一层薄壳。

**async 形态**：三个选择，**优先 (c)**。

- **(c) 原生 AFIT（async fn in trait，首选）**：Rust 1.75 起稳定且支持 dyn 分派，
  仓库是 recent stable + edition 2024、无 MSRV pin，完全可用。零新依赖，trait/impl
  都是原生 `async fn` 语法。两点注意：
  - **静态分派调用点不产生 box**（`SITES` 之外若还有直接调 `TwitterSite::fetch_from_url`
    的路径，零分配）；dyn 调用时编译器按需 box，这是 dyn 分派的固有成本。
  - **dyn 上要 Send future 必须反糖**：直接 `async fn` 在 `dyn Site` 上不保证
    future 是 Send（URL/队列工人 `tokio::spawn` 需要），要写成
    `fn fetch_from_url(&self, url: &str) -> impl Future<Output = Result<Fetched, FetchError>> + Send + '_`。
    反糖后方法仍可 `site.fetch_from_url(url).await` 调用，语义不变。
- **(b) `async-trait`**：语法与 (c) 相同，但新增一个依赖（唯一新包；
  proc-macro2/quote/syn 树里已有），且**无论静态还是 dyn 调用都 box**（生成
  `BoxFuture`）。适用场景是 MSRV < 1.75 或需要 `?Send` 的 trait，本仓库都不占。
- **(a) 手写 `Pin<Box<dyn Future>>`**：零新依赖、静态分派也 box；签名噪音大，
  且"借 `&self`/参数却写成 `'static`"这类生命周期错误要自己防（async-trait/AFIT
  自动处理）。

结论：先按 (c) 设计，trait 里直接写 `async fn`；若将来工具链约束出现（MSRV 下调）
再降级到 (b)，实现方签名几乎不用改（async fn ↔ `#[async_trait] async fn`）。

**风险**：中。动中央分派，但每站点行为不变；注册表迭代 + `find_site` 补单测
（`fetch`/`cache_key` 对既有 URL 集合的结果与阶段 2 完全一致）。
**回滚**：revert。

### 阶段 4：FetchError 泛化（可选，配合阶段 3）

**动机**：`FetchError::Pixiv(PixivError)`（`site/mod.rs:16,184,241-245`）是站点特有
错误嵌进通用枚举；第 4 个站点要么再加变体，要么用泛化变体。

**改动**：`FetchError` 增加 `Site { site: &'static str, error: Box<dyn std::error::Error + Send + Sync> }`，
`Pixiv(PixivError)` 变体保留但内部迁移到 `Site`（或直接替换并更新
`is_retryable`/`Display`/`source()` 与测试）。重试判定在阶段 3 已归站点，
中央枚举只剩通用类（Http/Json/NotFound/Blocked/Sensitive/TooLarge/Transient/Io）。

**风险**：中。`Display`/`source()`/`From<PixivError>` 与 `fetch_error_is_retryable`
测试（`site/mod.rs:480-522`）需同步。
**回滚**：revert。

### 阶段 5：收尾

- 更新 `AGENTS.md` 的 "Site adapter convention" 段：写新约定（注册表 + `impl Site` +
  每站点 `cache_key`/`is_retryable`/`media_headers`），删除 "no trait" 表述；
- `examples/fetch.rs` 不变（走 `site::fetch`）；
- 新增站点 checklist 见 §4。

## 4. 重构后新增站点 checklist

```
1. crates/x-media/src/site/example/{mod,interface,model}.rs   // 新模块
2. impl Site for ExampleSite 并注册进 SITES                   // 注册一行
3. （可选）token 读取 + validate() 实现                         // 启动校验自动生效
── bot crate 零改动 ──
```

对比现状的 8-9 处，bot crate 完全不碰：`SetFormat` 白名单、格式查找口径、
缓存 key、启动校验全部自动跟随注册表。

## 5. 权衡与明确不做的事

- **不做**：Media 类型扩展（`media.rs` + `MediaItemPayload` + `CachedMediaKind` +
  send.rs 约 10+ 处 match 的 blast radius）——这是"新增媒体类型"的摩擦，与"新增
  站点"正交，优先级低，保持现状。
- **不做**：DI/全局注入改造（`CHAT_STORE`/`TASK_QUEUE`/`CONFIG` 的 `LazyLock` 静态
  模式是仓库惯例，与站点扩展无关）。
- **不做**：schema 迁移——新站点只产生新的 cache key 前缀与 `message_format` JSON
  key，`link_cache`/`chat_state` 表结构均无需变化。
- **代价**：阶段 3 引入 `dyn Site` 与 trait 方法（async 形态选 (c) 原生 AFIT，零新依赖、
  静态分派零 box，见 §3）；`Send` 约束前移到 trait 边界，站点 impl 的 future 必须
  Send（现仅在各 `tokio::spawn` 点检查，重构后在 impl 处即报错，提前暴露问题）。
  若站点数量长期 ≤5 且无新增迹象，阶段 2 的折中方案已够用，阶段 3/4 可无限期推迟。

## 6. 建议的提交序列

| 阶段 | 提交消息（建议） |
|---|---|
| 1 | `refactor(site): carry site_id on Fetched; unify cache-key site lookup` |
| 2 | `refactor(site): move cache_key/is_retryable/media_headers into site modules` |
| 3 | `refactor(site): introduce Site trait and SITES registry` |
| 4 | `refactor(site): genericize FetchError::Site` |
| 5 | `docs: update site adapter convention in AGENTS.md` |

每阶段独立合入、独立回滚；阶段 2 完成后即可认为"加站点"摩擦已收敛，
3/4 为可选深化。
