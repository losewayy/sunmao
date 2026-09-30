# DESIGN-SYSTEM.md — GUI token 体系与动效规范（实施稿）

> 对象：实现这套体系的 agent。本文是**唯一**的视觉/动效数值来源，
> 取代 `DESIGN.md` 里的 token 表。实现对象是真实产品前端：
> `crates/cli/src/serve/assets/`（`sunmao serve` 与 Tauri 壳共用同一份资产）。

## 0. 现状与目标

现状（2026-09 审计 `index.html` 的 `<style>`，541 行）：

| 维度 | 现状 | 问题 |
|---|---|---|
| 颜色 | 有 ~40 个变量，但组件里仍有 ~30 处裸 `rgba()/#fff` | 换主题/调对比度时漏改 |
| 字号 | 15 种（10.5–38px，含 6 种半像素） | 无层级，中文半像素发虚 |
| 圆角 | 18 种（1–18px） | 嵌套圆角不成比例 |
| 玻璃层级 | 组件里手写 `calc(var(--glass-a) ± x)`，10 种偏移 | 层级不可推理 |
| 阴影 | 20 种写法，4 处裸阴影 | 同上 |
| 动效 | 1 条曲线 + `ease`/`ease-in-out` 混用；时长 14 种；JS 里写死 `300`/`160`/`220`ms | 动效不成体系，JS 与 CSS 各一份真相 |
| z-index | 13 个裸数字 | 叠放关系靠记忆 |

目标：**组件代码里不出现任何裸设计值**。颜色、字体、字号、圆角、阴影、
玻璃层级、时长、曲线、位移量、z-index 全部来自 `tokens.css`；
JS 里的动效时长也从同一份 token 读取；`xtask` 守门，违规不能合入。

审美判断（保留什么、改什么）：

- **保留**壁纸 + 磨砂玻璃的材质立场——这是产品辨识度所在，已经成立。
- **收敛**：字号收成 8 档整数、圆角 6 档、玻璃 4 层、阴影 3 层、时长 5 档。
  精致感主要来自"少而一致"，不是更多装饰。
- **动效性格**：安静、短、有物理感。进场 ease-out、退场更快的 ease-in；
  只在 ≤24px 的开关类元素上允许极轻微回弹；流式输出逐字不做任何动画，
  只在"块"首次出现时进场一次；回放（replay）不播进场动画。

## 1. 文件结构与加载

```
crates/cli/src/serve/assets/
  tokens.css   仅含 :root / [data-theme] / [data-motion] 下的自定义属性
               + 全部 @keyframes。唯一允许出现裸值的文件。
  app.css      组件样式，只消费 token。
  index.html   结构 + 内联 <script>（保持内联：replay_parity.mjs 只 eval
               index.html 里的 <script>，脚本外移需同步改 parity 驱动）。
```

- `index.html` 的 `<head>` 用 `<link rel="stylesheet" href="/tokens.css">`
  和 `/app.css`（先 tokens 后 app）。
- 路由加在 `serve/request.rs` 的路由表（`HostHandle::request`）里：
  `("GET"|"HEAD", ["tokens.css"])`、`["app.css"]`，内容用 `include_str!`
  嵌入，新增 `HostResponse::css()`，`content-type: text/css; charset=utf-8`。
  Tauri 壳走同一张路由表（`sunmao` scheme），无需改 `crates/gui`。
- 两个 `const` 放在 `serve.rs` 紧挨 `INDEX`，注释说明来源。

## 2. 分层与命名

两层，组件只准用第二层：

1. **原始值**：只存在于 `tokens.css`（色值、px、ms、贝塞尔）。
2. **语义 token**：按类别加前缀，组件只写 `var(--前缀-*)`。

