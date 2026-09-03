# 进度追踪

> 每次继续开发前先读这里，再读 `doc/PLAN.md`。完成一项就更新本文件。

最后更新：2026-09-03

## 当前状态

- **引擎层 `logd-core` 完成，48/48 测试通过。**
- **M0 已通过一次**（用 crates.io 的 `gpui 0.2.2`）：`logd.exe` 起窗口、存活 10s 不崩。
- **正在切换到 git 依赖**（按 gpui-component 官网的安装方式），需要重新验证 M0。

### 环境

- `cargo 1.96.0` / `rustc 1.96.0`，Windows 11
- Windows 后端走 **DirectX 11 + DirectWrite**（gpui 里由 target-gated 的 `windows` crate 提供）
- 除 gpui 系列外，所有依赖都命中本机 cargo cache

### UI 依赖：走 git（用户指定，按官网 README）

```toml
gpui = { git = "https://github.com/zed-industries/zed" }
gpui_platform = { git = "https://github.com/zed-industries/zed", features = ["font-kit"] }
gpui-component = { git = "https://github.com/longbridge/gpui-component" }
gpui-component-assets = { git = "https://github.com/longbridge/gpui-component" }
```

⚠️ **待办：编译通过后把 git rev 钉死**。现在没锁 rev，zed main 分支随时变，
构建不可复现，某天突然编不过会很难查。

⚠️ git HEAD 把平台层拆成了独立的 `gpui_platform` crate，说明 API 与 crates.io 的
`0.2.2` **不同**。M0 那份 `main.rs`（基于 0.2.2 的 `Application::new().run()`）
大概率要照 gpui-component 的 examples 重写。

### 组件库调研结论

| crate | 结论 |
|---|---|
| `gpui-kit` 0.1.0 | ❌ **空壳**。整个 crate 只有 `fn main() { println!("Hello, world!"); }`，无 lib target、无依赖。longbridge 占的名，尚未实现 |
| `gpui-component` 0.5.1 (crates.io) | ✅ 真库，同一组织。crates.io 版 pin `gpui 0.2.2` |
| `gpui-component` (git HEAD) | ✅ **当前采用**。配套 zed git HEAD 的 gpui |

`gpui-component` 提供的、我们要用的组件：
`tab/`（M3 标签栏）、`input/`（M2 关键字框、M4 过滤器编辑）、`table/`（M4 过滤器面板）、
`color_picker.rs`（M4 配色）、`scroll/`、`theme/`、`root.rs`、`title_bar.rs`、
`resizable/`、`menu/`、`progress.rs`、`dialog.rs`、`virtual_list.rs`。
默认特性为空（`decimal`/`inspector`/`tree-sitter-languages`/`webview` 都是可选），
不会拖进 tree-sitter 那一大坨。

**但 `log_view` 仍然必须自己写**：见 PLAN 2.6，gpui 的 `Pixels` 是 f32，
任何基于像素总高的虚拟列表（含 `uniform_list` 和大概率 `virtual_list`）
在约 93 万行之后就丢精度。5 亿行必须用「u64 锚点行号 + 自绘滚动条」。
gpui-component 帮的是**外围控件**，核心视口是自研。

### 验证结果

```
cargo test -p logd-core
test result: ok. 48 passed; 0 failed
```

```
cargo build -p logd            # gpui 0.2.2, 37m33s, exit 0
target/debug/logd.exe          # 8.0 MB，存活 10s 未崩溃
```

---

## 文件清单

| 文件 | 状态 | 说明 |
|---|---|---|
| `Cargo.toml` | ✅ | workspace；UI 依赖已切 git |
| `doc/PLAN.md` | ✅ | 权威方案 |
| `doc/PROGRESS.md` | ✅ | 本文件 |
| `crates/logd-core/src/lib.rs` | ✅ | 模块声明 + re-export |
| `crates/logd-core/src/progress.rs` | ✅ | `Progress`：原子进度 + 取消 |
| `crates/logd-core/src/index.rs` | ✅ **8 测试** | `LineIndex`/`ChunkIndex`/`build_head`/`build_full`/`line_spans` |
| `crates/logd-core/src/source.rs` | ✅ **5 测试** | `FileSource`：mmap + BOM/UTF-8/GB18030 探测 + 解码 |
| `crates/logd-core/src/matcher.rs` | ✅ **14 测试** | `FilterSpec`/`MatcherSet`/`is_visible`/`analyze`/`flatten_spans` |
| `crates/logd-core/src/scan.rs` | ✅ **7 测试** | rayon 并行筛选，`ScanOutcome`，取消 |
| `crates/logd-core/src/tat.rs` | ✅ **9 测试** | `.tat` 读写，含真实样本解析与往返 |
| `crates/logd-core/src/cache.rs` | ✅ **5 测试** | 索引 sidecar 序列化（提前做掉，原计划 M5） |
| `crates/logd-core/examples/gen_big_log.rs` | ✅ | 生成 N GB 合成 AE 日志 |
| `crates/logd/Cargo.toml` | ✅ | 已接到 workspace 的 git 依赖 |
| `crates/logd/src/main.rs` | 🟡 **需重写** | 现在是基于 gpui 0.2.2 API 的空窗口，git HEAD API 变了 |
| `crates/logd/src/app.rs` | ⬜ | 根视图：标签栏 + 活动页 + 状态栏 |
| `crates/logd/src/doc.rs` | ⬜ | `LogDocument`：source+index+matches+视图状态 |
| `crates/logd/src/ui/log_view.rs` | ⬜ | **自建视口**（M1 核心） |
| `crates/logd/src/ui/scrollbar.rs` | ⬜ | u64 精度滚动条 |

