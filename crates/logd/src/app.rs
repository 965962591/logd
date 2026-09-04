//! 根视图：标签栏 + 工具栏 + 过滤器面板 + 日志视口 + 状态栏。
//!
//! 过滤器**全局共享**（AE 调试通常一套关键字看多个 log）。改动只对当前标签页立刻
//! 重扫，其它标签页打个 dirty 标记，切过去时才扫——否则一改关键字就要同时扫 4 个
//! 50GB 文件。

use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{h_flex, v_flex, Root, Sizable, TitleBar};
use logd_core::{Encoding, FilterSpec, HighlightMode, TatFile};

use crate::log_view::LogView;
use crate::theme;

struct Tab {
    view: Entity<LogView>,
    title: String,
    path: PathBuf,
}

pub struct LogdApp {
    tabs: Vec<Tab>,
    active: usize,
    /// 全局共享的过滤器集合，对应一个 `.tat` 文件的内容
    filters: Vec<FilterSpec>,
    show_only_filtered: bool,
    /// 上次加载/保存的 `.tat` 路径，Ctrl+S 写回这里
    tat_path: Option<PathBuf>,
    keyword: Entity<InputState>,
    goto: Entity<InputState>,
    panel_open: bool,
    status: Option<String>,
    focus: FocusHandle,
}