| 前缀 | 类别 | 例 |
|---|---|---|
| `--c-` | 颜色 | `--c-text-2` `--c-accent-soft` |
| `--font-` `--fs-` `--fw-` `--lh-` `--ls-` | 字体 | `--fs-sm` |
| `--s-` | 间距（名字即 px 值） | `--s-12` |
| `--h-` | 控件高度 | `--h-lg` |
| `--w-` | 布局宽度 | `--w-rail` |
| `--r-` | 圆角 | `--r-lg` |
| `--glass-` `--blur` | 玻璃层级 | `--glass-2` |
| `--sh-` `--ring-` | 阴影 / 内描边 | `--sh-2` `--ring-1` |
| `--z-` | 叠放层 | `--z-pop` |
| `--dur-` `--ease-` `--move-` `--scale-` `--delay-` `--hold-` `--stagger` | 动效 | `--dur-base` |

`apply()` 运行时写入的 token（用户设置映射）只有这些，且必须先在
`tokens.css` 里有默认值：`--c-text` `--c-text-2` `--c-text-3` `--c-accent`
`--c-stroke` `--glass-rgb` `--glass-a` `--blur` `--veil` `--font-ui` `--font-mono`
`--noise`。其余派生色（`--c-accent-soft` 等）在 `tokens.css` 里用
`color-mix()` 从这些推导，用户改强调色时自动跟随，JS 不再逐个写。

## 3. 颜色

### 3.1 语义色

| token | dark | light | 用途 |
|---|---|---|---|
| `--c-text` | `#E8E9F0`（运行时） | `#1C1E28` | 正文、标题 |
| `--c-text-2` | `#8B8FA5`（运行时） | `#5D6278` | 次级文本、图标 |
| `--c-text-3` | `#565A70`（运行时） | `#9095A8` | 元信息、占位、时间 |
| `--c-on-accent` | `#fff` | `#fff` | 强调底上的字 |
| `--c-accent` | `#339CFF`（运行时） | 同 | 唯一强调色 |
| `--c-ok` | `#9ece6a` | `#3d8b37` | 成功 |
| `--c-warn` | `#e0af68` | `#b7791f` | 等待、审批、警示 |
| `--c-err` | `#f7768e` | `#d0435c` | 失败、停止 |
| `--c-tool` | `#7dcfff` | `#0b7bab` | 工具名 |
| `--c-mark-you` | `var(--c-warn)` | 同 | 用户标记点 |
| `--c-mark-bot` | `#8c9cff` | `#5b6cf0` | agent 标记点 |
| `--c-danger-solid` | `#c42b1c` | 同 | 仅窗口关闭钮 hover（Windows 惯例） |
| `--c-thumb` | `#fff` | `#fff` | 开关/滑块把手 |

派生（全部写在 `tokens.css`，不随主题重复定义）：

```css
--c-accent-tint:  color-mix(in srgb, var(--c-accent) 9%,  transparent); /* 用户气泡底、选中卡底 */
--c-accent-soft:  color-mix(in srgb, var(--c-accent) 16%, transparent); /* pill 底 */
--c-accent-soft-2:color-mix(in srgb, var(--c-accent) 26%, transparent); /* pill hover */
--c-accent-line:  color-mix(in srgb, var(--c-accent) 60%, transparent); /* 焦点描边、输入框焦点 */
--c-selection:    color-mix(in srgb, var(--c-accent) 38%, transparent);
--c-ok-soft:   color-mix(in srgb, var(--c-ok)   16%, transparent);
--c-warn-soft: color-mix(in srgb, var(--c-warn) 16%, transparent);
--c-err-soft:  color-mix(in srgb, var(--c-err)  16%, transparent);
--c-ok-row:    color-mix(in srgb, var(--c-ok)   11%, transparent);   /* diff 行 */
--c-err-row:   color-mix(in srgb, var(--c-err)  11%, transparent);
--c-approve-bg:   color-mix(in srgb, var(--c-warn) 12%, transparent); /* 替代 --amber-bg */
--c-approve-line: color-mix(in srgb, var(--c-warn) 34%, transparent); /* 替代 --amber-line */
```

### 3.2 表面色

