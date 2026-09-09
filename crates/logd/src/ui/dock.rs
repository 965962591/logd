//! Dock panel adapters. Business state remains owned by `LogdApp`.

use std::{collections::HashSet, path::PathBuf, rc::Rc, sync::Arc};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::dock::{
    panel_handle, register_panel, BasePanel, BasePanelView, DockArea, DockAreaRenderer,
    DockContext, DockSkin, DropIndicator, NodeId, Panel, PanelControl, PanelEvent, PanelHandle,
    PanelInfo, PanelState, TabGroupContext, TabGroupRenderer, TilesRenderer,
};
use gpui_component::{h_flex, IconName, Sizable as _};

use crate::app::LogdApp;
use crate::i18n::{text, Key, Language};
use crate::theme;

pub const WORKSPACE_PANEL: &str = "logd.workspace";
pub const FILTER_PANEL: &str = "logd.filters";
pub const SEARCH_RESULTS_PANEL: &str = "logd.search-results";

/// Builds the normal component dock, except that the central log workspace
/// does not draw redundant single-panel chrome above its content.
pub fn logd_dock_area(
    id: impl Into<SharedString>,
    version: Option<usize>,
    app: WeakEntity<LogdApp>,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<DockArea>, Rc<DockSkin>) {
    let mut component_skin = None;
    let area = cx.new(|cx| {
        let skin = DockSkin::new(cx);
        component_skin = Some(skin.clone());
        DockArea::new(id, version, window, cx)
            .with_renderer(Rc::new(LogdDockSkin { inner: skin, app }))
    });
    (
        area,
        component_skin.expect("DockSkin::new ran inside the dock constructor"),
    )
}

struct LogdDockSkin {
    inner: Rc<DockSkin>,
    app: WeakEntity<LogdApp>,
}

impl DockAreaRenderer for LogdDockSkin {
    fn frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(window, cx)
    }

    fn split_frame(
        &self,
        node: NodeId,
        axis: Axis,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.split_frame(node, axis, window, cx)
    }

    fn center_frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.center_frame(window, cx)
    }

    fn render_dock(
        &self,
        dock: &DockContext,
        content: AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_dock(dock, content, window, cx)
    }

    fn build_placeholder(
        &self,
        state: &PanelState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<dyn BasePanelView>> {
        self.inner.build_placeholder(state, window, cx)
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(LogdTabGroupSkin {
            inner: self.inner.tab_group_renderer(),
            app: self.app.clone(),
        })
    }

    fn tiles_renderer(&self) -> Rc<dyn TilesRenderer> {
        self.inner.tiles_renderer()
    }
}

struct LogdTabGroupSkin {
    inner: Rc<dyn TabGroupRenderer>,
    app: WeakEntity<LogdApp>,
}

const DRAG_PREVIEW_SIZE: Size<Pixels> = size(px(96.), px(30.));

struct LogdPanelDragPreview {
    panel: Arc<dyn BasePanelView>,
}

impl Render for LogdPanelDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        div()
            .id("logd-dock-drag-preview")
            .cursor_grab()
            .h(DRAG_PREVIEW_SIZE.height)
            .w(DRAG_PREVIEW_SIZE.width)
            .px_3()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_1()
            .border_color(palette.border)
            .rounded_sm()
            .text_color(palette.tab_foreground)
            .bg(palette.tab_active)
            .opacity(0.75)
            .child(panel_title(&self.panel, window, cx))
    }
}

fn panel_title(panel: &Arc<dyn BasePanelView>, window: &mut Window, cx: &mut App) -> AnyElement {
    PanelHandle::of(panel)
        .map(|handle| handle.title(window, cx))
        .unwrap_or_else(|| SharedString::from(panel.panel_name(cx)).into_any_element())
}

fn close_tool_panel_button(
    id: &'static str,
    panel_name: &'static str,
    app: WeakEntity<LogdApp>,
    language: Language,
) -> Button {
    Button::new(id)
        .icon(IconName::Close)
        .accessibility_label(text(Key::Close, language))
        .tooltip(text(Key::Close, language))
        .on_click(move |_, window, cx| {
            app.update(cx, |app, cx| match panel_name {
                FILTER_PANEL => app.show_filter_panel(false, window, cx),
                SEARCH_RESULTS_PANEL => app.show_search_results(false, window, cx),
                _ => {}
            })
            .ok();
        })
}

