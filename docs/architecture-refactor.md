# 架构优化设计：可测试性接缝 + handlers 拆分

> 状态：设计稿（未实施）。目标：把仓库最大的测试空白（`handlers.rs`/`send.rs` 的
> 发送与分派逻辑）补上可测试接缝，并把 ~1100 行的 handlers 单体拆成模块。
> 每个阶段独立提交、独立回滚；全程 fmt / clippy / test 全绿，行为不变。

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

## 4. 阶段 C（可选）：主动限流

批量转发时的突发会触发 Telegram 频道限速，现在靠 `RetryAfter → 队列重试` 被动
应对。新增轻量令牌桶（`rate_limit.rs`，~50 行）：

```rust
pub struct TokenBucket { /* capacity, refill_rate, state */ }
impl TokenBucket {
    pub async fn acquire(&self, n: u64) -> Duration; // 等待时长（或 Notify 唤醒）
}
```

- 按频道粒度（`HashMap<ChatId, Arc<TokenBucket>>`），在 `send_media_group`/
  `copy_messages` 前置 `acquire`。
- 收益：减少 429 → 重试 → 死信；风险低，独立模块。
- 不做的理由（若选不做）：当前重试链路已能自愈，容量可按需再加。

## 5. 阶段 D（可选）：DB 版本化迁移

`schema_init` 是 `CREATE TABLE IF NOT EXISTS`，无版本概念。改为：

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

## 7. 提交序列

| 阶段 | 提交消息（建议） |
|---|---|
| A | `refactor(handlers): split monolithic handlers.rs into modules` |
| B | `refactor(send): introduce MediaSender seam for testable send paths` |
| B+ | `test(send): cover fallback and classification via MockSender` |
| C | `feat(send): add per-chat token bucket rate limiting` |
| D | `refactor(db): versioned schema migrations` |

每阶段独立合入；A、B 为核心，C、D 可选。
