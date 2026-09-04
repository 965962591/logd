# logd — 50GB 级日志筛选/阅读工具（Rust + GPUI）

> 本文档是项目的唯一权威计划。继续开发时先读本文件 + `doc/PROGRESS.md`。

## 1. 背景与目标

AE 调试场景下 logcat / 平台日志动辄几十 GB，TextAnalysisTool.NET（.NET 单线程）
打不开或卡死。需要一个原生工具：秒开 50GB、低内存、多标签、关键字高亮 + 筛选，
并能直接复用现有 `.tat` 关键字配置。

产物：`logd.exe`，Windows 原生 GPUI 应用。

现有资产：`tat/` 下 3 个真实配置样本（`ae_log.tat` 24 条、`ae_log_rafe.tat`、`展锐ae.tat`），
既是兼容性依据，也是测试夹具。

### 已确认的三个决策

1. **高亮模式按每条过滤器独立设置**（字段匹配 / 行匹配），不是全局开关
2. **`.tat` 双向兼容**：能读现有文件，保存时保留原属性，新特性写成 `logd_` 前缀的额外属性
3. **分阶段交付** M0→M5，每阶段可运行可验证

---

## 2. 关键设计

### 2.1 绝不把文件读进内存

50GB / 平均 100 字节每行 ≈ **5 亿行**。任何「每行一个 String」或「每行一个 u64 偏移」
的方案都会爆内存（5 亿 × 8B = 4GB）。核心是**分块稀疏索引**。

```rust
pub const ANCHOR_STRIDE: u64 = 1024;   // 每 1024 行打一个锚点

pub struct ChunkIndex {
    start_byte: u64,       // 本块第一整行的起始字节
    end_byte:   u64,       // = 下一块的 start_byte，跨界那行归本块
    start_line: u64,       // 本块第一行的全局行号（前缀和得出）
    line_count: u64,
    anchors:    Vec<u64>,  // 块内局部第 0, 1024, 2048... 行的绝对字节偏移
}

pub struct LineIndex {
    chunks:        Vec<ChunkIndex>,  // 按 start_line 有序，partition_point 查找
    total_lines:   u64,
    indexed_bytes: u64,              // 首屏索引时 < file_len
    file_len:      u64,
    complete:      bool,
}
```

内存开销：5 亿 / 1024 × 8B ≈ **3.9 MB**。

取第 `n` 行：二分找 chunk → `local = n - start_line` → `anchors[local/1024]`
→ mmap 上 `memchr` 前进 `local % 1024` 个换行。最坏扫 1024 行 ≈ 100KB ≈ 20µs。

**为什么按块存锚点而不是一张全局表**：并行建索引时每个 worker 只知道块内的局部行号，
全局行号要等所有块统计完才能前缀和得出。按块存让合并退化成一次前缀和，
完全精确、零额外扫描。

> LRU 锚点缓存（原计划里的优化）**暂缓**。连续滚动每帧最多回扫 100KB，
> 60fps 也才 6MB/s，先不引入复杂度；等 M5 profiling 说话。

### 2.2 建索引：先头部秒开，再后台并行全量

- **阶段 A（同步，<100ms）**：`build_head()` 只索引头 256MB，UI 立刻可显示可滚动，
  行数显示为 `1,234,567+`（`+` 表示仍在统计）。边界回退到 `limit` 之前的最后一个换行，
  保证不出现半行
- **阶段 B（后台 rayon）**：`build_full()` 按 **64MiB** 切块（50GB → 800 块，
  进度条平滑 + 负载均衡），块起点串行对齐到行首后并行 `memchr_iter` 统计 → 前缀和合并
- 进度：`Progress { done, total, cancel }` 原子量，UI 30Hz 轮询
- 预期：NVMe ~3GB/s 磁盘瓶颈，50GB 约 **15–25s**

**索引缓存（M5）**：sidecar 写到
`%LOCALAPPDATA%\logd\cache\{xxh3(path):016x}-{len}-{mtime}.idx`，
自定义 LE 二进制格式（magic + version + file_len + mtime + total_lines + chunk 表），
约 4MB。二次打开同一文件 **<1s**。