impl LogdTabGroupSkin {
    fn render_tool_panel_title(
        &self,
        group: &TabGroupContext,
        ix: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let panel = &group.panels()[ix];
        let Some(handle) = PanelHandle::of(panel) else {
            return self.inner.render_tab_bar(group, window, cx);
        };
        let language = self
            .app
            .upgrade()
            .map(|app| app.read(cx).language())
            .unwrap_or(Language::EnUs);
        let zoomed = group.is_zoomed();
        let title = handle.title(window, cx);
        let title_suffix = handle.title_suffix(window, cx);
        let toolbar_buttons = handle.toolbar_buttons(window, cx);
        let drag = group
            .is_draggable()
            .then(|| group.drag_panel(ix, cx))
            .flatten();

        h_flex()
            .justify_between()
            .h(px(30.))
            .py_2()
            .pl_3()
            .pr_2()
            .child(
                div()
                    .id("logd-tool-panel-tab")
                    .flex_1()
                    .min_w_16()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(title)
                    .when_some(drag, |this, drag| {
                        this.on_drag(drag, {
                            let panel = panel.clone();
                            move |drag, offset, _, cx| {
                                cx.stop_propagation();
                                drag.set_drag_offset(offset);
                                drag.set_preview_size(DRAG_PREVIEW_SIZE);
                                cx.new(|_| LogdPanelDragPreview {
                                    panel: panel.clone(),
                                })
                            }
                        })
                    }),
            )
            .children(title_suffix)
            .when(!group.is_collapsed(), |this| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .ml_1()
                        .gap_1()
                        .child(
                            Button::new(if zoomed {
                                "restore-tool-panel"
                            } else {
                                "maximize-tool-panel"
                            })
                            .icon(if zoomed {
                                IconName::Minimize
                            } else {
                                IconName::Maximize
                            })
                            .xsmall()
                            .ghost()
                            .tab_stop(false)
                            .accessibility_label(text(
                                if zoomed { Key::Restore } else { Key::Maximize },
                                language,
                            ))
                            .tooltip(text(
                                if zoomed { Key::Restore } else { Key::Maximize },
                                language,
                            ))
                            .on_click({
                                let group = group.clone();
                                move |_, window, cx| group.toggle_zoom(window, cx)
                            }),
                        )
                        .when_some(toolbar_buttons, |this, buttons| {
                            this.children(
                                buttons
                                    .into_iter()
                                    .map(|button| button.xsmall().ghost().tab_stop(false)),
                            )
                        }),
                )
            })
            .into_any_element()
    }
}

impl TabGroupRenderer for LogdTabGroupSkin {
    fn frame(&self, group: &TabGroupContext, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(group, window, cx)
    }

    fn content_frame(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.content_frame(group, window, cx)
    }

    fn render_tab_bar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let visible_panels = group
            .panels()
            .iter()
            .enumerate()
            .filter(|(_, panel)| panel.visible(cx))
            .map(|(ix, _)| ix)
            .collect::<Vec<_>>();
        match visible_panels.as_slice() {
            [ix] if group.panels()[*ix].panel_name(cx) == WORKSPACE_PANEL => {
                Empty.into_any_element()
            }
            [ix] if matches!(
                group.panels()[*ix].panel_name(cx),
                FILTER_PANEL | SEARCH_RESULTS_PANEL
            ) =>
            {
                self.render_tool_panel_title(group, *ix, window, cx)
            }
            _ => self.inner.render_tab_bar(group, window, cx),
        }
    }

    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_active_panel(panel, group, window, cx)
    }

    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_drop_indicator(indicator, window, cx)
    }

    fn render_empty(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_empty(group, window, cx)
    }
}