## 已确认的 gpui API（0.2.2 版，git HEAD 需复核）

- 引导：`Application::new().run(|cx: &mut App| { cx.open_window(WindowOptions{..}, |_w, cx| cx.new(|_| View)) })`
- 视图：`impl Render for V { fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement }`
- 样式：`div().flex().flex_col().size_full().bg(rgb(0x1e1e1e)).text_color(..).child(..)`
- 富文本：`StyledText::new(text).with_highlights(impl IntoIterator<Item = (Range<usize>, HighlightStyle)>)`
  —— range 是**字节**下标，且**必须落在 char 边界**（debug_assert 会炸）
- `HighlightStyle { color, font_weight, font_style, background_color, underline, strikethrough, fade_out }`
- 文件拖放：`ExternalPaths::paths() -> &[PathBuf]`，配 `.on_drop(cx.listener(|this, paths: &ExternalPaths, w, cx| ..))`
- 后台任务：`cx.spawn(async move |weak_this, cx| ...)`（`AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R`）
  + `cx.background_executor().spawn(fut)`（要求 `Send`）
- 定时器：`background_executor().timer(Duration) -> Task<()>`
- 事件回调：`cx.listener(|this, ev, window, cx| ..)`

## 已做的设计决定

1. **拆成 workspace 双 crate** `logd-core` + `logd`。已证明有效——引擎在 gpui 还没拉下来时就全绿了，
   现在切 git 依赖重编也不影响引擎
2. **块大小 64MiB**（非 `num_cpus` 等分）。50GB → 800 块，进度条更平滑、rayon 负载更均衡
3. **暂缓 LRU 锚点缓存**。连续滚动每帧最多回扫 100KB，60fps 才 6MB/s
4. **AC 必须用 `find_overlapping_iter` + `MatchKind::Standard`**。非重叠 leftmost 会漏匹配
   （`abc`/`bcd` 在 `abcd` 中只命中前者），可见性算错。回归测试：`overlapping_patterns_all_reported`
5. **扩展属性用 `logd_mode` 而不是 `logd:mode`**。无 namespace 声明的前缀是非法 XML
6. **`scan_all` 返回 `ScanOutcome` 枚举**。「没有任何过滤器」和「筛选出 0 行」必须区分
7. **`cache.rs` 提前实现**（原计划 M5）
8. **不引入 `twox-hash`**。缓存文件名去重用内联 FNV-1a 就够

## 事实修正

- `tat/ae_log.tat` 是 **24 条**过滤器，不是 27（`ae_log_rafe.tat` 24，`展锐ae.tat` 14）
- gpui **已在 crates.io 上**（0.2.2），原计划里「必须挂 git rev」的判断过时了；
  但按用户要求最终仍走 git HEAD

## 里程碑

- [x] **M0** 空 gpui 窗口编译运行（gpui 0.2.2 已过；切 git 后**待重新验证**）
- [ ] **M1** 打开大文件 + 虚拟滚动 ← **当前**（引擎侧 `index.rs`/`source.rs` 就绪）
- [ ] **M2** 过滤 + 双高亮 + 仅显示筛选（引擎侧 `matcher.rs`/`scan.rs` 就绪）
- [ ] **M3** 多标签 + 拖拽
- [ ] **M4** `.tat` 读写 + 过滤器管理面板（引擎侧 `tat.rs` 就绪）
- [ ] **M5** 索引缓存（引擎侧 `cache.rs` 就绪）+ 编码菜单 + 设置持久化

## 下一步（按顺序）

1. `cargo fetch` 拉 zed + gpui-component 的 git 仓库（**正在后台跑**，zed 仓库很大）
2. 读 `~/.cargo/git/checkouts/gpui-component-*/examples/` 里的引导代码，
   确认 git HEAD 的 `Application` / `gpui_platform` / `Root` / `Assets` 正确用法
3. 照新 API 重写 `crates/logd/src/main.rs`，`cargo run -p logd` 重新验收 M0
4. **把 git rev 钉死**写回 workspace Cargo.toml
5. 进 M1：`doc.rs` + `ui/log_view.rs`（自建视口）+ `ui/scrollbar.rs`
6. `cargo run --release -p logd-core --example gen_big_log -- 10 D:/tmp/big.log` 造测试文件，
   跑 M1 性能验收
