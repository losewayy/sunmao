# IM 接入 — `sunmao im`（v0.3-im）

> Telegram / 飞书 / QQ / 钉钉 / 微信进，sunmao 会话出。IM 是一个**渠道**，
> 不是一套前端协议——会话事实仍然只有 SessionEvent 日志一份，
> 工具/审批缝原样复用。

## 架构

```text
Telegram Bot API (getUpdates 长轮询 — 不需要公网入口)
飞书/Lark 开放平台 (WebSocket 长连接，订阅 im.message.receive_v1)
QQ 开放平台 v2 (WebSocket 长连接 + 被动回复窗口)
钉钉企业内部机器人 (Stream 反向 WebSocket — gateway/connections/open)
微信个人号 iLink (ilink/bot/getupdates，40s 长轮询)
        │ InboundMsg { source: ImSource, text }
        ▼
crates/cli/src/im/ —— gateway
  config.rs    ~/.sunmao/channels.json（凭据走 *_env/*_file，不落明文）
  authz.rs     dm_policy: pairing(默认)/allowlist/open/disabled
  route.rs     build_session_key — 唯一的 key 推导点
  store.rs     ~/.sunmao/im/state.db —— routes/pairing/allowlist/delivery/meta
  progress.rs  LiveEvent → 节流草稿编辑 + turn_end 时 final 分发
  deliver.rs   投递账本 pending→attempting→delivered + 启动重投
  channels/    telegram.rs / feishu.rs(+frame) / qq.rs(+protocol) /
               dingtalk.rs(+protocol,api) / wechat.rs(+protocol)
        │ 复用 serve 的 Host/Shared 机制（adopt → queue / push_steer）
        ▼
  host.queue（idle 时进 FIFO）或 agent.push_steer（busy 时注入）
        ▼
  AgentLoop / SessionEvent / permissions / hooks —— 内核零感知
```

## 渠道一览

五个适配器都只收**单聊**（群聊一律丢弃，见「非目标」），都已经接进 `channels::build()` 这一处工厂——加渠道 = 一个 `ChannelSpec` 变体 + 它的 `scoped()` 分支 + 工厂一个真臂，别处不动。

| 渠道 | 入站 | 出站 | 分片 | 草稿编辑 | 群聊 |
|---|---|---|---|---|---|
| telegram | `getUpdates` 长轮询 | `sendMessage` | 4096 字符（段落优先） | `editMessageText` | 丢弃 |
| feishu | WS 长连接事件 | `im.message.create`（post） | 4000 字符（段落优先） | `im.message.update` | 丢弃 |
| qq | WS + 被动 `msg_id` | `/v2/users/{openid}/messages` | 2000 字符硬切 | 无 | 丢弃 |
| dingtalk | Stream 反向 WS + 逐条 ACK | `/robot/oToMessages/batchSend` | 12000 UTF-8 字节 | 无 | 丢弃 |
| wechat | `ilink/bot/getupdates` 40s 长轮询 | `ilink/bot/sendmessage` | 4000 字符 | 无 | 没有群聊 |

「草稿编辑 = 无」的渠道，`progress.rs` 的进度草稿会**重发**而不是原地刷新（`send_text` 返回 `None`，没有可编辑的 message id）。

## Owner 拍板钉死的语义（勿悄悄改动）

- **入口**：独立 `sunmao im` 子命令；serve 公网化不做。
- **dmScope=main**：所有 DM 汇入同一会话 `im:main`（"一个脑子"）。多用户共处同一会话时**上下文隔离 ≠ 安全隔离**——所有 sender 共享同一 transcript 与工具面，别把它当多租户边界；要隔离就分部署。
- **忙中 = steer**：消息注入点是「当前工具执行完、下一个 LLM 请求前」，不打断正在跑的命令、不波及子代理。
- **`/stop` 只停主 agent**：不级联子代理（`cancel_main()`）。停单个子代理在 GUI 的子代理面板操作，或让主代理去停。
- **cwd 固定**：`~/.sunmao/im/workspace/`。
- **审批 = full_access**：IM 渠道一律 full_access——手机端没有按钮审批的位置。安全靠 pairing/allowlist 准入 + `deny` 权限规则兜底。
- **投递 = at-least-once**：SQLite 账本；`attempting` 态重启重投带 "♻️ 可能重复"前缀（诚实语义）。

## 配置：`~/.sunmao/channels.json`

```jsonc
{
  "dm_policy": "pairing",            // pairing | allowlist | open | disabled
  "dm_scope": "main",                // main | per_channel_peer
  "unauthorized_dm_behavior": "pair",// pair | ignore（ignore = 完全静默）
  "allowlist": ["telegram:12345"],   // "*" 只对 dm_policy:open 有意义
  "owner": "telegram:12345",         // 可选；首个批准者自动成为 owner
  "channels": [
    {
      "kind": "telegram",
      "enabled": true,
      "token_env": "SUNMAO_TG_TOKEN",  // 或 "token_file": "D:/secrets/tg.txt"
      "poll_timeout_secs": 30,
      // 渠道级覆盖（可选）：dm_policy / allowlist（sender id 列表）
    },
    {
      "kind": "feishu",
      "enabled": true,
      "app_id": "cli_xxx",
      "app_secret_env": "SUNMAO_FS_SECRET", // 或 app_secret_file
      "region": "feishu_cn",                // feishu_cn | lark_global
    },
    {
      "kind": "qq",
      "enabled": true,
      "app_id": "1024xxxx",
      "app_secret_env": "SUNMAO_QQ_SECRET", // 或 app_secret_file
    },
    {
      "kind": "dingtalk",
      "enabled": true,
      "corp_id": "dingxxxxxxxx",             // 组织 ID
      "client_id": "dingxxxxxxxx",           // 企业内部应用 Client ID
      "client_secret_env": "SUNMAO_DT_SECRET",// 或 client_secret_file
      "robot_code": "dingxxxxxxxx",          // 机器人编码
      "api_base_url": "https://api.dingtalk.com/v1.0", // 可省，默认即此
    },
    {
      "kind": "wechat",
      "enabled": true,
      "bot_token_env": "SUNMAO_WX_TOKEN",    // 或 bot_token_file
    }
  ]
}
```

