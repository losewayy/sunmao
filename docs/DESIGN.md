# DESIGN.md — GUI 设计契约

> 提取自 `.sunmao/artifacts/webui-workbench.html`（已定稿的方向稿）。
> 这是**机器可读的设计变量表**——前端实现、Stitch/v0 生成、Tauri 壳
> 全部以本文件为准。GUI 结构见 `GUI.md`；本文只管"长什么样"。

## 1. 材质立场（先于一切 token）

- **壁纸垫层 + 半透明磨砂面板**：应用底层是一张可换壁纸
  （canvas 渲染或用户上传），所有 UI 表面是半透明玻璃。
  **不存在不透明实心面板**——层级只用透明度/模糊度区分。
- **玻璃配方**（`.glass`）：
  `background: rgb(var(--glass-rgb) / var(--ga, --glass-a))`
  `backdrop-filter: blur(var(--blur)) saturate(1.2)`
  `border: 1px solid var(--stroke)`；圆角 10–16px。
- **层级用 `--ga` 递增**：基准 `--glass-a:.56`，上层 `.l2` 加 .18，
  island 加到 ~.9——越浮越不透明，不用投影堆层级。

## 2. Token 表

### 字体

```css
--font-ui: -apple-system,"Segoe UI Variable Text","Segoe UI",
           "PingFang SC","Microsoft YaHei UI",system-ui,sans-serif;
--font-mono: "JetBrains Mono","Cascadia Mono",ui-monospace,Consolas,monospace;
```

正文 13–14px / lh 1.5–1.7；次级文本 11.5–12.5px；
工具名/命令/数字一律 mono + `tabular-nums`。

### 语义色（dark 默认）

| token | 值 | 用途 |
|---|---|---|
| `--text / -2 / -3` | `#E8E9F0` / `#8B8FA5` / `#565A70` | 三级文本 |
| `--accent` | `#339CFF` | **唯一强调色**：选中、toggle、发送键、链接、运行态点 |
| `--warn` / `--you` | `#e0af68` | 审批卡、警示、用户标记点 |
| `--ok` | `#9ece6a` | 成功、✓、done 点 |
| `--err` | `#f7768e` | 失败、deny、错误 |
| `--tool` | `#7dcfff` | 工具名（mono 青色） |
| `--bot` | `#8c9cff` | agent 标记点/heading |
| 语法色 | `--syn-k #bb9af7` `--syn-t #2ac3de` `--syn-p #73daca` `--syn-s #9ece6a` `--syn-n #ff9e64` `--syn-v #c0caf5` `--syn-x #6b7089` | tokyonight 谱系，与 TUI 同源 |

### 材质参数

| token | dark | light |
|---|---|---|
| `--glass-rgb` | `22 24 31` | （同构，亮度反转） |
| `--glass-a` | `.56` | 同 |
| `--blur` | `24px` | 同 |
| `--stroke / -2` | `rgba(255,255,255,.08/.13)` | `rgba(20,24,40,.14)` 系 |
| `--fill-1/2/3` | `rgba(255,255,255,.045/.075/.12)` | `rgba(20,24,40,.04/.07/.11)` |
| `--sunken` | `rgba(0,0,0,.24)` | `rgba(20,24,40,.05)` |
| `--sh / --sh-float` | `0 10px 30px rgba(0,0,0,.24)` / `0 16px 48px rgba(0,0,0,.4)` | 换 `rgba(30,36,60,.1/.2)` |
| `--veil`（壁纸暗化） | `rgba(6,8,12,.16)` | `rgba(0,0,0,.12)` |
| `--island-rgb` | `13 14 19` | `250 250 252` |

### 组件专用

```css
--amber-bg: rgba(224,175,104,.12);  /* 审批卡底 */
--amber-line: rgba(224,175,104,.34);/* 审批卡边 */
--allow-bg:#9ece6a; --allow-fg:#13200a;  /* Allow 实心钮 */
--code-bg: rgba(255,255,255,.07);   /* 行内 code */
```

### light 主题换值（不重复列结构）

语义色全换成可读版本：`--warn #b7791f`、`--ok #3d8b37`、
`--err #d0435c`、`--tool #0b7bab`、`--you #c98a2e`、`--bot #5b6cf0`；
`--allow-bg #3f9142`；语法色见 §源文件 `[data-theme="light"]`。