impl LogdApp {
    pub fn new(initial: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let keyword = cx.new(|cx| InputState::new(window, cx).placeholder("关键字，回车添加"));
        let goto = cx.new(|cx| InputState::new(window, cx).placeholder("跳转行号"));

        cx.subscribe_in(&keyword, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let text = state.read(cx).value().to_string();
                this.add_keyword(text, window, cx);
            }
        })
        .detach();

        cx.subscribe_in(&goto, window, |this, state, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let text = state.read(cx).value().to_string();
                this.goto_line(&text, cx);
            }
        })
        .detach();

        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            filters: Vec::new(),
            show_only_filtered: false,
            tat_path: None,
            keyword,
            goto,
            panel_open: true,
            status: None,
            focus: cx.focus_handle(),
        };
        for p in initial {
            app.open_path(&p, window, cx);
        }
        app
    }

    // ---- 打开 ----

    /// `.tat` 当配置加载，其它一律当日志开新标签页。
    pub fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tat")) {
            self.load_tat(path, cx);
        } else {
            self.open_log(path, window, cx);
        }
    }

    fn open_log(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // 已经开过就直接切过去，别开重复标签
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.set_active(i, cx);
            return;
        }
        match LogView::load(path) {
            Ok(loaded) => {
                let view = cx.new(|cx| LogView::new(loaded, window, cx));
                if !self.filters.is_empty() {
                    let filters = self.filters.clone();
                    let only = self.show_only_filtered;
                    view.update(cx, |v, cx| {
                        v.apply_filters(filters, cx);
                        v.set_show_only_filtered(only, cx);
                    });
                }
                self.tabs.push(Tab {
                    view,
                    title: path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                    path: path.to_path_buf(),
                });
                self.active = self.tabs.len() - 1;
                self.status = None;
            }
            Err(e) => self.status = Some(format!("打不开：{e:#}")),
        }
        cx.notify();
    }

    fn set_active(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.tabs.len() {
            return;
        }
        self.active = i;
        // 切过来时才把攒下的过滤器改动兑现
        let filters = self.filters.clone();
        let only = self.show_only_filtered;
        let view = self.tabs[i].view.clone();
        if view.read(cx).is_dirty() {
            view.update(cx, |v, cx| {
                v.apply_filters(filters, cx);
                v.set_show_only_filtered(only, cx);
            });
        }
        cx.notify();
    }

    fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        if i >= self.tabs.len() {
            return;
        }
        self.tabs.remove(i); // Entity 释放 → mmap 解除映射
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        cx.notify();
    }

    fn active_view(&self) -> Option<&Entity<LogView>> {
        self.tabs.get(self.active).map(|t| &t.view)
    }

    // ---- 过滤器 ----

    /// 过滤器变了：当前标签页立刻重扫，其它的打标记。
    fn filters_changed(&mut self, cx: &mut Context<Self>) {
        let filters = self.filters.clone();
        let only = self.show_only_filtered;
        for (i, tab) in self.tabs.iter().enumerate() {
            if i == self.active {
                let f = filters.clone();
                tab.view.update(cx, |v, cx| {
                    v.apply_filters(f, cx);
                    v.set_show_only_filtered(only, cx);
                });
            } else {
                tab.view.update(cx, |v, _| v.mark_dirty());
            }
        }
        cx.notify();
    }

    fn add_keyword(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.filters.push(FilterSpec {
            text,
            // 新加的默认字段模式：AE 调试更常见的是「把关键词点亮」而不是整行刷底色
            mode: HighlightMode::Field,
            fore: Some(theme::PALETTE[self.filters.len() % theme::PALETTE.len()]),
            ..Default::default()
        });
        self.keyword
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.filters_changed(cx);
    }

    fn goto_line(&mut self, text: &str, cx: &mut Context<Self>) {
        let Ok(n) = text.trim().replace(',', "").parse::<u64>() else {
            return;
        };
        if let Some(v) = self.active_view().cloned() {
            // 用户输入是 1-based
            v.update(cx, |v, cx| v.goto_line(n.saturating_sub(1), cx));
        }
    }

    fn toggle_show_only(&mut self, cx: &mut Context<Self>) {
        self.show_only_filtered = !self.show_only_filtered;
        let only = self.show_only_filtered;
        if let Some(v) = self.active_view().cloned() {
            v.update(cx, |v, cx| v.set_show_only_filtered(only, cx));
        }
        cx.notify();
    }

    fn toggle_encoding(&mut self, cx: &mut Context<Self>) {
        let Some(v) = self.active_view().cloned() else {
            return;
        };
        let next = match v.read(cx).doc().encoding() {
            Encoding::Utf8 => Encoding::Gb18030,
            Encoding::Gb18030 => Encoding::Utf8,
        };
        let filters = self.filters.clone();
        v.update(cx, |v, cx| v.set_encoding(next, filters, cx));
        cx.notify();
    }

    // ---- .tat ----

    fn load_tat(&mut self, path: &Path, cx: &mut Context<Self>) {
        match TatFile::load(path) {
            Ok(t) => {
                self.filters = t.filters;
                self.show_only_filtered = t.show_only_filtered;
                self.tat_path = Some(path.to_path_buf());
                self.status = Some(format!("已加载 {} 条过滤器", self.filters.len()));
                self.filters_changed(cx);
            }
            Err(e) => {
                self.status = Some(format!("读 .tat 失败：{e:#}"));
                cx.notify();
            }
        }
    }

    fn save_tat(&mut self, cx: &mut Context<Self>) {
        // 没来源就存到当前日志旁边
        let path = self.tat_path.clone().or_else(|| {
            self.tabs
                .get(self.active)
                .map(|t| t.path.with_extension("tat"))
        });
        let Some(path) = path else {
            self.status = Some("没有可保存的位置".into());
            cx.notify();
            return;
        };
        let file = TatFile {
            show_only_filtered: self.show_only_filtered,
            filters: self.filters.clone(),
            ..Default::default()
        };
        self.status = Some(match file.save(&path) {
            Ok(()) => {
                self.tat_path = Some(path.clone());
                format!("已保存 {}", path.display())
            }
            Err(e) => format!("保存失败：{e:#}"),
        });
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if !ks.modifiers.control {
            return;
        }
        match ks.key.as_str() {
            "s" => self.save_tat(cx),
            "tab" => {
                if !self.tabs.is_empty() {
                    let next = (self.active + 1) % self.tabs.len();
                    self.set_active(next, cx);
                }
            }
            "w" => self.close_tab(self.active, cx),
            _ => {}
        }
    }

    // ---- 渲染 ----

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .h(px(28.))
            .bg(theme::c(theme::GUTTER_BG))
            .border_b_1()
            .border_color(theme::c(theme::BORDER))
            .text_size(px(12.))
            .children(self.tabs.iter().enumerate().map(|(i, t)| {
                let active = i == self.active;
                h_flex()
                    .id(("tab", i))
                    .h_full()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_r_1()
                    .border_color(theme::c(theme::BORDER))
                    .bg(theme::c(if active { theme::BG } else { theme::GUTTER_BG }))
                    .text_color(theme::c(if active { theme::FG } else { theme::MUTED }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                        this.set_active(i, cx)
                    }))
                    .child(t.title.clone())
                    .child(
                        div()
                            .id(("close", i))
                            .px_1()
                            .text_color(theme::c(theme::MUTED))
                            .child("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.close_tab(i, cx)
                            })),
                    )
            }))
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let enc = self
            .active_view()
            .map(|v| v.read(cx).doc().encoding().label())
            .unwrap_or("—");

        h_flex()
            .w_full()
            .h(px(34.))
            .px_2()
            .gap_2()
            .items_center()
            .bg(theme::c(theme::GUTTER_BG))
            .border_b_1()
            .border_color(theme::c(theme::BORDER))
            .text_size(px(12.))
            .child(div().w(px(220.)).child(Input::new(&self.keyword).small()))
            .child(div().w(px(110.)).child(Input::new(&self.goto).small()))
            .child(
                self.toggle_chip(
                    "only-filtered",
                    "仅显示筛选",
                    self.show_only_filtered,
                    cx.listener(|this, _: &ClickEvent, _w, cx| this.toggle_show_only(cx)),
                ),
            )
            .child(
                self.toggle_chip(
                    "panel",
                    "过滤器面板",
                    self.panel_open,
                    cx.listener(|this, _: &ClickEvent, _w, cx| {
                        this.panel_open = !this.panel_open;
                        cx.notify();
                    }),
                ),
            )
            .child(
                self.toggle_chip(
                    "encoding",
                    enc,
                    false,
                    cx.listener(|this, _: &ClickEvent, _w, cx| this.toggle_encoding(cx)),
                ),
            )
            .child(
                Button::new("save-tat")
                    .small()
                    .label("保存 .tat")
                    .on_click(cx.listener(|this, _, _w, cx| this.save_tat(cx))),
            )
            .into_any_element()
    }

    /// 一个小方块开关。用裸 div 而不是 Checkbox，样式更贴近这个工具的密度。
    fn toggle_chip(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        on: bool,
        click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        h_flex()
            .id(id)
            .px_2()
            .h(px(22.))
            .items_center()
            .rounded_sm()
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .bg(theme::c(if on { theme::STATUS_BG } else { theme::BG }))
            .text_color(theme::c(if on { theme::STATUS_FG } else { theme::FG }))
            .child(label.into())
            .on_click(click)
            .into_any_element()
    }

    /// 颜色小方块。`None` 时画成暗灰的空位，点一下换 [`theme::next_color`] 的下一个。
    fn swatch(
        &self,
        id: (&'static str, usize),
        glyph: &'static str,
        color: Option<u32>,
        click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .w(px(18.))
            .text_color(theme::c(color.unwrap_or(theme::BORDER)))
            .child(glyph)
            .on_click(click)
            .into_any_element()
    }

    fn render_filter_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .max_h(px(200.))
            .overflow_y_scrollbar()
            .id("filter-panel")
            .bg(theme::c(theme::BG))
            .border_b_1()
            .border_color(theme::c(theme::BORDER))
            .text_size(px(12.))
            .children(self.filters.iter().enumerate().map(|(i, f)| {
                h_flex()
                    .w_full()
                    .h(px(24.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    // 启用
                    .child(
                        div()
                            .id(("en", i))
                            .w(px(18.))
                            .child(if f.enabled { "☑" } else { "☐" })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.filters[i].enabled = !this.filters[i].enabled;
                                this.filters_changed(cx);
                            })),
                    )
                    // 排除
                    .child(
                        div()
                            .id(("ex", i))
                            .w(px(28.))
                            .text_color(theme::c(if f.excluding { 0xf48771 } else { theme::MUTED }))
                            .child(if f.excluding { "排除" } else { "包含" })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.filters[i].excluding = !this.filters[i].excluding;
                                this.filters_changed(cx);
                            })),
                    )
                    // 高亮模式：每条过滤器独立
                    .child(
                        div()
                            .id(("mode", i))
                            .w(px(34.))
                            .text_color(theme::c(theme::MUTED))
                            .child(match f.mode {
                                HighlightMode::Field => "字段",
                                HighlightMode::Line => "整行",
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.filters[i].mode = match this.filters[i].mode {
                                    HighlightMode::Field => HighlightMode::Line,
                                    HighlightMode::Line => HighlightMode::Field,
                                };
                                this.filters_changed(cx);
                            })),
                    )
                    // 正则
                    .child(
                        div()
                            .id(("re", i))
                            .w(px(28.))
                            .text_color(theme::c(if f.regex { theme::FG } else { theme::MUTED }))
                            .child(".*")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.filters[i].regex = !this.filters[i].regex;
                                this.filters_changed(cx);
                            })),
                    )
                    // 前景色 / 背景色，点一下换下一个
                    .child(self.swatch(("fg", i), "A", f.fore, cx.listener(
                        move |this, _: &ClickEvent, _w, cx| {
                            this.filters[i].fore = theme::next_color(this.filters[i].fore);
                            this.filters_changed(cx);
                        },
                    )))
                    .child(self.swatch(("bg", i), "■", f.back, cx.listener(
                        move |this, _: &ClickEvent, _w, cx| {
                            this.filters[i].back = theme::next_color(this.filters[i].back);
                            this.filters_changed(cx);
                        },
                    )))
                    // 关键字本体 + 描述
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .when_some(f.fore, |el, c| el.text_color(theme::c(c)))
                            .child(f.text.clone()),
                    )
                    .child(
                        div()
                            .w(px(160.))
                            .overflow_hidden()
                            .text_color(theme::c(theme::MUTED))
                            .child(f.description.clone()),
                    )
                    .child(
                        div()
                            .id(("del", i))
                            .px_1()
                            .text_color(theme::c(theme::MUTED))
                            .child("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.filters.remove(i);
                                this.filters_changed(cx);
                            })),
                    )
            }))
            .when(self.filters.is_empty(), |el| {
                el.child(
                    div()
                        .p_2()
                        .text_color(theme::c(theme::MUTED))
                        .child("还没有过滤器。上面输入关键字回车添加，或把 .tat 文件拖进来。"),
                )
            })
            .into_any_element()
    }

    fn render_status(&self, cx: &App) -> AnyElement {
        let mut bar = h_flex()
            .h(px(22.))
            .w_full()
            .px_2()
            .gap_4()
            .bg(theme::c(theme::STATUS_BG))
            .text_color(theme::c(theme::STATUS_FG))
            .text_size(px(11.));

        if let Some(v) = self.active_view() {
            let v = v.read(cx);
            let doc = v.doc();
            let total = doc.total_file_lines();
            bar = bar
                .child(if doc.index_complete() {
                    format!("{} 行", group(total))
                } else {
                    // 索引还在跑，行数会继续涨
                    format!("{}+ 行", group(total))
                })
                .child(format!(
                    "第 {} 行",
                    group(doc.top_file_line().map(|l| l + 1).unwrap_or(0))
                ))
                .child(doc.encoding().label().to_string());

            if let Some(n) = doc.match_count() {
                bar = bar.child(format!("命中 {}", group(n as u64)));
            }
            if let Some(p) = v.indexing_progress() {
                bar = bar.child(format!("建索引 {:.0}%", p * 100.0));
            }
            if let Some(p) = v.scanning_progress() {
                bar = bar.child(format!("筛选中 {:.0}%", p * 100.0));
            }
            if v.from_cache() {
                bar = bar.child("索引缓存命中".to_string());
            }
            if let Some(e) = v.error() {
                bar = bar.child(e.to_string());
            }
        } else {
            bar = bar.child("就绪".to_string());
        }

        if let Some(s) = &self.status {
            bar = bar.child(s.clone());
        }
        bar.into_any_element()
    }

    fn empty_state(&self) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(theme::c(theme::BG))
            .text_color(theme::c(theme::FG))
            .child("logd")
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .child("把日志文件拖进来；拖 .tat 文件可加载关键字配置"),
            )
            .into_any_element()
    }
}