### 2.3 mmap + 编码

- `memmap2` 映射整个文件（64 位地址空间 128TB），RSS 由 OS page cache 管理，
  进程常驻内存不随文件大小增长
- 编码探测：BOM 优先；否则采样头部 64KB，能过 UTF-8 校验（末尾截断的不完整序列不算错）
  就当 UTF-8，否则回落 **GB18030**。每个标签页有手动覆盖菜单
- **匹配在原始字节上做**（`regex::bytes` + `aho-corasick`）：建 matcher 时把关键字按该文件
  的编码编码成字节串，UTF-8 / GB18030 都能正确匹配，且避免逐行解码
- 解码只发生在**可见的 ~60 行**上，成本可忽略
- 已知局限：GB18030 文件 + 含非 ASCII 的**正则**过滤器不保证正确（字面量没问题）。
  UI 需给提示

### 2.4 匹配引擎：一次扫描搞定所有关键字

```rust
pub enum HighlightMode { Line, Field }   // 缺省 Line，对齐 TAT.NET 语义

pub struct FilterSpec {
    enabled: bool,
    excluding: bool,          // .tat 的 excluding="y"
    description: String,
    text: String,
    regex: bool,
    case_sensitive: bool,
    fore: Option<u32>,        // 0xRRGGBB
    back: Option<u32>,
    bold: bool,               // logd 扩展
    italic: bool,             // logd 扩展
    mode: HighlightMode,      // logd 扩展
    extra: Vec<(String, String)>,  // 保留未知属性，回写时原样吐回
}

pub struct MatcherSet {
    ac_cs:  Option<(AhoCorasick, Vec<usize>)>,  // 大小写敏感字面量集
    ac_ci:  Option<(AhoCorasick, Vec<usize>)>,  // 大小写不敏感字面量集
    re_set: Option<(RegexSet, Vec<usize>)>,     // 多正则，判命中
    re_each: Vec<(usize, Regex)>,               // 逐条正则，取 span（只对可见行用）
    has_include: bool,
    has_exclude: bool,
}
```

- 样本 `.tat` 里**全部是 `regex="n"` 字面量** → Aho–Corasick 一次遍历同时命中 20+ 关键字，
  ~1–2 GB/s/核，这是最大的性能杠杆
- **必须用 `find_overlapping_iter` + `MatchKind::Standard`**：非重叠的 leftmost 语义会漏，
  比如模式 `abc` 与 `bcd` 在 `abcd` 中只会命中前者，导致可见性判断出错
- `regex::bytes::RegexSet::matches()` 给出「哪些模式命中」（不给 span），
  span 由 `re_each[i].find_iter()` 单独取——只在渲染 ~60 行时调用，成本无所谓
- 两个入口：
  - `is_visible(line) -> bool`：筛选扫描用。**无 exclude 过滤器时首次命中即早退**
  - `analyze(line, &mut spans) -> Verdict`：只对可见行调用，同时给出行级样式和字段 span

**可见性语义（对齐 TAT.NET）**：

```
include = enabled && !excluding
exclude = enabled &&  excluding
visible = (无 include 过滤器 || 命中至少一个 include) && 命中 0 个 exclude
```

### 2.5 筛选扫描

- 复用索引的 chunk 划分（已知每块 `start_line`），rayon 并行
- 每个 worker 产出局部 `Vec<u64>` 命中行号，按块序合并 → 有序数组
- **取消**：`Progress::cancel`，每 4096 行检查一次
- **渐进结果**：已完成的**前缀连续块**立即合并推给 UI，扫到一半就能看结果
- 命中行号：`Vec<u64>`。1000 万命中 = 80MB。超过 2 亿命中时提示用户收紧条件
- 预期：50GB 全量筛选 **20–40s**，带进度条 + 取消

### 2.6 ⚠️ 渲染：不能用 `uniform_list` 直接铺 5 亿行

GPUI 的 `Pixels` 是 `f32`。5 亿行 × 18px = 9e9 px，f32 尾数 24 位，
超过 ~1.67e7 px（约 **93 万行**）后滚动偏移开始丢精度、抖动、跳行。

**方案：自建视口 + 自建滚动条**

