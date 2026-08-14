# 站点适配器重构方案：让新增站点变成"新模块 + 注册一行"

> 状态：**已实施**（阶段 1-5，提交 `7ca8fd1` / `5e23916` / `bf4e615` / `5679a8c` +
> 本文档收尾）。目标：把"加一个新站点"从改 8-9 处收敛到 3 处，并让站点身份、
> 重试策略、下载 header 等站点能力归位到站点模块自身。实施过程中的关键偏差
> （async 形态）见 §3 的 "async 形态" 段——原生 AFIT 实测不可用于 dyn 分派，
> 最终采用手写 `BoxFuture`（`SiteFuture` 别名）。

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

**新增**（`site/mod.rs`，按实施后的实际形态）：

```rust
/// Boxed, Send future produced by a Site async method. Boxed so the trait
/// stays dyn-compatible; Send because URL/queue workers tokio::spawn these.
type SiteFuture<'a, T, E = FetchError> =
    Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

pub trait Site: Send + Sync {
    fn id(&self) -> &'static str;
    fn pattern(&self) -> &'static Regex;
    fn enabled(&self) -> bool { true }                 // 默认: true
    fn cache_key(&self, url: &str) -> Option<String>;
    fn fetch_from_url<'a>(&'a self, url: &'a str) -> SiteFuture<'a, Fetched>;
    fn is_retryable(&self, err: &FetchError) -> bool;  // 默认: Http|Transient
    fn media_headers(&self, url: &str) -> Option<Vec<(&'static str, String)>>; // 默认: None
    fn validate(&self) -> SiteFuture<'static, (), String>;  // 默认: Ok(())
}

static SITES: LazyLock<Vec<Box<dyn Site>>> = LazyLock::new(|| vec![
    Box::new(twitter::TwitterSite), Box::new(bsky::BskySite), Box::new(pixiv::PixivSite),
]);
```

- `fetch` → `find_site(url)`（注册表中首个 PATTERN 命中且 `enabled()` 的站点，
  返回 `&'static dyn Site`）→ `site.fetch_from_url(url).await`；
- `cache_key` / `site_ids()` / `site_id_from_key()` / `apply_media_headers()` /
  `validate_all()` 全部遍历 `SITES`；`validate_all` 返回失败列表，pixiv 的
  `Site::validate` 失败时自行 `disable()`；
- `match_site`/`SiteKind`（阶段 2 的静态分派）与中央 `fetch_error_is_retryable`
  删除，重试判定走 `site.is_retryable`；
- `main.rs` 的 pixiv 特判 → `site::validate_all()` + 通用失败通知；
- 保留各站点的 `PATTERN`/`enabled()`/`fetch_from_url()` 顶层导出（兼容既有
  测试），trait impl 只是薄壳。

**async 形态**（实施结论）：**原生 AFIT 不可行**。

- 实测（rustc 1.95.0，edition 2024）：trait 里写 `async fn` 报
  "method is `async`"（非 dyn 兼容）；写反糖 `-> impl Future<...> + Send + '_`
  报 "references an `impl Trait` type in its return type"（同样非 dyn 兼容）。
  即：**RPITIT/AFIT 目前无法用于 `Vec<Box<dyn Site>>` 注册表**，与早期设计的
  判断相反。
- **采用 (a) 手写 `Pin<Box<dyn Future + Send + '_>>`**（`SiteFuture` 别名）：
  零新依赖、dyn 兼容、future 保证 Send。签名噪音靠别名缓解；生命周期坑因
  站点是无状态单元结构体 + `'a` 同时约束 `&self` 与 `url` 而完全可控
  （future 只借用调用域内的 url）。
- **(b) `async-trait`** 仍是可行备选（语法更干净、同样 box），但新增依赖；
  本仓库采用 (a) 后无需引入。
- 若未来 Rust 稳定版放开 RPITIT 的 dyn 兼容，可再评估换回原生 `async fn`。

**风险**：中。动中央分派，但每站点行为不变；注册表迭代 + `find_site` 补单测
（`fetch`/`cache_key` 对既有 URL 集合的结果与阶段 2 完全一致）。
**回滚**：revert。

### 阶段 4：FetchError 泛化（已实施）

**改动**：`FetchError` 新增 `Site { site: &'static str, error: Box<dyn std::error::Error + Send + Sync> }`
变体（`Display`/`source()` 同步）。**`Pixiv(PixivError)` 变体保留**（未迁移）——
它已有完整的 `Display`/`source()`/`is_retryable` 处理，替换纯属 churn。`Site`
变体默认永久性（各站点 `is_retryable` 都不匹配它）；需要可重试站点错误的站点
应自行转换为 `Http`/`Transient` 再返回。

**风险**：低（纯增量变体）。测试：`site_error_variant_displays_and_sources`。

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
- **代价**：阶段 3 引入 `dyn Site` 与 boxed future 签名（`SiteFuture`，见 §3）；
  `Send` 约束前移到 trait 边界，站点 impl 的 future 必须 Send（现仅在各
  `tokio::spawn` 点检查，重构后在 impl 处即报错，提前暴露问题）。
  若站点数量长期 ≤5 且无新增迹象，阶段 2 的折中方案已够用；本次已按完整方案
  实施到阶段 4。

## 6. 提交序列（已按此实施）

| 阶段 | 提交 | hash |
|---|---|---|
| 1 | `refactor(site): carry site_id on Fetched; unify cache-key site lookup` | `7ca8fd1` |
| 2 | `refactor(site): move cache_key/is_retryable/media_headers into site modules` | `5e23916` |
| 3 | `refactor(site): introduce Site trait and SITES registry` | `bf4e615` |
| 4 | `refactor(site): genericize FetchError::Site` | `5679a8c` |
| 5 | `docs: update site adapter convention in AGENTS.md` | 本文档收尾提交 |

每阶段独立合入、独立回滚；阶段 2 完成后"加站点"摩擦已收敛，3/4 为深化。