| token | dark | light | 用途 |
|---|---|---|---|
| `--c-stroke` | `rgba(255,255,255,.08)`（运行时） | `rgba(20,24,40,.07)` | 玻璃边、分隔 |
| `--c-stroke-2` | `rgba(255,255,255,.13)` | `rgba(20,24,40,.14)` | 强调边、浮层边 |
| `--c-hi` | `rgba(255,255,255,.055)` | `rgba(255,255,255,.75)` | 玻璃顶部高光 |
| `--c-fill-1` | `rgba(255,255,255,.045)` | `rgba(20,24,40,.04)` | hover |
| `--c-fill-2` | `rgba(255,255,255,.075)` | `rgba(20,24,40,.07)` | 选中 / pressed |
| `--c-fill-3` | `rgba(255,255,255,.12)` | `rgba(20,24,40,.11)` | 轨道、禁用底 |
| `--c-sunken` | `rgba(0,0,0,.24)` | `rgba(20,24,40,.05)` | 代码块、输入框底 |
| `--c-code-bg` | `rgba(255,255,255,.07)` | `rgba(20,24,40,.06)` | 行内 code |
| `--c-scrim` | `rgba(4,6,10,.28)` | `rgba(40,46,64,.14)` | 命令面板遮罩 |
| `--c-scrollbar` | `rgba(255,255,255,.13)` | `rgba(20,24,40,.18)` | 滚动条 |
| `--c-swatch-ring` | `rgba(255,255,255,.25)` | `rgba(20,24,40,.12)` | 色点内描边（合并现 .22/.28/.3 三处） |
| `--c-page` | `#07090e` | `#e9ecf2` | 壁纸加载前的底色 |
| `--c-doc` | `#fbfaf7` | 同 | artifact iframe 底（文档纸色） |
| `--glass-rgb` | `22 24 31`（运行时） | `244 245 248` | 玻璃基色 |
| `--island-rgb` | `13 14 19` | `250 250 252` | 岛屿基色 |
| `--veil` | 运行时 | 运行时 | 壁纸暗化 |

设置页里主题预览小图（`.mini`）是"画出另一个主题"，允许在
`tokens.css` 里定义专用 `--c-mini-dk-*` / `--c-mini-lt-*`，不走主题切换。

### 3.3 语法色

`--syn-k/t/p/s/n/v/x` 保持现值与命名（与 TUI 同源），不加前缀。

## 4. 字体排印

中文字形不允许半像素字号（现有 10.5/11.5/12.5/13.5 全部取整）。

| token | 值 | 角色 |
|---|---|---|
| `--fs-2xs` | 11px | kbd、tag、mono 元信息（时间、计数、版本号） |
| `--fs-xs` | 12px | 次级文本、分组头、提示、mono 路径/耗时 |
| `--fs-sm` | 13px | UI 默认：列表行、按钮、菜单项 |
| `--fs-md` | 14px | 聊天正文、输入框、审批卡标题 |
| `--fs-lg` | 16px | 气泡 h3、命令面板输入、关于页标题 |
| `--fs-xl` | 18px | 气泡 h2 |
| `--fs-2xl` | 20px | 气泡 h1 |
| `--fs-3xl` | 26px | 设置页页标题 |
| `--fs-display` | 30px | 空会话 hero 标题 |
| `--fs-metric` | 36px | dataflow 主数字（mono） |

规则：**mono 在视觉上比同号 UI 字大约一档**，密集行（工具行、kv、
路径列表）里 mono 用比相邻 UI 文本小一档的 token。

| token | 值 | | token | 值 |
|---|---|---|---|---|
| `--fw-regular` | 400 | | `--lh-tight` | 1.3（标题、hero、数字） |
| `--fw-medium` | 500（替代现 550） | | `--lh-ui` | 1.5（全局默认） |
| `--fw-strong` | 600 | | `--lh-body` | 1.7（聊天正文） |
| `--ls-display` | -.01em | | `--lh-code` | 1.6（代码块、命令块） |
| `--ls-metric` | -.03em | | | |

`--font-ui` / `--font-mono` 保持现有字体栈（运行时由设置页覆盖）。

## 5. 尺寸

### 5.1 间距 `--s-*`（4 基，含 2/6/10 细分）

`--s-0:0` `--s-2:2px` `--s-4:4px` `--s-6:6px` `--s-8:8px` `--s-10:10px`
`--s-12:12px` `--s-16:16px` `--s-20:20px` `--s-24:24px` `--s-32:32px`
`--s-40:40px` `--s-64:64px`。