impl Render for LogdApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.active_view() {
            Some(v) => v.clone().into_any_element(),
            None => self.empty_state(),
        };
        let tabs = self.render_tabs(cx);
        let toolbar = self.render_toolbar(cx);
        let panel = self.panel_open.then(|| self.render_filter_panel(cx));
        let status = self.render_status(cx);

        v_flex()
            .id("root")
            .key_context("Logd")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme::c(theme::BG))
            .on_key_down(cx.listener(Self::on_key))
            .child(TitleBar::new().child(h_flex().w_full().pr_2().child("logd")))
            .when(!self.tabs.is_empty(), |el| el.child(tabs))
            .child(toolbar)
            .when_some(panel, |el, p| el.child(p))
            .child(div().flex_1().overflow_hidden().child(body))
            .child(status)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                for p in paths.paths() {
                    this.open_path(p, window, cx);
                }
            }))
    }
}

/// 千分位分隔。5 亿行的数字不加分隔没法读。
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn run(initial: Vec<PathBuf>) {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        let initial = initial.clone();
        cx.spawn(async move |cx| {
            cx.open_window(TitleBar::window_options(), |window, cx| {
                let view = cx.new(|cx| LogdApp::new(initial, window, cx));
                // 窗口第一层必须是 Root
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("创建窗口失败");
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::group;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(500_000_000), "500,000,000");
    }
}
