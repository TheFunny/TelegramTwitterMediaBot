# 架构优化设计：可测试性接缝 + handlers 拆分

> 状态：**阶段 A、B、C 已实施**（A: `c9e72fd`，B: `50206a9` + `ae69d72`，C:
> rate_limit 提交）；**D 已延迟**——待下次数据库 schema 变化时实施（见 §5）。
> 目标：把仓库最大的测试空白（`handlers.rs`/`send.rs` 的发送与分派逻辑）补上
> 可测试接缝，并把 ~1100 行的 handlers 单体拆成模块。

---

## 1. 现状与动机

- `handlers.rs`（~1100 行）混装：命令解析/执行、URL 提取 + 任务通道、inline
  debounce、callback、edit-before-forward、全部全局静态。
- 关键路径零测试：`url_media` 的分派、`dispatch_send` 的失败分类、缓存命中路径、
  edit-before-forward、转发重试——AGENTS.md 自认 "untested: handlers.rs"。
- 根因：`handlers.rs`/`send.rs` 直接依赖 teloxide `Bot`（具体类型）与全局静态
  （`CHAT_STORE`/`TASK_QUEUE`/`LINK_CACHE`/`CONFIG`），没有注入点。

## 2. 阶段 A：handlers 拆分（纯组织，零风险，先行）

把 `handlers.rs` 拆为模块（仅移动代码，不改签名）：

```
handlers/
  mod.rs       — 入口：message/inline/callback 分发 + 公共类型（UrlJob、log_key）
  statics.rs   — CHAT_STORE / TASK_QUEUE / LINK_CACHE / DB / CONFIG / URL_JOBS
  commands.rs  — Command enum + execute_command + set_forward_channel_handler
  urls.rs      — extract_urls + start/stop_url_workers + url_media + build_send_task + media_to_payload
  inline.rs    — inline_query_handler + debounce 状态机 + answer_inline_query
  callback.rs  — callback_query_handler + edit_message_handler
```

- `mod.rs` 用 `pub use` 重导出，bot 侧引用 `handlers::xxx` 不变。
- 收益：每个模块独立审阅；后续阶段 B 的接缝改动落在明确的模块内。

## 3. 阶段 B：MediaSender 接缝（核心）

**动机**：`send.rs` 的所有发送入口（`send_media_group`/`send_animation`/
`copy_messages`）都挂在具体 `Bot` 上；测试无法注入失败/成功。

**设计**：新增 `crates/xmedia-bot/src/media_sender.rs`：

```rust
/// 发送抽象：生产用 teloxide Bot，测试用记录型 mock。
/// 方法签名与 teloxide 调用点一一对应，返回 Result 以便注入任意失败。
pub trait MediaSender: Send + Sync {
    fn send_media_group(&self, chat_id: ChatId, items: Vec<InputMedia>)
        -> BoxFuture<'_, Result<Vec<Message>, RequestError>>;
    fn send_animation(&self, chat_id: ChatId, file: InputFile, caption: Option<&str>, spoiler: bool, reply_to: i64)
        -> BoxFuture<'_, Result<Message, RequestError>>;
    fn copy_messages(&self, to: ChatId, from: ChatId, ids: Vec<MessageId>)
        -> BoxFuture<'_, Result<Vec<MessageId>, RequestError>>;
    // 按需扩展：edit_message_caption / delete_message / answer_callback_query …
}

impl MediaSender for Bot { /* 委托现有 teloxide 调用 */ }
```

配套：`ChatStore`/`LinkCache`/`PersistentTaskQueue` 已是具体类型——给 `send.rs`/
`url_media` 需要的最小面加 trait（`ChatStoreReader`/`LinkCacheReader` 等），或直接
注入具体类型（它们已有内存态，测试用真实 tempdir 即可，见阶段 B-注）。

**接入点**：
- `dispatch_send` / `send_media_sequence` / `send_animation` / `forward_messages` /
  `post_send_actions` / `notify_failure` 的 `bot: &Bot` 参数改为 `sender: &dyn MediaSender`。
- `url_media` 由 `url_media(bot, message, url)` 改为 `url_media(sender, store, queue, cache, message, url)`（或聚合为一个 `AppContext` 结构传引用）。