`gap / padding / margin` 只用这些；现有 3/5/7/9/11/13/14/18/22/30/34 取最近档
（等距取较小者）。布局边距 `--s-gutter: var(--s-8)`（替代 `--m`）。

### 5.2 控件高度 `--h-*`

| token | 值 | 用于（现值 → 新值） |
|---|---|---|
| `--h-xs` | 20px | tag、开关 |
| `--h-sm` | 24px | 小图标钮、工具栏按钮、kbd |
| `--h-md` | 28px | pill、kv 行、胶囊内按钮（26→28） |
| `--h-lg` | 32px | 列表行、菜单项、按钮、工具行、crumb（30→32） |
| `--h-xl` | 36px | 输入框、发送键、工作卡头、toast、设置返回（34/38→36） |
| `--h-2xl` | 40px | 工作区行、岛屿头、事件日志头（42→40） |
| `--h-3xl` | 48px | 标题栏、命令面板输入（50→48） |

### 5.3 布局宽度 `--w-*`

`--w-rail:252px` `--w-dock:264px` `--w-read:720px`（transcript 与输入框）
`--w-bubble-you:600px` `--w-settings:760px` `--w-palette:580px`
`--w-starters:620px` `--w-pop:220px` `--w-pop-max:380px`。

### 5.4 图标

`--icon-xs:12px` `--icon-sm:14px` `--icon-md:16px` `--icon-lg:20px`
`--icon-stroke:1.75` `--icon-stroke-bold:2.4`。

## 6. 圆角

| token | 值 | 用于 |
|---|---|---|
| `--r-xs` | 4px | 光标、diff 条、气泡尾角 |
| `--r-sm` | 6px | 行内 code、tag、kbd、小图标钮 |
| `--r-md` | 8px | 列表行、按钮、菜单项、代码块、输入框 |
| `--r-lg` | 12px | 工作卡、pop、主题卡、壁纸缩略图、toast 以外的浮层 |
| `--r-xl` | 16px | 面板（rail、dock 卡、设置卡）、气泡、岛屿、审批卡、命令面板 |
| `--r-2xl` | 20px | 输入框容器（composer） |
| `--r-full` | 999px | pill、胶囊、toast、开关、圆点 |

映射：1/2→xs，5/6/7→sm，8/9→md，10/12→lg，14/16→xl，17/18→2xl，
`50%`（正圆）→`--r-full`。

**嵌套规则**：内层圆角 = 外层圆角 − 内边距（最小 `--r-xs`）。例：composer
`--r-2xl` 内顶栏用 `calc(var(--r-2xl) - 1px)` 贴边；pop `--r-lg` + padding 6 →
菜单项 `--r-sm`。用户气泡：`var(--r-xl) var(--r-xl) var(--r-xs) var(--r-xl)`。

## 7. 玻璃层级、阴影、叠放

### 7.1 玻璃层级（取代组件里手写的 `calc(var(--glass-a) ± x)`）

| token | 定义 | 用于 |
|---|---|---|
| `--glass-content` | `max(.18, calc(var(--glass-a) - .08))` | 气泡、工作卡、开场卡、notice、think |
| `--glass-base` | `var(--glass-a)` | rail、dock 卡、crumb、胶囊、设置卡 |
| `--glass-raised` | `min(.95, calc(var(--glass-a) + .18))` | composer、`.l2` |
| `--glass-float` | `min(.96, calc(var(--glass-a) + .28))` | pop、toast、tooltip、命令面板、岛屿、不透明侧栏 |

每层有 `-hover` 变体 = 同层 + .08（封顶同上）。`.glass` 配方不变，
默认 `--ga: var(--glass-base)`；组件只允许写 `--ga: var(--glass-*)`。
工具类 `.g-content / .g-raised / .g-float` 供标记直接使用。

### 7.2 阴影