pub fn register_logd_panels(
    workspace: &Entity<LogPanel>,
    filters: &Entity<FilterPanel>,
    search_results: &Entity<SearchResultsPanel>,
    cx: &mut App,
) {
    register_panel(cx, WORKSPACE_PANEL, {
        let workspace = workspace.clone();
        move |_, _, _| panel_handle(workspace.clone())
    });
    register_panel(cx, FILTER_PANEL, {
        let filters = filters.clone();
        move |context, _, cx| {
            if let Some(visible) = restored_visibility(context.info()) {
                filters.update(cx, |panel, cx| panel.set_visible(visible, cx));
            }
            panel_handle(filters.clone())
        }
    });
    register_panel(cx, SEARCH_RESULTS_PANEL, {
        let search_results = search_results.clone();
        move |context, _, cx| {
            if let Some(visible) = restored_visibility(context.info()) {
                search_results.update(cx, |panel, cx| panel.set_visible(visible, cx));
            }
            panel_handle(search_results.clone())
        }
    });
}

fn visibility_state(panel_name: &'static str, visible: bool) -> PanelState {
    let mut state = PanelState::new(panel_name);
    state.info = PanelInfo::panel(serde_json::json!({ "visible": visible }));
    state
}

fn restored_visibility(info: &PanelInfo) -> Option<bool> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    value.get("visible").and_then(serde_json::Value::as_bool)
}

pub struct LogPanel {
    app: WeakEntity<LogdApp>,
    focus: FocusHandle,
}

impl LogPanel {
    pub fn new(app: WeakEntity<LogdApp>, cx: &mut Context<Self>) -> Self {
        Self {
            app,
            focus: cx.focus_handle(),
        }
    }
}

impl BasePanel for LogPanel {
    fn panel_name(&self) -> &'static str {
        WORKSPACE_PANEL
    }

    fn closable(&self, _: &App) -> bool {
        false
    }
}

impl Panel for LogPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "logd"
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for LogPanel {}

impl Focusable for LogPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LogPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .upgrade()
            .map(|app| app.read(cx).render_workspace(cx))
            .unwrap_or_else(|| div().into_any_element())
    }
}

pub struct FilterPanel {
    app: WeakEntity<LogdApp>,
    focus: FocusHandle,
    visible: bool,
}

impl FilterPanel {
    pub fn new(app: WeakEntity<LogdApp>, cx: &mut Context<Self>) -> Self {
        if let Some(app) = app.upgrade() {
            cx.observe(&app, |_, _, cx| cx.notify()).detach();
        }
        Self {
            app,
            focus: cx.focus_handle(),
            visible: true,
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            cx.notify();
        }
    }
}

impl BasePanel for FilterPanel {
    fn panel_name(&self) -> &'static str {
        FILTER_PANEL
    }

    fn visible(&self, _: &App) -> bool {
        self.visible
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn dump(&self, _: &App) -> PanelState {
        visibility_state(FILTER_PANEL, self.visible)
    }
}

impl Panel for FilterPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .upgrade()
            .map(|app| text(Key::FilterPanel, app.read(cx).language()).to_string())
            .unwrap_or_else(|| "Filters".to_string())
    }

    fn title_suffix(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let language = self
            .app
            .upgrade()
            .map(|app| app.read(cx).language())
            .unwrap_or(Language::EnUs);
        let (has_filters, all_filters_enabled) = self
            .app
            .upgrade()
            .map(|app| {
                let state = app.read(cx);
                (state.has_filters(), state.all_filters_enabled())
            })
            .unwrap_or((false, false));
        let select_all_app = self.app.clone();
        let add_filter_app = self.app.clone();
        Some(
            h_flex()
                .items_center()
                .gap_1()
                .when(has_filters, |controls| {
                    controls.child(
                        Checkbox::new("filter-select-all")
                            .small()
                            .checked(all_filters_enabled)
                            .tab_stop(false)
                            .accessibility_label(text(Key::SelectAll, language))
                            .tooltip(text(Key::SelectAll, language))
                            .on_click(move |checked, _, cx| {
                                select_all_app
                                    .update(cx, |app, cx| app.set_all_filters_enabled(*checked, cx))
                                    .ok();
                            }),
                    )
                })
                .child(
                    Button::new("filter-add")
                        .icon(IconName::Plus)
                        .xsmall()
                        .ghost()
                        .tab_stop(false)
                        .accessibility_label(text(Key::AddFilter, language))
                        .tooltip(text(Key::AddFilter, language))
                        .on_click(move |_, window, cx| {
                            add_filter_app
                                .update(cx, |app, cx| app.begin_add_filter(window, cx))
                                .ok();
                        }),
                ),
        )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    fn toolbar_buttons(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Button>> {
        let language = self
            .app
            .upgrade()
            .map(|app| app.read(cx).language())
            .unwrap_or(Language::EnUs);
        Some(vec![close_tool_panel_button(
            "close-filter-panel",
            FILTER_PANEL,
            self.app.clone(),
            language,
        )])
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        Some(PanelControl::Toolbar)
    }
}

impl EventEmitter<PanelEvent> for FilterPanel {}

impl Focusable for FilterPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilterPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .upgrade()
            .map(|app| LogdApp::render_filters(&app, window, cx))
            .unwrap_or_else(|| div().into_any_element())
    }
}