**测试策略**（仓库无 mock 框架，手写 mock）：
- `MockSender` 记录调用序列、按脚本返回 Ok/Err（覆盖：URL 发送成功、media-fetch
  失败触发兜底、RetryAfter 触发入队、Permanent 触发缓存失效）。
- `ChatStore`/`LinkCache` 用真实 tempdir 实例（现有测试已这么做）。
- 新增测试：`send_media_sequence` 分批续传、`send_animation` 兜底、`url_media`
  缓存命中 vs 未命中、`dispatch_send` 三分支。

**风险**：中。动 `send.rs`/`handlers.rs` 签名（约 15 处调用点），行为不变。
**不做**：`main.rs` 的 teloxide 装配不抽象（那是真正的胶水，无测试价值）。

## 4. 阶段 C：主动限流（已实施）

批量转发时的突发会触发 Telegram 频道限速，现在靠 `RetryAfter → 队列重试` 被动
应对。新增轻量令牌桶（`rate_limit.rs`）：

```rust
pub struct TokenBucket { capacity, refill_per_sec, state: Mutex<State> }
impl TokenBucket {
    pub async fn acquire(&self, n: f64); // 按 n 个 token 等待并消费
}
pub fn limiter_for(chat_id: i64) -> Arc<TokenBucket>; // 每频道一个桶
```

- 默认 `CAPACITY = 20`、`REFILL_PER_SEC = 20/60`（约 20 msg/min）；
  单次 acquire 可超出容量（记为债务，由后续 refill 偿还）。
- 挂点：`MediaSender for Bot` 的 `send_media_group`（按 items 数）、
  `copy_messages`（按 ids 数）、`send_animation`（1 token）前置 `acquire`；
  MockSender 不受影响（测试不经过限流）。
- 收益：减少 429 → 重试 → 死信；队列重试仍是全局限速的安全网。
- 风险：低，独立模块；`tokio::time`（paused-clock 可测）。

## 5. 阶段 D：DB 版本化迁移（**已延迟**）

> ⚠️ **待办提醒**：本阶段**推迟到下次数据库 schema 变化时实施**（给
> `link_cache`/`chat_state`/`tasks` 加列、改结构等）。当前 `schema_init` 是
> `CREATE TABLE IF NOT EXISTS`，无版本概念；一旦需要迁移已有线上库，必须先落地
> 本方案（`PRAGMA user_version` 迁移链）再改 schema。`db.rs` 的 `schema_init`
> 处已留注释指向这里。

```rust
// db.rs
const MIGRATIONS: &[&str] = &[
    // v1: 初始 schema（tasks / chat_state / link_cache）
    "CREATE TABLE IF NOT EXISTS tasks (...); ...",
];
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(v as usize) {
        conn.execute_batch(sql)?;
        conn.pragma_update(None, "user_version", (i + 1) as i64)?;
    }
    Ok(())
}
```

- 低优先级：schema 未变时无收益；将来加列/改结构时必须有。
- `open_store` 改用 `migrate` 替换 `schema_init` 调用。

## 6. 明确不做

- **不拆 xmedia-core**：`Task`/队列/发送抽成独立 lib crate 是大工程，除非出现
  第二个客户端，否则收益不抵成本。
- **不引入 DI 框架**：仓库惯例是 LazyLock 静态 + 显式传参，保持。
- **不抽象 main.rs 的 teloxide 装配**。

## 7. 实施记录

| 阶段 | 提交 | 说明 |
|---|---|---|
| A | `c9e72fd` | handlers 拆为 `{mod, statics, commands, urls, inline, callback}` |
| B | `50206a9` | `media_sender.rs`：`trait MediaSender` + `impl for Bot`（`<Bot as Requester>::` 消歧）；send.rs 8 处签名改 `&dyn MediaSender`；`MockSender` 测试覆盖兜底触发与错误分类（+5 测试） |
| B | `ae69d72` | `AppContext` 注入 `url_media`（sender/store/queue/cache），url_media 全链路测试（缓存命中/失效/成功/不支持 URL，+3 测试） |
| C | rate_limit 提交 | `rate_limit.rs` 令牌桶 + 每频道注册表；`MediaSender for Bot` 的 group/copy/animation 前置 `acquire`（+3 测试） |
| D | — | **已延迟**：待下次数据库 schema 变化时实施（见 §5） |

A、B、C 为核心并已实施；D 在 schema 变更时落地。