| token | dark | light |
|---|---|---|
| `--sh-0` | `0 4px 12px rgba(0,0,0,.25)` | `0 4px 12px rgba(30,36,60,.10)` |
| `--sh-1` | `0 10px 30px rgba(0,0,0,.24)` | `0 10px 30px rgba(30,36,60,.10)` |
| `--sh-2` | `0 16px 48px rgba(0,0,0,.40)` | `0 16px 48px rgba(30,36,60,.20)` |
| `--sh-3` | `0 22px 56px rgba(0,0,0,.42)` | `0 22px 56px rgba(30,36,60,.18)` |
| `--sh-thumb` | `0 1px 3px rgba(0,0,0,.35)` | 同 |
| `--sh-text` | `0 1px 2px rgba(0,0,0,.35)` | `none` |
| `--sh-text-display` | `0 2px 18px rgba(0,0,0,.35)` | `none` |

层级 ↔ 阴影固定配对：content 无外阴影、base→`--sh-1`、raised→`--sh-2`、
float→`--sh-3`（tooltip 例外用 `--sh-0`）。所有玻璃都叠加 `--sh-inset-hi`
（`inset 0 1px 0 var(--c-hi)`）。

描边：`--ring-1: inset 0 0 0 1px var(--c-stroke)`、`--ring-2`（stroke-2）、
`--ring-accent`（accent-line）、`--ring-focus: 0 0 0 3px color-mix(in srgb,var(--c-accent) 45%,transparent)`、
`--focus-outline: 2px solid color-mix(in srgb,var(--c-accent) 80%,transparent)`。
`--blur-edge: 10px`（顶/底渐隐带）。

### 7.3 叠放 `--z-*`

`--z-wall:0` `--z-app:1` `--z-hero:2` `--z-edge:3` `--z-rail:4` `--z-dock:5`
`--z-composer:6` `--z-chrome:20` `--z-toast:30` `--z-pop:50` `--z-modal:60`
`--z-tip:80`。组件内部局部叠放用 `--z-l1:1` `--z-l2:2`。

## 8. 动效

### 8.1 原则

1. **动效只表达因果**：出现、离开、展开、移动、状态变化。不做装饰性动画。
2. **进场慢于退场**：进场 ease-out，退场用 ease-in，且退场取短一档的时长。
3. **位移小**：进场位移 ≤6px，缩放 ≥.985。玻璃界面大位移会显廉价。
4. **流式输出不动**：逐 token 追加的文字没有任何动画。只有块（消息、工作卡、
   审批卡、岛屿）**首次**出现时进场一次。
5. **回放不动**：`renderReplay` 期间给 `<html>` 加 `data-replaying`，所有
   `.enter` 进场与 hero 动画在该属性下禁用；回放结束后移除。
6. **不依赖 `transitionend` / `animationend`**：reduce 模式下时长可能为 0，
   事件不触发。收尾逻辑一律用 `motion.wait('exit')` 或 WAAPI 的 `.finished`。
7. **只动 `opacity` / `transform`**：高度展开例外，且只用 WAAPI 做一次性过渡，
   不在 CSS 里 transition `height`。

### 8.2 时长

| token | 值 | 用于 |
|---|---|---|
| `--dur-instant` | 80ms | hover 底色/文字色、图标色 |
| `--dur-fast` | 140ms | press 反馈、tooltip、所有退场、toast 退场 |
| `--dur-base` | 200ms | pop/菜单进场、chevron 旋转、开关、选中态、焦点环 |
| `--dur-slow` | 320ms | 布局级：dock 滑入滑出、卡片高度展开、岛屿变高、进度条 |
| `--dur-scene` | 560ms | 场景级：hero 进场、壁纸交叉淡化 |

循环：`--loop-breathe:1600ms`（等待）`--loop-breathe-busy:1200ms`（运行）
`--loop-spin:1000ms` `--loop-caret:1000ms`。

驻留与延迟：`--hold-toast:2600ms` `--hold-toast-long:4200ms`
`--delay-tip:400ms`（悬停多久出 tooltip）`--stagger:60ms`（同组元素逐个进场间隔）。

### 8.3 曲线