## 3. 用户可调变量（设置页 ↔ token 的映射）

```
DEFAULTS = { mode:'dark', accent:'#339CFF', background:'#16181F',
  foreground:'#E8E9F0', wallpaper:'graphite', dim:.16,
  panelOpacity:.72, blur:24, translucentSidebar:false,
  contrast:50, fonts:{ui:'Segoe UI', code:'JetBrains Mono'} }
```

- `panelOpacity` → `--glass-a`（0–1，默认 .56 显示为 ~56%）
- `blur` → `--blur`；`dim` → `--veil` 不透明度
- `accent`/`background`/`foreground` → 直接写对应 token
- `wallpaper` → 内置壁纸 id 或 `custom`（localStorage 存图）
- `translucentSidebar` → 侧栏 `.rail.solid`（`--ga` 加 .3）
- 持久化目标：`~/.sunmao/ui.json`（界面设置与项目配置分离）

## 4. 组件配方（结构语义，实现照 GUI.md §6）

| 组件 | 形态 |
|---|---|
| 左侧栏 `.rail` | 悬浮玻璃轨：四周 margin、radius 16、不贴边；列表=纯文字行+6px 状态点（`.sd`：wait 琥珀呼吸/run 蓝呼吸/done 绿） |
| transcript | 单栏居中 max-width ~720px；`who` 行 = 6px 色点 + 名字 + 时间（user 琥珀点 / agent 蓝紫点） |
| 气泡 `.bubble` | radius 14，`--ga` 比基准 -0.06（比面板更透）；`user-select:text` |
| 工具块 `.tool` | radius 10 行卡，`--ga` -0.1；✓绿/⟳琥珀/mono 工具名（`--tool`）+灰摘要+右侧耗时 |
| artifact 岛 `.island` | **最不透明**（`--ga`≈.9）+ `--sh-float` 重投影 + 内顶高光；头部 mono 类型名 + `+N notes` 琥珀 pill + ↗；iframe sandbox 渲染浅色文档 |
| 审批卡 `.approve` | `--amber-bg`+`--amber-line`；命令行块 sunken；Allow 实心绿/Deny 描边/Always；附 Y/N/A kbd |
| dataflow `.df` | 右栏悬浮卡；`⚡86%` hero 数 + mono kv 行 + accent 渐变细进度条 |
| 输入 dock | 底部居中悬浮卡（`--sh-float`），上层项目/分支/env 行，下层输入+附件+审批模式+model▾+实心 ↑ |
| 状态点呼吸 | `@keyframes breathe 1.4–2s ease-in-out`（透明度脉动），运行/等待专用 |
| 图标 `.i` | 16px stroke 1.75（Lucide 系），`.sm` 14 / `.xs` 12 |

## 5. 交互与可达性基线

- `:focus-visible` → `outline:2px solid color-mix(accent 80%,transparent)`；
  列表项用内描边（`outline-offset:-2px`）
- `::selection` → accent 38%
- 滚动条 thin、半透明；`color-scheme: dark/light` 跟随主题
- 正文 ≥13px、注释 ≥11px；正文对比 ≥4.5:1（三级文本阶梯已满足）
- 中文输入保护：`e.isComposing` 才允许 Enter 发送
- 快捷键：Ctrl+K 命令面板、Ctrl+N 新对话、Ctrl+, 设置、Ctrl+\ 侧栏、
  非输入态 Y/N/A 直接裁决审批卡、Esc 逐层关闭（palette→pop→settings→blur）

## 6. 禁区（反 slop，延续 huashu-design 清单）

- 不透明实心面板（材质立场破例=bug）
- 紫渐变背景、emoji 图标、左 border accent 卡片、霓虹 glow
- 顶部大 logo 块（品牌只在侧栏顶部一行小字+icon）
- 内联 HTML 聊天渲染旁路（富内容一律 HtmlArtifact 岛屿）
- `Inter/Roboto` 当 display——系统栈即可，别引入"demo 味"字体

## 7. 同源关系（叙事要点）

`--syn-*` 语法色与 `crates/cli/src/tui/theme.rs` 同为 tokyonight
谱系——**终端和桌面共享同一套视觉基因**，TUI 语义角色（tool/ok/err/
warn/band）与 GUI token 一一对应。这是"四个前端一套灵魂"的证据。