- 视口状态：`anchor_line: u64` + `pixel_offset: f32`（行内平滑偏移）
- 只渲染可见的 N 行：`v_flex().children(rows)`，N ≈ 视口高 / 行高 + 2
- `on_scroll_wheel` → 改 `anchor_line` / `pixel_offset`
- 自绘滚动条：滑块位置 = `anchor_line / total_lines`，拖拽反算行号
  （全程 u64 运算，只在最后一步映射到像素，精度无损）
- 键盘：↑↓ / PageUp/Dn / Ctrl+Home/End / Ctrl+G 跳转行号

行的组成：

```
[ 行号槽 (等宽, 右对齐, 暗色) ][ 内容 (StyledText + highlight runs) ]
```

- **字段模式**：`StyledText::with_highlights(runs)`，run 带 color / background_color /
  font_weight / font_style
- **行模式**：整行底色画在 row 的 `div().bg(...)`，前景色作为该行文字基础色
- **重叠 span 冲突**：按过滤器在列表中的顺序，靠前的优先；渲染前把 span 拍平成
  互不重叠的有序区间
- **超长行**：单行几百 KB 会拖垮 layout。渲染截断到 4096 字符 + `…`（可配置，
  提供「展开本行」）
- **横向滚动**：整个视口共享一个 `h_scroll: f32`，作用在内容容器的 `.left(px(-h_scroll))`

### 2.7 标签页

- 拖入多文件 → `on_drop::<ExternalPaths>` → 每个路径开一个标签
- `rfd` 原生文件对话框（Ctrl+O）
- 标签：文件名标题、全路径 tooltip、关闭按钮、中键关闭、Ctrl+Tab 切换
- **过滤器作用域**：默认全局共享（AE 调试通常一套关键字看多个 log），
  标签页可「分离」成独立副本

### 2.8 `.tat` 读写

`quick-xml` 手写 reader/writer（不用 serde——要保留未知属性和属性顺序）。

扩展属性用 **`logd_` 前缀的普通属性**，不用 XML namespace：
带前缀但未声明 `xmlns:logd` 是非法 XML，而声明了又可能让 TAT.NET 的
XmlSerializer 报错。普通属性名任何宽容解析器都会忽略。

```xml
<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<TextAnalysisTool.NET version="2020-12-17" showOnlyFilteredLines="False">
  <filters>
    <filter enabled="y" excluding="n" description="曝光表"
            foreColor="fa8072" backColor="ffff00"
            type="matches_text" case_sensitive="n" regex="n" text="AEtable"
            logd_mode="field" logd_bold="y" />
  </filters>
</TextAnalysisTool.NET>
```

- 读：`logd_mode` 缺省 → `line`；`logd_bold` / `logd_italic` 缺省 → `n`
- 写：UTF-8 BOM + 2 空格缩进 + 自闭合标签（与 `ae_log_rafe.tat`、`展锐ae.tat` 一致）
- `showOnlyFilteredLines` 双向映射到「仅显示筛选结果」开关
- 颜色 `foreColor` / `backColor` 是**无 `#` 的 6 位 hex RGB** → `gpui::rgb(0x......)`

---

## 3. 工程结构

采用 **workspace 双 crate**（相对原计划的单 crate 有调整）：
把引擎和 UI 拆开，`logd-core` 不依赖 gpui，可以在 gpui 构建风险未排除前
独立 `cargo test -p logd-core`。