| token | 值 | 用于 |
|---|---|---|
| `--ease-out` | `cubic-bezier(.2,.8,.2,1)` | 进场、移动、展开（沿用现有曲线） |
| `--ease-in` | `cubic-bezier(.4,0,1,1)` | 退场 |
| `--ease-in-out` | `cubic-bezier(.65,0,.35,1)` | 双向切换（dock、折叠）、呼吸循环 |
| `--ease-spring` | `cubic-bezier(.34,1.4,.64,1)` | 仅开关把手、勾选标记；元素 ≤24px 才允许 |
| `--ease-linear` | `linear` | 旋转、进度 |

`ease` / `ease-in-out` 关键字不再出现在组件代码里。

### 8.4 位移与缩放

`--move-1:4px`（pop、toast）`--move-2:6px`（块进场、命令面板）
`--move-hover:-2px`（开场卡 hover 上浮）`--move-join:7px`（hero 标志合拢）
`--scale-pop:.985` `--scale-press:.98`。

### 8.5 模式目录（组件只允许用这些组合）

| 模式 | 属性 | 进 | 出 |
|---|---|---|---|
| hover | background/color | `--dur-instant` `--ease-out` | 同 |
| press | transform: scale(`--scale-press`) | `--dur-fast` `--ease-out` | 同 |
| 块进场 `.enter` | `k-rise`：opacity 0→1、Y `--move-2`→0 | `--dur-slow` `--ease-out` | — |
| pop / 菜单 | opacity + Y `--move-1` + scale `--scale-pop` | `--dur-base` `--ease-out` | `--dur-fast` `--ease-in` |
| 命令面板 | `k-drop`：Y −`--move-2` + scale | `--dur-base` `--ease-out` | 立即移除 |
| toast | `k-rise` | `--dur-base` `--ease-out` | `--dur-fast` `--ease-in`，Y `--move-1` |
| tooltip | opacity | `--dur-fast` | `--dur-fast` |
| 折叠 chevron | rotate 0↔90° | `--dur-base` `--ease-in-out` | 同 |
| 高度展开（工具输出、工作卡、岛屿） | WAAPI height | `--dur-slow` `--ease-out` | `--dur-slow` `--ease-in-out` |
| dock | translateX | `--dur-slow` `--ease-in-out` | 同 |
| 开关 | 把手 translateX；底色 | `--dur-base` `--ease-spring`；`--dur-base` | 同 |
| 玻璃层级 hover | `--ga` → `-hover` 变体 | `--dur-base` `--ease-out` | 同 |
| hero | 标志 `k-join-l/r` → 标题 → 路径 → 开场卡逐个 | `--dur-scene`，各段相差 `--stagger` | — |
| 壁纸切换 | canvas opacity | `--dur-scene` `--ease-in-out` | 同 |
| 状态点 | `k-breathe` | `--loop-breathe(-busy)` `--ease-in-out` infinite | — |

`--ga` 可动画化：在 `tokens.css` 注册

```css
@property --ga { syntax: '<number>'; inherits: false; initial-value: .56; }
```

注册后 `var(--ga)` 不再回落到 fallback，所以 `.glass` 必须显式写
`--ga: var(--glass-base)`，`transition` 里写 `--ga var(--dur-base) var(--ease-out)`。

keyframes 统一放 `tokens.css`，命名 `k-*`：`k-rise` `k-drop` `k-spin`
`k-breathe` `k-join-l` `k-join-r`（取代 `blockIn` `palIn` `spin` `breathe` `joinA` `joinB`）。
keyframes 内的位移也用 `--move-*`。

### 8.6 减少动效

删掉现在 `*{animation-duration:.001ms!important}` 的全局覆盖，改为 token 级：

```css
@media (prefers-reduced-motion: reduce) { :root:not([data-motion="full"]) { … } }
:root[data-motion="reduce"] { … }
/* … = --move-*:0px; --scale-*:1; --dur-slow/--dur-scene: var(--dur-fast);
       --loop-breathe*: 0s（状态点改为静态实心）；--stagger:0ms */
```

保留淡入淡出（不引发眩晕，且保持状态变化可感知）；旋转 spinner 保留
（表示运行中，属于必要信息）。设置页「外观」加一行「动效」：跟随系统 /
完整 / 减少，写 `S.motion`，`apply()` 设置 `data-motion`。

### 8.7 JS 桥：`motion`

