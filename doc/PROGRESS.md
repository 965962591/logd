# 进度追踪

> 每次继续开发前先读这里，再读 `doc/PLAN.md`。完成一项就更新本文件。

最后更新：2026-09-03

## 当前状态

**引擎层 `logd-core` 已完成并全部通过测试（48/48）。** 正在推 M0（gpui 构建打通）。

### 环境

- `cargo 1.96.0` / `rustc 1.96.0`，Windows 11
- **`gpui = "0.2.2"` 已确认在 crates.io 上**（`Zed's GPU-accelerated UI framework`,
  Apache-2.0, homepage gpui.rs）。不需要挂 git 依赖
- gpui 默认特性 `default = [font-kit, wayland, x11, windows-manifest]`。
  wayland/x11 是 Linux 的，Windows 上应考虑
  `default-features = false, features = ["windows-manifest"]` 来砍构建量——
  **待验证 font-kit 在 Windows 上是否必需**（gpui 在 Windows 走 DirectWrite）
- 其余依赖全部命中本机 cargo cache

### 验证结果

```
cargo test -p logd-core
test result: ok. 48 passed; 0 failed
```

---

## 文件清单

| 文件 | 状态 | 说明 |
|---|---|---|
| `Cargo.toml` | ✅ | workspace；依赖版本钉死到本机缓存版本；dev profile 开 opt-level |
| `doc/PLAN.md` | ✅ | 权威方案 |
| `doc/PROGRESS.md` | ✅ | 本文件 |
| `crates/logd-core/src/lib.rs` | ✅ | 模块声明 + re-export |
| `crates/logd-core/src/progress.rs` | ✅ | `Progress`：原子进度 + 取消 |
| `crates/logd-core/src/index.rs` | ✅ **8 测试通过** | `LineIndex`/`ChunkIndex`/`build_head`/`build_full`/`line_spans` |
| `crates/logd-core/src/source.rs` | ✅ **5 测试通过** | `FileSource`：mmap + BOM/UTF-8/GB18030 探测 + 解码 |
| `crates/logd-core/src/matcher.rs` | ✅ **14 测试通过** | `FilterSpec`/`MatcherSet`/`is_visible`/`analyze`/`flatten_spans` |
| `crates/logd-core/src/scan.rs` | ✅ **7 测试通过** | rayon 并行筛选，`ScanOutcome`，取消 |
| `crates/logd-core/src/tat.rs` | ✅ **9 测试通过** | `.tat` 读写，含真实样本文件的解析与往返测试 |
| `crates/logd-core/src/cache.rs` | ✅ **5 测试通过** | 索引 sidecar 序列化（提前做掉了，原计划在 M5） |
| `crates/logd/Cargo.toml` | ✅ | bin crate，依赖 gpui 0.2.2 |
| `crates/logd/src/main.rs` | 🟡 占位 | 目前是空 `fn main()`，等 gpui 拉完写真窗口 |
| `tools/gen_big_log.rs` | ⬜ 待写 | 生成 N GB 测试日志 |

## 已做的设计决定

1. **拆成 workspace 双 crate** `logd-core` + `logd`，而非原计划的单 crate。
   目的：gpui 构建风险不阻塞引擎开发与测试。已证明有效——引擎在 gpui 还没拉下来时
   就已经全绿
2. **块大小 64MiB**（非 `num_cpus` 等分）。50GB → 800 块，进度条更平滑、rayon 负载更均衡
3. **暂缓 LRU 锚点缓存**。连续滚动每帧最多回扫 100KB，60fps 才 6MB/s，先不加复杂度
4. **AC 必须用 `find_overlapping_iter` + `MatchKind::Standard`**。
   非重叠 leftmost 语义会漏匹配（`abc`/`bcd` 在 `abcd` 中只命中前者），
   导致可见性判断出错。已有回归测试 `overlapping_patterns_all_reported`
5. **扩展属性用 `logd_mode` 而不是 `logd:mode`**。无 namespace 声明的前缀是非法 XML，
   声明了又可能让 TAT.NET 的 XmlSerializer 报错
6. **`scan_all` 返回 `ScanOutcome` 枚举而非 `Vec<u64>`**。
   「没有任何过滤器」和「筛选出 0 行」必须区分开，否则会误进空白的筛选视图
7. **`cache.rs` 提前实现**（原计划 M5）。逻辑自洽、易测，顺手做掉降低后期风险
8. **不引入 `twox-hash`**。缓存文件名去重用内联 FNV-1a 就够，少一个依赖

## 事实修正

- `tat/ae_log.tat` 是 **24 条**过滤器，不是 27 条（`ae_log_rafe.tat` 也是 24，
  `展锐ae.tat` 是 14）。PLAN 里的 27 是我数错了

## 里程碑

- [ ] **M0** 空 gpui 窗口在本机编译运行 ← **当前**
- [ ] **M1** 打开大文件 + 虚拟滚动（引擎侧 `index.rs`/`source.rs` 已就绪且测试通过）
- [ ] **M2** 过滤 + 双高亮 + 仅显示筛选（引擎侧 `matcher.rs`/`scan.rs` 已就绪）
- [ ] **M3** 多标签 + 拖拽
- [ ] **M4** `.tat` 读写 + 过滤器管理面板（引擎侧 `tat.rs` 已就绪）
- [ ] **M5** 索引缓存（引擎侧 `cache.rs` 已就绪）+ 编码菜单 + 设置持久化

## 下一步（按顺序）

1. `cargo fetch` 拉完 gpui 依赖树（**正在后台跑**，耗时很久）
2. 读 `~/.cargo/registry/src/*/gpui-0.2.2/` 下的 `examples/` 和 `Cargo.toml`，
   确认 0.2.x 的 API 形态和 Windows 上该开哪些 feature
3. 写 `crates/logd/src/main.rs` 的空窗口，`cargo run -p logd` 验收 M0
4. 进 M1：`ui/log_view.rs` 自建视口 + `ui/scrollbar.rs`
5. 写 `tools/gen_big_log.rs`，造 10GB/50GB 测试文件跑性能验收