```
D:\tuning\tools\AE\logd\
├── Cargo.toml                 # workspace
├── doc/
│   ├── PLAN.md                # 本文件
│   └── PROGRESS.md            # 里程碑与当前进度
├── tat/                       # 已有样本，兼作测试夹具
└── crates/
    ├── logd-core/             # 引擎，无 UI 依赖
    │   ├── examples/
    │   │   └── gen_big_log.rs # 生成 N GB 测试日志
    │   └── src/
    │       ├── lib.rs
    │       ├── progress.rs    # 进度/取消
    │       ├── index.rs       # LineIndex / ChunkIndex / 并行构建
    │       ├── source.rs      # FileSource: mmap + 编码探测
    │       ├── matcher.rs     # MatcherSet: AC + 多正则
    │       ├── scan.rs        # 并行筛选
    │       ├── viewport.rs    # u64 精度滚动数学（见 2.6）
    │       ├── render.rs      # 编码坐标换算 + char 边界 + 超长行截断
    │       ├── document.rs    # 文件行号 ↔ 视图行号双坐标系
    │       ├── cache.rs       # 索引 sidecar 持久化
    │       └── tat.rs         # .tat 读写
    └── logd/                  # bin，GPUI UI
        └── src/
            ├── main.rs        # 入口，解析命令行路径
            ├── app.rs         # 根视图：标签栏 + 工具栏 + 过滤器面板 + 状态栏
            ├── log_view.rs    # 自建视口 + 自绘滚动条 + 后台索引/筛选
            └── theme.rs       # 配色、字号、配色循环表
```

> 与最初设想的差异：滚动数学、编码换算、双坐标系被抽进 `logd-core` 做成纯函数并单测，
> 而不是埋在 UI 里。这三块是最容易出错的地方，隔离出来收益很大。

## 4. 依赖

| crate | 用途 | 来源 |
|---|---|---|
| `gpui` | UI 框架 | zed git HEAD（rev `d7b9b38`） |
| `gpui_platform` | 平台层（git HEAD 已从 gpui 拆出） | zed git HEAD，`features = ["font-kit"]` |
| `gpui-component` | 组件库：Input / Button / TitleBar / Root | longbridge git HEAD（rev `f517e74`） |
| `gpui-component-assets` | 默认图标资源 | longbridge git HEAD |
| `memmap2` 0.9.11 | 文件映射 | crates.io |
| `memchr` 2.8.0 | 换行扫描 | crates.io |
| `aho-corasick` 1.1.4 | 多字面量匹配 | crates.io |
| `regex` 1.12 | 多正则 + span（`regex::bytes`） | crates.io |
| `rayon` 1.12.0 | 并行 | crates.io |
| `quick-xml` 0.36.2 | `.tat` | crates.io |
| `encoding_rs` 0.8.35 | UTF-8 / GB18030 | crates.io |
| `dirs` / `anyhow` / `thiserror` / `parking_lot` | 杂项 | crates.io |

**不用 `rfd`**：`.tat` 靠拖拽加载、Ctrl+S 保存。离线环境下少一个依赖少一分风险。
**不用 `twox-hash`**：缓存文件名去重用内联 FNV-1a 就够。

### ⚠️ gh-proxy 镜像的陷阱

`gpui-component` 自己的 manifest 里，zed 的地址写死的是**原始 github**。
如果我们这边写成 `https://gh-proxy.org/https://github.com/...`，cargo 会按 URL 字符串
判成两个 source，拉进**两份 gpui**，编出互不兼容的 `WindowOptions` / `Render`：

```
error[E0308]: mismatched types
note: there are multiple different versions of crate `gpui` in the dependency graph
```

所以 **zed 那两条必须用原始 github 地址**；`gpui-component` 自己没人跟它抢，可以走镜像。
验证：`grep -c 'gh-proxy.*zed-industries' Cargo.lock` 必须是 0。

⚠️ **git 依赖目前没锁 rev**，zed main 分支随时会变。建议钉死：

```toml
gpui = { git = "https://github.com/zed-industries/zed", rev = "d7b9b38" }
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "d7b9b38", features = ["font-kit"] }
gpui-component = { git = "https://gh-proxy.org/https://github.com/longbridge/gpui-component", rev = "f517e74" }
gpui-component-assets = { git = "https://gh-proxy.org/https://github.com/longbridge/gpui-component", rev = "f517e74" }
```

**profile 注意**：dev 下 `opt-level = 1` + 依赖 `opt-level = 3`。
memchr / aho-corasick 不开优化跑 50GB 会慢到没法用。

---

## 5. 分阶段执行

### M0 — 构建打通（风险最高，先做）

把一个**空的 gpui 窗口**在这台 Windows 机器上编译运行起来。
gpui 在 Windows 上走 DirectX 后端，需要 MSVC 工具链；首次编译 10–25 分钟。
失败就立刻反馈，不往下走。

