# 进度追踪

> 每次继续开发前先读这里，再读 `doc/PLAN.md`。完成一项就更新本文件。

最后更新：2026-09-04

## 当前状态

**M0–M5 的代码全部写完并编译通过，101 个测试全绿。尚未做真机性能验收。**

```
cargo build --release --workspace     # 通过
cargo test --workspace                # 101 passed; 0 failed
```

| | 引擎 | UI | 验收 |
|---|---|---|---|
| M0 gpui 起窗口 | — | ✅ | ✅ 已跑起来过（crates.io 版），git 版编过但**未实跑** |
| M1 大文件 + 虚拟滚动 | ✅ | ✅ | ⬜ **待你验证** |
| M2 过滤 + 双高亮 + 仅筛选 | ✅ | ✅ | ⬜ |
| M3 多标签 + 拖拽 | — | ✅ | ⬜ |
| M4 `.tat` 读写 + 过滤器面板 | ✅ | ✅ | ⬜ |
| M5 索引缓存 + 编码 | ✅ | ✅ | ⬜ |

---

## 怎么跑

```bash
cd D:/tuning/tools/AE/logd

# 造测试日志（GB 数, 输出路径）。行的形态照抄真实 AE logcat，
# 并按 tat/ae_log.tat 的关键字掺入命中行，命中率约 1/12
cargo run --release -p logd-core --example gen_big_log -- 10 D:/tmp/big.log

# 启动。命令行可直接给路径：日志开标签页，.tat 当配置加载
./target/release/logd.exe D:/tmp/big.log tat/ae_log.tat
```

### 操作

| 操作 | 说明 |
|---|---|
| 拖入文件 | 日志 → 新标签页；`.tat` → 加载关键字配置 |
| 关键字框回车 | 在所有已导入文件中执行临时搜索（用 `|` 分隔关键词） |
| 跳转框回车 | 跳到指定行号（1-based，可带千分位逗号） |
| 日志正文点击 | 直接在点击位置显示光标；鼠标拖选限定在当前行 |
| `仅显示筛选` | 只列命中行，**行号槽仍显示原始文件行号** |
| `过滤器面板` | 每行：启用 / 包含-排除 / 字段-整行 / 正则 / 删除；前景色和背景色仅在编辑表单中设置 |
| 编码按钮 | UTF-8 ↔ GB18030 切换，会按新编码重建 matcher |
| `保存 .tat` 或 Ctrl+S | 写回加载来源；没有来源就写到当前日志同名 `.tat` |
| Ctrl+Tab / Ctrl+W | 切换 / 关闭标签页 |
| ↑↓ PageUp/Dn | 逐行 / 翻页；Ctrl+Home/End 到首尾 |
| Home/End ←→ | 横向滚动 |
| 滚轮 / 拖滚动条 | 纵向滚动；Shift+滚轮横向 |

---

## ⚠️ 两个大坑（都已解决，别再踩）

### 1. gh-proxy 镜像导致依赖图里有**两份 gpui**

症状：

```
error[E0308]: mismatched types
  expected `WindowOptions`, found `gpui::platform::WindowOptions`
note: there are multiple different versions of crate `gpui` in the dependency graph
```

原因：workspace 用了 `https://gh-proxy.org/https://github.com/zed-industries/zed`，
但 **gpui-component 自己的 manifest 里写死的是原始 github 地址**。
cargo 按 URL 字符串区分 source，两个地址 = 两套互不兼容的 gpui 类型。

修法：**zed 那两条必须用原始 github 地址**，和 gpui-component 内部声明对齐。
`gpui-component` 自己没人跟它抢，可以继续走镜像。见根 `Cargo.toml` 里的注释。

验证：`grep -c 'gh-proxy.*zed-industries' Cargo.lock` 必须是 **0**。

### 2. gpui 的 `Pixels` 是 f32，撑不住 5 亿行

5 亿行 × 18px = 9e9 px，f32 尾数 24 位，超过约 1.67e7 px（≈**93 万行**）滚动偏移
就丢精度、抖动、跳行。所以 `uniform_list` / `virtual_list` 在这个量级**都不能用**。

方案见 `crates/logd-core/src/viewport.rs`：位置存成 `(anchor_line: u64, pixel_offset: f32)`，
比例运算全走 f64，只在最后一步落像素。回归测试 `thumb_drag_round_trips_at_500m_lines`
和 `single_line_steps_are_exact_near_end` 专盯这个。

---

## 环境

- `cargo 1.96.0` / `rustc 1.96.0`，Windows 11
- gpui 走 zed git HEAD（rev `d7b9b38`），gpui-component git HEAD（rev `f517e74`）
- Windows 后端 = DirectX 11 + DirectWrite
- ⚠️ **git 依赖没锁 rev**，zed main 分支随时变。建议把上面两个 rev 写进 `Cargo.toml`
- 首次编译 gpui 全家桶约 40 分钟；之后改 logd 自己的代码约 10 秒

## 文件清单

### `crates/logd-core`（引擎，无 UI 依赖，98 测试）