脚本里现有写死的 `duration: 300`、`'cubic-bezier(.2,.8,.2,1)'`、
`setTimeout(…, 160 / 220 / 2600 / 4200)`、`const reduced = matchMedia(…)`
全部改走一个对象：

```js
/* motion — the only way JS touches timing; values come from tokens.css */
const motion = (() => {
  const FALLBACK = { instant: 80, fast: 140, base: 200, slow: 320, scene: 560 };
  const css = n => getComputedStyle(root).getPropertyValue(n).trim();
  const ms = (n, d) => { const v = css(n); return v ? parseFloat(v) * (v.endsWith('ms') ? 1 : 1000) : d; };
  return {
    dur: k => ms(`--dur-${k}`, FALLBACK[k]),
    ease: k => css(`--ease-${k}`) || 'ease-out',
    hold: k => ms(`--hold-${k}`, 2600),
    reduced: () => root.dataset.motionEff === 'reduce',
    wait: k => new Promise(r => setTimeout(r, motion.dur(k))),
    // one-shot WAAPI height tween; no-op when reduced
    height(el, from, to, k = 'slow') {
      if (motion.reduced()) return;
      el.animate([{ height: from + 'px' }, { height: to + 'px' }], { duration: motion.dur(k), easing: motion.ease('out') });
    },
  };
})();
```

- 每次读取即时计算（用户切换动效设置后立即生效），调用频率低，无需缓存。
- `replay_parity.mjs` 的假 DOM 里 `getComputedStyle` 返回空串，所以必须有
  FALLBACK；`apply()` 需同时写 `root.dataset.motionEff`（解析「跟随系统」后的实际值）。
- 防抖（dataflow 刷新 400ms、resize 160ms）不是动效，保留为具名常量
  `const DEBOUNCE_DATAFLOW = 400` 等，放脚本顶部。

## 9. 组件映射（重点项，其余按 §3–§8 映射表机械替换）

| 组件 | 改为 |
|---|---|
| `.glass` | `--ga:var(--glass-base)`；`box-shadow:var(--sh-inset-hi),var(--sh-1)`；`transition:--ga var(--dur-base) var(--ease-out),border-color var(--dur-base) var(--ease-out)` |
| `.bubble` / `.tools` / `.starter` / `.notice` / `.think-o` | `--ga:var(--glass-content)`；圆角 bubble `--r-xl`、tools `--r-lg` |
| `.composer` | `--glass-raised` + `--sh-2`，`--r-2xl`，边 `--c-stroke-2` |
| `.pop` `.toast` `#tip` `#palette .box` `.island` `.rail.solid` | `--glass-float`；阴影见 §7.2 配对；pop 进出按 §8.5 |
| `.row` `.nav-i` `.mi` `.ib` `.tool-h` `.tg-h` | 高度 `--h-lg`（`.ib.sm` 为 `--h-sm`）、`--r-md`、hover 模式 |
| `.btn` `.cb` | 高 `--h-lg` / `--h-xl`；`font-weight:var(--fw-medium)`；加 press 模式 |
| `.sw` 开关 | `--h-xs`、把手 `--ease-spring` |
| `.msg-h` | `--fs-xs` `--fw-strong`，文字阴影 `--sh-text` |
| 聊天正文 | `--fs-md` / `--lh-body`；h1/h2/h3/h4 = `--fs-2xl/xl/lg/md` |
| 代码块 `pre` / `.cmd` / `.tool-o pre` | `--fs-xs` mono / `--lh-code`，`--r-md`，`--c-sunken` |
| dataflow 大数字 | `--fs-metric` `--ls-metric` `--lh-tight` |
| hero | h1 `--fs-display` `--ls-display`；子标题 `--fs-xs` mono；开场卡逐个延迟 `calc(var(--stagger) * n)` |
| `.cap.close:hover` | `--c-danger-solid` / `--c-on-accent` |
| `.edge` | `backdrop-filter:blur(var(--blur-edge))` |

light 主题下现在单独写的 `[data-theme="light"] .x{text-shadow:none}` /
`box-shadow:…` 覆盖全部删除——对应 token 在 light 下已取 `none` 或浅色值。
组件里不应再出现 `[data-theme=…]` 选择器（`.mini` 预览除外）。