pub struct SearchResultsPanel {
    app: WeakEntity<LogdApp>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    collapsed_files: HashSet<PathBuf>,
    content_width: f32,
    visible: bool,
}

impl SearchResultsPanel {
    pub fn new(app: WeakEntity<LogdApp>, cx: &mut Context<Self>) -> Self {
        Self {
            app,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            collapsed_files: HashSet::new(),
            content_width: 720.0,
            visible: true,
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            cx.notify();
        }
    }

    pub fn reset_scroll(&mut self) {
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.collapsed_files.clear();
        self.content_width = 720.0;
    }

    pub(crate) fn toggle_file(&mut self, path: PathBuf, row: usize, cx: &mut Context<Self>) {
        if !self.collapsed_files.remove(&path) {
            self.collapsed_files.insert(path);
        }
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    pub(crate) fn observe_content_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if width > self.content_width {
            self.content_width = width;
            cx.notify();
        }
    }
}

impl BasePanel for SearchResultsPanel {
    fn panel_name(&self) -> &'static str {
        SEARCH_RESULTS_PANEL
    }

    fn visible(&self, _: &App) -> bool {
        self.visible
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn dump(&self, _: &App) -> PanelState {
        visibility_state(SEARCH_RESULTS_PANEL, self.visible)
    }
}

impl Panel for SearchResultsPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .upgrade()
            .map(|app| text(Key::SearchResults, app.read(cx).language()).to_string())
            .unwrap_or_else(|| "Search Results".to_string())
    }

    fn title_suffix(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let palette = theme::palette(cx);
        let (summary, progress) = self
            .app
            .upgrade()
            .map(|app| app.read(cx).search_results_title_status(cx))
            .unwrap_or_else(|| ("0 Matches  |  0 Files".to_string(), None));

        Some(
            h_flex()
                .flex_shrink_0()
                .gap_2()
                .text_size(px(12.))
                .text_color(palette.muted)
                .child(summary)
                .children(
                    progress.map(|progress| {
                        div().text_color(palette.search_foreground).child(progress)
                    }),
                ),
        )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    fn toolbar_buttons(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Button>> {
        let language = self
            .app
            .upgrade()
            .map(|app| app.read(cx).language())
            .unwrap_or(Language::EnUs);
        Some(vec![close_tool_panel_button(
            "close-search-results-panel",
            SEARCH_RESULTS_PANEL,
            self.app.clone(),
            language,
        )])
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        Some(PanelControl::Toolbar)
    }
}

impl EventEmitter<PanelEvent> for SearchResultsPanel {}

impl Focusable for SearchResultsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SearchResultsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        self.app
            .upgrade()
            .map(|app| {
                LogdApp::render_search_results(
                    &app,
                    &self.scroll,
                    &self.collapsed_files,
                    self.content_width,
                    panel,
                    window,
                    cx,
                )
            })
            .unwrap_or_else(|| div().into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::{restored_visibility, visibility_state, FILTER_PANEL};
    use gpui_component::dock::PanelState;

    #[test]
    fn panel_visibility_round_trips_through_json() {
        for visible in [false, true] {
            let state = visibility_state(FILTER_PANEL, visible);
            let json = serde_json::to_string(&state).unwrap();
            let restored: PanelState = serde_json::from_str(&json).unwrap();

            assert_eq!(restored.panel_name, FILTER_PANEL);
            assert_eq!(restored_visibility(&restored.info), Some(visible));
        }
    }
}
