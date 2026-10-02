# IM 接入 — `sunmao im`（v0.3-im）

> Telegram 进，sunmao 会话出。IM 是一个**渠道**，不是一套前端协议——
> 会话事实仍然只有 SessionEvent 日志一份，工具/审批缝原样复用。

## 架构

```text
Telegram Bot API (getUpdates 长轮询 — 不需要公网入口)
        │ InboundMsg { source: ImSource, text }
        ▼
crates/cli/src/im/ —— gateway
  config.rs    ~/.sunmao/channels.json（凭据走 token_env/token_file，不落明文）
  authz.rs     dm_policy: pairing(默认)/allowlist/open/disabled
  route.rs     build_session_key — 唯一的 key 推导点
  store.rs     ~/.sunmao/im/state.db —— routes/pairing/allowlist/delivery/meta
  progress.rs  LiveEvent → 节流草稿编辑 + turn_end 时 final 分发
  deliver.rs   投递账本 pending→attempting→delivered + 启动重投
        │ 复用 serve 的 Host/Shared 机制（adopt → queue / push_steer）
        ▼
  host.queue（idle 时进 FIFO）或 agent.push_steer（busy 时注入）
        ▼
  AgentLoop / SessionEvent / permissions / hooks —— 内核零感知
```

## Owner 拍板钉死的语义（勿悄悄改动）

- **入口**：独立 `sunmao im` 子命令；serve 公网化不做。
- **dmScope=main**：所有 DM 汇入同一会话 `im:main`（"一个脑子"）。
  多用户共处同一会话时**上下文隔离 ≠ 安全隔离**——所有 sender 共享
  同一 transcript 与工具面，别把它当多租户边界；要隔离就分部署。
- **忙中 = steer**：消息注入点是「当前工具执行完、下一个 LLM 请求前」，
  不打断正在跑的命令、不波及子代理。
- **`/stop` 只停主 agent**：不级联子代理（`cancel_main()`）。停单个
  子代理在 GUI 的子代理面板操作，或让主代理去停。
- **cwd 固定**：`~/.sunmao/im/workspace/`。
- **审批 = full_access**：IM 渠道一律 full_access——手机端没有按钮
  审批的位置。安全靠 pairing/allowlist 准入 + `deny` 权限规则兜底。
- **投递 = at-least-once**：SQLite 账本；`attempting` 态重启重投带
  "♻️ 可能重复"前缀（诚实语义）。

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
    }
  ]
}
```

凭据约定与 mcp.json 一致：`token_env` 读环境变量、`token_file` 读文件，
配置文件里**永远不写 token 明文**（字段 `token` 不存在）。

## 准入流程（dm_policy: pairing）

1. 陌生 sender 发 DM → 收到 8 位配对码（无歧义字母表，1h TTL，
   每渠道 ≤3 个 pending，每 sender 60s 冷却）。
2. 部署机上 `sunmao pairing approve <code>`（或 GUI 设置页
   "IM 渠道" 里批准）→ sender 进 allowlist；**首个批准者 = owner**。
3. `sunmao pairing list` 看待批准/已批准；`revoke <sender>` 撤销。
4. IM 内 owner 可用 `/pairing` 看待批准列表。

## 消息与回复

- 每条入站 DM 以 `[im:<channel> from <sender>]` 前缀注入会话——
  main 会话里多 sender 的发言需要归属标记。
- 空闲 → 进 host FIFO；busy → `push_steer` 注入当前回合。
- Turn 期间每个待答 chat 维护一条**可编辑状态草稿**（3s 节流编辑 +
  typing keep-alive）；`turn_end` 时最后一段 assistant 文本经投递
  账本发成新消息。草稿只有状态没有答案——半截泄漏不存在。
- `/stop` `/pairing` 在 adapter 上游 inline 处理，绝不进队列
  （busy 时也能立即生效）。

## serve 集成

`GET /channels`（配置文本 + status.json + pairing/allowlist）、
`PUT /channels`（写前先解析校验）、`POST /channels/pairing/approve`——
三条路由 axum 与 Tauri scheme 同享。设置页 "IM 渠道" 分区即它们的前端。
`sunmao im` 进程内建完整 host（无 HTTP 监听）；im 会话是普通
SessionLog，serve 同机启动即可看到 `im-main` 日志。

## 状态文件

| 路径 | 内容 |
|---|---|
| `~/.sunmao/channels.json` | 渠道声明与策略（唯一配置面） |
| `~/.sunmao/im/state.db` | 路由索引 + pairing + allowlist + delivery + adapter cursor（WAL） |
| `~/.sunmao/im/status.json` | 守护进程心跳（serve 页读它） |
| `~/.sunmao/im/workspace/` | IM 会话的固定 cwd；其 `.sunmao/sessions/im-main.jsonl` 是主会话 |

## 非目标（本版）

群聊、飞书/微信渠道、`streaming:draft` 模式、跨渠道身份合并、媒体消息、
多 profile。Session key 结构（`im:{channel}:dm:{chat}`）为群聊
`:u{user}` 段预留了位置。