## 10. 守门：`xtask arch` 新增 `design tokens` 检查

在 `xtask/src/main.rs` 增加 `check_design_tokens(root, violations)`，扫描
`crates/cli/src/serve/assets/app.css` 与 `index.html`：

`app.css` 中禁止（注释内容先剥离）：

| 规则 | 正则（Rust regex） |
|---|---|
| 裸颜色 | `#[0-9a-fA-F]{3,8}\b`、`\brgba?\(\s*\d` 、`\bhsla?\(` |
| 裸时长 | `\b\d*\.?\d+m?s\b`（`0s` 除外） |
| 裸曲线 | `cubic-bezier\(`、`\b(ease\|ease-in\|ease-out\|ease-in-out)\b`（`var(--ease-` 内除外） |
| 裸字号 | `font(-size)?:[^;]*\b\d+(\.\d+)?px` |
| 裸圆角 | `border-radius:[^;]*\b[1-9]\d*(\.\d+)?px`、`border-radius:\s*50%` |
| 裸 z-index | `z-index:\s*-?\d` |
| 主题分叉 | `\[data-theme=`（以 `/* token-exempt: mini */` 结尾的行豁免） |
| keyframes | `@keyframes`（只能在 tokens.css） |

`index.html` 中禁止：`<style` 块；`style="` 属性里出现颜色/时长；
`<script>` 里出现 `cubic-bezier`、`duration:\s*\d`、`setTimeout\([^)]*,\s*\d{3,}\)`
（防抖常量以 `DEBOUNCE_` 开头声明，引用处不是字面量，自然不命中）。

`tokens.css`：每个 `var(--x)` 引用都必须在本文件或 `apply()` 的运行时名单
（§2）中有定义；`app.css` 里每个 `var(--x)` 也必须能在 `tokens.css` 找到定义
（组件内局部变量以 `--_` 开头，豁免）。这条同时抓拼写错误。

违规信息格式沿用现有：`design token: app.css:123 — raw color #fff; use a --c-* token`。

## 11. 实施顺序（每步都应可单独提交并通过全部门禁）

1. **抽离**：把 `<style>` 原样拆成 `tokens.css`（`:root`/`[data-theme]`/keyframes）
   与 `app.css`（其余），加路由与 `HostResponse::css`，页面效果零变化。
   截图对比确认。
2. **token 化颜色与表面**：§3，消灭裸色与 light 分叉选择器。
3. **排印与尺寸**：§4–§6。视觉会有 1px 级变化，属预期。
4. **玻璃、阴影、叠放**：§7。
5. **动效**：§8，含 `@property --ga`、`motion` 桥、`data-replaying`、
   `data-motion` 与设置页新增一行。
6. **守门**：§10 的 xtask 检查；确认 `app.css` 零违规后打开。
7. **文档**：`DESIGN.md` 的 token 表替换为指向本文件的一句话（DESIGN.md 与
   GUI.md 为 gitignore 的私有稿，本文件是否入库由维护者决定）。

## 12. 验收

- `cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace`（含 `tui::replay_parity`）、`cargo run -p xtask -- arch` 全绿。
- `sunmao serve` 下截图对比：空会话、长会话（工作卡折叠/展开、审批卡、岛屿）、
  设置三页、命令面板、dark/light 各一套；步骤 1 应像素级一致，之后差异只来自
  §4–§6 的取整。
- 手动核对动效：pop 进出不对称（出更快）；回放历史会话时无进场闪烁；
  系统开启「减少动效」后无位移、无呼吸，淡入仍在，spinner 仍转；
  设置页动效三档即时生效。
- 用户改强调色后，pill、用户气泡、焦点环、选中卡底全部跟随（验证派生色链路）。
- Tauri 壳：`cargo build -p sunmao-gui` 后确认 `tokens.css`/`app.css` 经
  `sunmao` scheme 正常加载（无 CSP 问题，`tauri.conf.json` 当前 `csp:null`）。
- 仓库里不出现本机绝对路径（AGENTS.md 隐私红线）。