**验收**：`cargo run -p logd` 弹出窗口，标题 "logd"。

### M1 — 打开大文件 + 虚拟滚动

`source.rs` / `index.rs` / `ui/log_view.rs` / `ui/scrollbar.rs`

**验收**：
- `tools/gen_big_log.rs` 生成 10GB / 50GB 测试文件
- 打开 50GB：首屏 <0.5s，全量索引 <60s（带进度）
- 任务管理器 RSS <300MB
- 拖滚动条到文件末尾不抖动不跳行，行号正确
- Ctrl+G 跳转到第 4 亿行，秒到

### M2 — 过滤 + 双高亮 + 仅显示筛选

`matcher.rs` / `scan.rs` / `ui/toolbar.rs`

**验收**：
- 手输关键字（含正则），字段模式只染词、行模式染整行，颜色/粗体/斜体生效
- 50GB 全量筛选 <60s，进度条可取消，结果渐进出现
- 「全部 / 仅筛选」切换正确，仅筛选模式下**行号仍显示原始行号**
- `excluding` 过滤器正确排除

### M3 — 多标签 + 拖拽

`ui/tabs.rs` / `app.rs`

**验收**：一次拖入 4 个文件 → 4 个标签，独立滚动位置，Ctrl+Tab 切换，关闭释放 mmap。

### M4 — `.tat` 读写 + 过滤器管理面板

`tat.rs` / `ui/filter_panel.rs`

**验收**：
- 加载 `tat/ae_log.tat`（24 条）、`tat/展锐ae.tat`（14 条），
  颜色 / 启用状态 / `showOnlyFilteredLines` 全对
- 面板里改颜色 / 模式 / 启用 → 另存 → 用 TextAnalysisTool.NET 能正常打开
  （不报错、原属性不丢）
- 再用 logd 读回，`logd_mode` / `logd_bold` 保留

> GPUI 原生没有现成文本输入控件。M4 需要输入框时，优先考虑引入
> `gpui-component`（longbridge，提供 TextInput / Table / Tabs），
> 否则手写一个最小单行输入。到 M4 时视情况定，届时同步。

### M5 — 索引缓存 + 编码 + 打磨

`cache.rs` + 编码探测 + 设置持久化

**验收**：二次打开 50GB <1s；GBK 日志中文不乱码；窗口大小 / 最近文件 / 上次 `.tat` 记忆。

---

## 6. 性能目标（M5 实测填表进 README）

| 指标 | 目标 |
|---|---|
| 50GB 首屏 | < 0.5s |
| 50GB 全量索引（首次） | < 60s |
| 50GB 打开（已缓存索引） | < 1s |
| 常驻内存（不含 OS page cache） | < 300MB |
| 滚动帧率 | 60fps |
| 50GB 全量筛选（20 关键字字面量） | < 60s，可取消 |
| 跳转任意行 | < 50ms |

## 7. 验证方式

1. `tools/gen_big_log.rs` 拼接真实 AE 日志到 10GB / 50GB
2. 每个里程碑跑 `cargo run --release -p logd`，任务管理器盯 RSS，秒表计时
3. `.tat` 往返测试：logd 读 → 改 → 写 → TextAnalysisTool.NET 打开验证
4. `cargo test -p logd-core` 覆盖关键路径：
   - 索引正确性（随机行号 vs 朴素逐行读对比）
   - `.tat` 解析 / 序列化往返
   - matcher 语义（include / exclude 组合、重叠模式）

## 8. 已知风险

1. **gpui 在 Windows 上构建** — 最大不确定项，M0 单独隔离验证。
   引擎已拆成独立 crate，即使 gpui 卡住也不阻塞引擎开发
2. **f32 像素精度** — 已用自建视口规避，但滚动条拖拽手感需实测调优
3. **超长单行**（某些平台会打印几百 KB 的 dump）— 渲染截断 + 「展开本行」
4. **mmap 正在被写入的文件** — 文件被截断时访问已映射页会触发异常；
   当前只支持静态文件，实时 tail 不在范围内
5. **GB18030 + 非 ASCII 正则** — 见 2.3，字面量没问题，正则需提示用户
