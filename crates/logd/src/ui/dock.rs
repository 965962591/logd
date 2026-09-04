//! Dock panel adapters. Business state remains owned by `LogdApp`.

use std::{rc::Rc, sync::Arc};

use gpui::*;
use gpui_component::dock::{
    BasePanel, BasePanelView, DockArea, DockAreaRenderer, DockContext, DockSkin, DropIndicator,
    NodeId, Panel, PanelControl, PanelEvent, PanelState, TabGroupContext, TabGroupRenderer,
    TilesRenderer,
};

use crate::app::LogdApp;
use crate::i18n::{text, Key};

/// Builds the normal component dock, except that the central log workspace
/// does not draw redundant single-panel chrome above its content.
pub fn logd_dock_area(
    id: impl Into<SharedString>,
    version: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<DockArea>, Rc<DockSkin>) {
    let mut component_skin = None;
    let area = cx.new(|cx| {
        let skin = DockSkin::new(cx);
        component_skin = Some(skin.clone());
        DockArea::new(id, version, window, cx).with_renderer(Rc::new(LogdDockSkin { inner: skin }))
    });
    (
        area,
        component_skin.expect("DockSkin::new ran inside the dock constructor"),
    )
}

struct LogdDockSkin {
    inner: Rc<DockSkin>,
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
        })
    }

    fn tiles_renderer(&self) -> Rc<dyn TilesRenderer> {
        self.inner.tiles_renderer()
    }
}

struct LogdTabGroupSkin {
    inner: Rc<dyn TabGroupRenderer>,
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
            .filter(|panel| panel.visible(cx))
            .count();
        if visible_panels == 1
            && group.active_panel().is_some_and(|panel| {
                matches!(panel.panel_name(cx), "logd.workspace" | "logd.filters")
            })
        {
            Empty.into_any_element()
        } else {
            self.inner.render_tab_bar(group, window, cx)
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
        "logd.workspace"
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
}

impl FilterPanel {
    pub fn new(app: WeakEntity<LogdApp>, cx: &mut Context<Self>) -> Self {
        Self {
            app,
            focus: cx.focus_handle(),
        }
    }
}

impl BasePanel for FilterPanel {
    fn panel_name(&self) -> &'static str {
        "logd.filters"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }
}

impl Panel for FilterPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.app
            .upgrade()
            .map(|app| text(Key::FilterPanel, app.read(cx).language()).to_string())
            .unwrap_or_else(|| "Filters".to_string())
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
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