| 文件 | 测试 | 职责 |
|---|---|---|
| `progress.rs` | — | 原子进度 + 取消信号 |
| `index.rs` | 8 | `LineIndex`/`ChunkIndex`；`build_head` 首屏、`build_full` 并行全量、`line_spans` 定位 |
| `source.rs` | 5 | `FileSource`：mmap + BOM/UTF-8/GB18030 探测 |
| `matcher.rs` | 14 | `FilterSpec`/`MatcherSet`；AC 多字面量 + RegexSet；`is_visible`/`analyze` |
| `scan.rs` | 7 | rayon 并行筛选，`ScanOutcome`，取消 |
| `viewport.rs` | 23 | **u64 精度滚动数学**，滑块位置/拖拽反算 |
| `render.rs` | 12 | **GB18030 span 坐标换算 + char 边界 + 超长行截断** |
| `document.rs` | 15 | 文件行号 ↔ 视图行号双坐标系、筛选视图、编码覆盖 |
| `tat.rs` | 9 | `.tat` 读写，含真实样本解析与往返 |
| `cache.rs` | 5 | 索引 sidecar 序列化 |
| `examples/gen_big_log.rs` | — | 造 N GB 合成 AE 日志 |

### `crates/logd`（UI，3 测试）

| 文件 | 职责 |
|---|---|
| `main.rs` | 入口，解析命令行路径 |
| `app.rs` | 根视图：标签栏 + 工具栏 + 过滤器面板 + 状态栏 + 拖放 + `.tat` 读写 |
| `log_view.rs` | 日志视口：自建滚动 + 自绘滚动条 + 行渲染 + 后台索引/筛选 |
| `theme.rs` | 配色与字号常量 |

## 关键设计决定

1. **workspace 双 crate**。引擎脱离 gpui 可单测——事实证明很值：gpui 还没拉下来时引擎就已经全绿，
   后来切 git 依赖重编也没影响引擎
2. **分块稀疏索引**，每 1024 行一个锚点。5 亿行索引只占 ~4MB
3. **块大小 64MiB**（非 `num_cpus` 等分）。50GB → 800 块，进度条平滑 + rayon 负载均衡
4. **AC 必须用 `find_overlapping_iter` + `MatchKind::Standard`**。非重叠 leftmost 会漏匹配
   （`abc`/`bcd` 在 `abcd` 中只命中前者），可见性算错。回归测试 `overlapping_patterns_all_reported`
5. **匹配跑原始字节，渲染用解码字符串**。GB18030 下两个坐标系不同，`render.rs` 负责换算；
   不换算高亮会错位，且 `StyledText` 对非 char 边界会 `debug_assert` 炸
6. **扩展属性用 `logd_mode` 不是 `logd:mode`**。无 namespace 声明的前缀是非法 XML，
   声明了又可能让 TAT.NET 的 XmlSerializer 报错
7. **`scan_all` 返回 `ScanOutcome` 枚举**。「没有任何过滤器」和「筛选出 0 行」必须区分，
   否则会误进空白的筛选视图
8. **`scan_gen` 代际号**。用户连续改过滤器时，旧扫描的结果必须丢弃
9. **过滤器全局共享，非活动标签页只打 dirty 标记**。否则改一次关键字要同时扫 4 个 50GB 文件
10. **编码存在 `Document` 而不是 `FileSource`**。后者在 `Arc` 里改不动
11. **不引入 `rfd`**。`.tat` 靠拖拽加载、Ctrl+S 保存，少一个依赖少一分离线风险
12. **不引入 `twox-hash`**。缓存文件名去重用内联 FNV-1a 就够

## 事实修正

- `tat/ae_log.tat` 是 **24 条**过滤器（`ae_log_rafe.tat` 24，`展锐ae.tat` 14），PLAN 早期写的 27 是数错了
- gpui **已在 crates.io 上**（0.2.2），原计划「必须挂 git rev」的判断过时；但按要求最终仍走 git HEAD
- `cache.rs` 提前到 M1 之前就做掉了（原计划 M5）

## 待验证 / 已知欠缺

**性能验收全部待做**（见 `doc/PLAN.md` 第 6 节的指标表）：
50GB 首屏 <0.5s、全量索引 <60s、缓存命中 <1s、RSS <300MB、60fps、全量筛选 <60s。

已知欠缺：

1. **git rev 未锁定** —— 最该先补的
2. **拖滚动条时鼠标移出视口会断开**。`on_mouse_move` 挂在视口 div 上，没做窗口级捕获
3. **筛选视图逐行定位**：命中行号是跳跃的，每行都要走一次锚点定位（最坏回扫 1024 行）。
   全部视图是一次定位后顺序扫，快得多。60 行 × 100KB = 6MB/帧，能跑但可以优化
4. **渐进筛选结果没做**。PLAN 2.5 说的「前缀连续块先出结果」还是等全部扫完才刷新
5. **超长行只截断，没有「展开本行」**
6. **GB18030 + 非 ASCII 正则**不保证正确（字面量没问题），UI 也没给提示
7. **LRU 锚点缓存暂缓**，等 profiling 说话
8. **实时 tail 不支持**。mmap 期间文件被截断会触发异常
9. 窗口大小 / 最近文件 / 上次 `.tat` **没做持久化**