凭据约定与 mcp.json 一致：`*_env` 读环境变量、`*_file` 读文件，配置文件里**永远不写 token 明文**（字段 `token` 不存在）。每个渠道块还可以带 `owner` / `dm_policy` / `allowlist` 覆盖顶层设置。

## 各渠道要点

- **telegram**：`token_env`/`token_file` 换 Bot API token； `poll_timeout_secs` 是 `getUpdates` 的 hold 时间（默认 30）。
- **feishu**：`region` 决定 API 域名（`feishu_cn` = open.feishu.cn， `lark_global` = open.larksuite.com），两边的应用不通用。
- **qq**：`app_id` + `app_secret` 换 `access_token`；回复挂在用户那条消息的被动窗口（5 分钟内），窗口外的回复不带 `msg_id` 发出。
- **dingtalk**：企业内部机器人走 **Stream 模式**——`POST /gateway/connections/open` 拿到一次性 `endpoint` + `ticket` 后拨反向 WebSocket，平台把回调推下来；**每条回调都要回 ACK**，不回会被重投。业务接口用 `x-acs-dingtalk-access-token`（`POST /oauth2/{corpId}/token` 换取，提前 60 秒过期），单聊发送走 `/robot/oToMessages/batchSend`。断线固定 5 秒重连。
- **wechat**：`bot_token` 换 iLink 机器人身份。增量靠 `get_updates_buf` 游标（落盘，重启续传）；**回复有窗口**——必须持有该用户最近的 `context_token`（入站消息带回，缓存 24 小时），没有就直接报错不发送。 `ret/errcode == -14` 表示 session 过期：停止轮询并报错，重新签发 token 才行。**扫码登录本版未提供**，`bot_token` 只能按上面的凭据约定手工提供。

## 准入流程（dm_policy: pairing）

1. 陌生 sender 发 DM → 收到 8 位配对码（无歧义字母表，1h TTL，每渠道 ≤3 个 pending，每 sender 60s 冷却）。
2. 部署机上 `sunmao pairing approve <code>`（或 GUI 设置页 "IM 渠道" 里批准）→ sender 进 allowlist；**首个批准者 = owner**。
3. `sunmao pairing list` 看待批准/已批准；`revoke <sender>` 撤销。
4. IM 内 owner 可用 `/pairing` 看待批准列表。

## 消息与回复

- 每条入站 DM 以 `[im:<channel> from <sender>]` 前缀注入会话—— main 会话里多 sender 的发言需要归属标记。
- 空闲 → 进 host FIFO；busy → `push_steer` 注入当前回合。
- Turn 期间每个待答 chat 维护一条**可编辑状态草稿**（3s 节流编辑 + typing keep-alive）；`turn_end` 时最后一段 assistant 文本经投递账本发成新消息。草稿只有状态没有答案——半截泄漏不存在。
- `/stop` `/pairing` 在 adapter 上游 inline 处理，绝不进队列（busy 时也能立即生效）。

## serve 集成

`GET /channels`（配置文本 + status.json + pairing/allowlist）、 `PUT /channels`（写前先解析校验）、`POST /channels/pairing/approve`—— 三条路由 axum 与 Tauri scheme 同享。设置页 "IM 渠道" 分区即它们的前端。 `sunmao im` 进程内建完整 host（无 HTTP 监听）；im 会话是普通 SessionLog，serve 同机启动即可看到 `im-main` 日志。

## 状态文件

| 路径 | 内容 |
|---|---|
| `~/.sunmao/channels.json` | 渠道声明与策略（唯一配置面） |
| `~/.sunmao/im/state.db` | 路由索引 + pairing + allowlist + delivery + 适配器游标（WAL） |
| `~/.sunmao/im/status.json` | 守护进程心跳（serve 页读它） |
| `~/.sunmao/im/workspace/` | IM 会话的固定 cwd；其 `.sunmao/sessions/im-main.jsonl` 是主会话 |

`state.db` 的 `meta` 表按渠道前缀存游标，互不干扰：

- `tg:offset` —— telegram `getUpdates` 偏移
- `qq:target:<chat>` / `qq:reply:<chat>` —— QQ 的回复目标类型与被动窗口
- `wx:cursor` —— 微信 `get_updates_buf`（必须落盘，丢了就重放或漏消息）
- `wx:ctx:<peer>` —— 微信每用户的 `context_token` 与时间戳（24h 过期）

## 非目标（本版）

- **群聊仍不做**：五个渠道一律丢弃群消息（飞书 `chat_type != p2p`、 QQ `GROUP_AT_MESSAGE_CREATE`、钉钉 `conversationType != 1`），微信个人号本来就只有单聊。理由是群里回一条配对码等于当众泄漏配对码，而配对是给单个 sender 的事。
- **微信扫码登录不做**：`get_bot_qrcode` / `get_qrcode_status` 不接， `bot_token` 由配置提供（见「各渠道要点」）。
- **媒体不做**：微信 iLink 的 AES-128-ECB CDN 上传/下载、飞书/钉钉的图片/文件/语音附件都不下载也不上传；微信语音的转写文本算文本，照收。
- `streaming:draft` 模式、跨渠道身份合并、多 profile 不做。
- Session key 结构（`im:{channel}:dm:{chat}`）为群聊 `:u{user}` 段预留了位置。
