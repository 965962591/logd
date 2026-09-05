//! Desktop shell and application command routing.
//!
//! Configured filters can target one tab or all imported tabs. Only the active
//! tab is rescanned immediately after a filter edit; inactive tabs are marked
//! dirty until activated. Title-bar searches are temporary and scan every tab.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::dock::{
    panel_handle, DockArea, DockAreaState, DockEvent, DockLayout, DockPlacement,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::scroll::{ScrollableElement, Scrollbar, ScrollbarMode};
use gpui_component::{
    h_flex, v_flex, Icon, IconName, InteractiveElementExt as _, Root, Selectable as _, Sizable,
};
use logd_core::{Encoding, FilterScope, FilterSpec, HighlightMode, TatFile};

use crate::i18n::{text, Key, Language};
use crate::log_view::LogView;
use crate::theme;
use crate::ui::dock::{
    logd_dock_area, register_logd_panels, FilterPanel, LogPanel, SearchResultsPanel,
};
use crate::ui::title_bar;

struct Tab {
    view: Entity<LogView>,
    title: String,
    path: PathBuf,
}

#[derive(Clone)]
struct SearchResultFile {
    tab_index: usize,
    path: PathBuf,
    full_path: String,
    view: Entity<LogView>,
    lines: Arc<Vec<u64>>,
    start: usize,
    expanded: bool,
}

const DOCK_LAYOUT_VERSION: usize = 2;

#[derive(Clone)]
struct TabDrag {
    index: usize,
    title: String,
}

impl Render for TabDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .h(px(24.))
            .bg(theme::c(theme::SELECTION))
            .text_color(theme::c(theme::FG))
            .child(self.title.clone())
    }
}

#[derive(Clone)]
enum MenuCommand {
    Open,
    OpenRecent(PathBuf),
    Refresh,
    SaveEditedCopy,
    ImportFilters,
    SetEncoding(Encoding),
    CopySelection,
    ShowAll,
    ShowOnlyFiltered,
    ToggleFilters,
    ToggleSearchResults,
    AddFilter,
    EditFilter,
    DeleteFilter,
    SaveFilters,
    ToggleLanguage,
}

pub struct LogdApp {
    tabs: Vec<Tab>,
    active: usize,
    filters: Vec<FilterSpec>,
    /// Temporary title-bar search terms. They are applied to every tab but
    /// are intentionally kept out of the persisted/configured filter list.
    search_filters: Vec<FilterSpec>,
    show_only_filtered: bool,
    tat_path: Option<PathBuf>,
    recent_files: Vec<PathBuf>,
    keyword: Entity<InputState>,
    filter_text: Entity<InputState>,
    filter_description: Entity<InputState>,
    filter_fore: Entity<ColorPickerState>,
    filter_back: Entity<ColorPickerState>,
    filter_scope: FilterScope,
    selected_filter: Option<usize>,
    editing_filter: Option<usize>,
    filter_editor_open: bool,
    dock_area: Entity<DockArea>,
    tab_scroll: ScrollHandle,
    filter_panel: Entity<FilterPanel>,
    search_results_panel: Entity<SearchResultsPanel>,
    last_layout_state: Option<DockAreaState>,
    save_layout_task: Option<Task<()>>,
    language: Language,
    status: Option<String>,
    focus: FocusHandle,
}

impl LogdApp {
    pub fn new(initial: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = Language::from_env();
        let keyword = cx.new(|cx| {
            InputState::new(window, cx).placeholder(text(Key::SearchPlaceholder, language))
        });
        let filter_text = cx.new(|cx| {
            InputState::new(window, cx).placeholder(text(Key::FilterTextPlaceholder, language))
        });
        let filter_description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(text(Key::FilterDescriptionPlaceholder, language))
        });
        let filter_fore = cx.new(|cx| ColorPickerState::new(window, cx));
        let filter_back = cx.new(|cx| ColorPickerState::new(window, cx));

        cx.subscribe_in(
            &keyword,
            window,
            |this, state, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.set_global_search(state.read(cx).value().to_string(), window, cx);
                }
            },
        )
        .detach();
        cx.subscribe(&filter_fore, |_, _, _: &ColorPickerEvent, cx| cx.notify())
            .detach();
        cx.subscribe(&filter_back, |_, _, _: &ColorPickerEvent, cx| cx.notify())
            .detach();
        cx.subscribe_in(
            &filter_text,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.commit_filter(window, cx);
                }
            },
        )
        .detach();

        let app = cx.weak_entity();
        let log_panel = cx.new(|cx| LogPanel::new(app.clone(), cx));
        let filter_panel = cx.new(|cx| FilterPanel::new(app.clone(), cx));
        let search_results_panel = cx.new(|cx| SearchResultsPanel::new(app, cx));
        register_logd_panels(&log_panel, &filter_panel, &search_results_panel, cx);

        let legacy_filter_placement = crate::settings::load_filter_placement();
        let (dock_area, skin) = logd_dock_area("logd.main", Some(DOCK_LAYOUT_VERSION), window, cx);
        // The View menu is the single visibility control; avoid a duplicate dock toggle button.
        skin.set_toggle_button_visible(false, cx);
        let restored = crate::settings::load_dock_layout()
            .filter(|state| state.version == Some(DOCK_LAYOUT_VERSION))
            .is_some_and(|state| {
                dock_area
                    .update(cx, |dock, cx| dock.load(state, window, cx))
                    .is_ok()
            });
        if !restored {
            Self::reset_default_dock_layout(
                &dock_area,
                &log_panel,
                &filter_panel,
                &search_results_panel,
                legacy_filter_placement,
                window,
                cx,
            );
        }
        dock_area.update(cx, |dock, cx| {
            for placement in [
                DockPlacement::Left,
                DockPlacement::Right,
                DockPlacement::Bottom,
            ] {
                if dock.has_dock(placement) {
                    dock.set_dock_collapsible(placement, true, window, cx);
                }
            }
        });
        let last_layout_state = Some(dock_area.read(cx).dump(cx));

        cx.subscribe_in(
            &dock_area,
            window,
            |this, dock_area, event: &DockEvent, window, cx| {
                if matches!(event, DockEvent::LayoutChanged) {
                    this.schedule_layout_save(dock_area, window, cx);
                }
            },
        )
        .detach();
        cx.on_app_quit({
            let dock_area = dock_area.clone();
            move |_, cx| {
                let state = dock_area.read(cx).dump(cx);
                cx.background_executor().spawn(async move {
                    let _ = crate::settings::save_dock_layout(&state);
                })
            }
        })
        .detach();

        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            filters: Vec::new(),
            search_filters: Vec::new(),
            show_only_filtered: false,
            tat_path: None,
            recent_files: Vec::new(),
            keyword,
            filter_text,
            filter_description,
            filter_fore,
            filter_back,
            filter_scope: FilterScope::default(),
            selected_filter: None,
            editing_filter: None,
            filter_editor_open: false,
            dock_area,
            tab_scroll: ScrollHandle::new(),
            filter_panel,
            search_results_panel,
            last_layout_state,
            save_layout_task: None,
            language,
            status: None,
            focus: cx.focus_handle(),
        };
        for path in initial {
            this.open_path(&path, window, cx);
        }
        this
    }

    fn reset_default_dock_layout(
        dock_area: &Entity<DockArea>,
        workspace: &Entity<LogPanel>,
        filters: &Entity<FilterPanel>,
        search_results: &Entity<SearchResultsPanel>,
        filter_placement: DockPlacement,
        window: &mut Window,
        cx: &mut App,
    ) {
        dock_area.update(cx, |dock, cx| {
            for placement in [
                DockPlacement::Left,
                DockPlacement::Right,
                DockPlacement::Bottom,
            ] {
                dock.remove_dock(placement, window, cx);
            }
            let workspace = DockLayout::tabs().panel_view(panel_handle(workspace.clone()), cx);
            let filters = DockLayout::tabs().panel_view(panel_handle(filters.clone()), cx);
            let search_results =
                DockLayout::tabs().panel_view(panel_handle(search_results.clone()), cx);

            // An outer dock protects its last visible panel from being
            // dragged away. One split tree keeps these tool panels movable.
            let center = match filter_placement {
                DockPlacement::Left => DockLayout::h_split().child(filters, Some(px(360.))).child(
                    DockLayout::v_split()
                        .child(workspace, None)
                        .child(search_results, Some(px(240.))),
                    None,
                ),
                DockPlacement::Bottom => DockLayout::v_split().child(workspace, None).child(
                    DockLayout::h_split()
                        .child(search_results, None)
                        .child(filters, Some(px(360.))),
                    Some(px(240.)),
                ),
                DockPlacement::Right | DockPlacement::Center => DockLayout::h_split()
                    .child(
                        DockLayout::v_split()
                            .child(workspace, None)
                            .child(search_results, Some(px(240.))),
                        None,
                    )
                    .child(filters, Some(px(360.))),
            };
            dock.set_center(center, window, cx);
        });
    }

    fn schedule_layout_save(
        &mut self,
        dock_area: &Entity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dock_area = dock_area.clone();
        self.save_layout_task = Some(cx.spawn_in(window, async move |this, window| {
            window
                .background_executor()
                .timer(Duration::from_millis(400))
                .await;
            _ = this.update_in(window, move |this, _, cx| {
                let state = dock_area.read(cx).dump(cx);
                if this.last_layout_state.as_ref() == Some(&state) {
                    return;
                }
                if crate::settings::save_dock_layout(&state).is_ok() {
                    this.last_layout_state = Some(state);
                }
            });
        }));
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn open_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tat"))
        {
            self.load_tat(path, cx);
        } else {
            self.open_log(path, window, cx);
        }
    }

    fn remember_file(&mut self, path: &Path) {
        self.recent_files.retain(|item| item != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(10);
    }

    fn open_log(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
            self.set_active(index, cx);
            self.remember_file(path);
            return;
        }
        match LogView::load(path) {
            Ok(loaded) => {
                let view = cx.new(|cx| LogView::new(loaded, window, cx));
                self.observe_log_view(&view, cx);
                let tab_index = self.tabs.len();
                self.tabs.push(Tab {
                    view: view.clone(),
                    title: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                    path: path.to_path_buf(),
                });
                self.active = tab_index;
                self.tab_scroll.scroll_to_item(tab_index);
                self.apply_filters_to_view(tab_index, &view, cx);
                self.apply_search_to_view(&view, cx);
                self.remember_file(path);
                self.status = None;
            }
            Err(error) => {
                self.status = Some(format!("{}: {error:#}", text(Key::Open, self.language)))
            }
        }
        cx.notify();
    }

    fn filters_for_tab(&self, tab_index: usize) -> Vec<FilterSpec> {
        let mut filters = self
            .filters
            .iter()
            .cloned()
            .map(|mut filter| {
                if filter.scope == FilterScope::CurrentFile && tab_index != self.active {
                    filter.enabled = false;
                }
                filter
            })
            .collect::<Vec<_>>();
        filters.extend(self.search_filters.iter().cloned());
        filters
    }

    fn apply_filters_to_view(&self, tab_index: usize, view: &Entity<LogView>, cx: &mut App) {
        let filters = self.filters_for_tab(tab_index);
        let only = self.show_only_filtered;
        view.update(cx, |view, cx| {
            view.apply_filters(filters, cx);
            view.set_show_only_filtered(only, cx);
        });
    }

    fn apply_search_to_view(&self, view: &Entity<LogView>, cx: &mut App) {
        let filters = self.search_filters.clone();
        view.update(cx, |view, cx| view.apply_search(filters, cx));
    }

    fn observe_log_view(&self, view: &Entity<LogView>, cx: &mut Context<Self>) {
        cx.observe(view, |this, _, cx| {
            this.search_results_panel.update(cx, |_, cx| cx.notify());
            cx.notify();
        })
        .detach();
    }

    fn notify_search_results(&self, cx: &mut App) {
        self.search_results_panel.update(cx, |_, cx| cx.notify());
    }

    fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(text(Key::Open, self.language).into()),
        });
        cx.spawn_in(window, async move |this, window| {
            let Some(paths) = paths.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            _ = window.update(|window, cx| {
                _ = this.update(cx, |this, cx| {
                    for path in paths {
                        this.open_path(&path, window, cx);
                    }
                });
            });
        })
        .detach();
    }

    fn prompt_import_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(text(Key::ImportFilters, self.language).into()),
        });
        cx.spawn_in(window, async move |this, window| {
            let Some(paths) = paths.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            _ = window.update(|_, cx| {
                _ = this.update(cx, |this, cx| {
                    if path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("tat"))
                    {
                        this.load_tat(&path, cx);
                    } else {
                        this.status =
                            Some(format!("{}: .tat", text(Key::ImportFilters, this.language)));
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn refresh_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let path = tab.path.clone();
        match LogView::load(&path) {
            Ok(loaded) => {
                let view = cx.new(|cx| LogView::new(loaded, window, cx));
                self.observe_log_view(&view, cx);
                self.apply_filters_to_view(self.active, &view, cx);
                self.apply_search_to_view(&view, cx);
                self.tabs[self.active].view = view;
                self.status = Some(format!(
                    "{}: {}",
                    text(Key::Refresh, self.language),
                    path.display()
                ));
            }
            Err(error) => {
                self.status = Some(format!("{}: {error:#}", text(Key::Refresh, self.language)))
            }
        }
        cx.notify();
    }

    fn set_active(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.active = index;
        self.tab_scroll.scroll_to_item(index);
        let view = self.tabs[index].view.clone();
        // Reapply even when the view is not dirty: a current-file filter
        // changes meaning when the active tab changes.
        if view.read(cx).is_dirty()
            || self
                .filters
                .iter()
                .any(|filter| filter.scope == FilterScope::CurrentFile)
        {
            self.apply_filters_to_view(index, &view, cx);
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let active_path = self.tabs.get(self.active).map(|tab| tab.path.clone());
        let removed_active = index == self.active;
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if removed_active {
            self.active = index.min(self.tabs.len() - 1);
        } else if let Some(path) = active_path.as_ref() {
            self.active = self
                .tabs
                .iter()
                .position(|tab| &tab.path == path)
                .unwrap_or_else(|| index.min(self.tabs.len() - 1));
        }
        if removed_active && !self.tabs.is_empty() {
            // Removing the active tab changes which file a current-file filter belongs to.
            let view = self.tabs[self.active].view.clone();
            self.apply_filters_to_view(self.active, &view, cx);
        }
        if !self.tabs.is_empty() {
            self.tab_scroll.scroll_to_item(self.active);
        }
        self.notify_search_results(cx);
        cx.notify();
    }

    fn close_tabs_before(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == 0 || index >= self.tabs.len() {
            return;
        }
        let active_path = self.tabs.get(self.active).map(|tab| tab.path.clone());
        self.tabs.drain(..index);
        self.restore_active_path(active_path, cx);
    }

    fn close_tabs_after(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len().saturating_sub(1) {
            return;
        }
        let active_path = self.tabs.get(self.active).map(|tab| tab.path.clone());
        self.tabs.truncate(index + 1);
        self.restore_active_path(active_path, cx);
    }

    fn close_clean_tabs(&mut self, cx: &mut Context<Self>) {
        let active_path = self.tabs.get(self.active).map(|tab| tab.path.clone());
        self.tabs
            .retain(|tab| tab.view.read(cx).can_close_without_prompt());
        self.restore_active_path(active_path, cx);
    }

    fn restore_active_path(&mut self, active_path: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            self.active = 0;
            self.notify_search_results(cx);
            cx.notify();
            return;
        }
        let old_path = active_path;
        let next = old_path
            .as_ref()
            .and_then(|path| self.tabs.iter().position(|tab| &tab.path == path))
            .unwrap_or_else(|| self.active.min(self.tabs.len() - 1));
        let changed = self.active != next
            || old_path
                .as_ref()
                .is_some_and(|path| self.tabs.get(next).is_some_and(|tab| &tab.path != path));
        self.active = next;
        self.tab_scroll.scroll_to_item(self.active);
        if changed {
            let view = self.tabs[self.active].view.clone();
            self.apply_filters_to_view(self.active, &view, cx);
        }
        self.notify_search_results(cx);
        cx.notify();
    }

    fn reorder_tab(&mut self, from: usize, target: usize, cx: &mut Context<Self>) {
        if from >= self.tabs.len() || target >= self.tabs.len() || from == target {
            return;
        }
        let active_path = self.tabs.get(self.active).map(|tab| tab.path.clone());
        let tab = self.tabs.remove(from);
        let insertion = if from < target { target - 1 } else { target };
        self.tabs.insert(insertion, tab);
        self.active = active_path
            .as_ref()
            .and_then(|path| self.tabs.iter().position(|tab| &tab.path == path))
            .unwrap_or(insertion);
        self.tab_scroll.scroll_to_item(self.active);
        self.notify_search_results(cx);
        cx.notify();
    }

    fn active_view(&self) -> Option<&Entity<LogView>> {
        self.tabs.get(self.active).map(|tab| &tab.view)
    }

    fn goto_search_result(&mut self, tab_index: usize, file_line: u64, cx: &mut Context<Self>) {
        if tab_index >= self.tabs.len() {
            return;
        }
        self.set_active(tab_index, cx);
        let view = self.tabs[tab_index].view.clone();
        view.update(cx, |view, cx| view.goto_file_line(file_line, cx));
    }

    fn filters_changed(&mut self, cx: &mut Context<Self>) {
        for (index, tab) in self.tabs.iter().enumerate() {
            if index == self.active {
                self.apply_filters_to_view(index, &tab.view, cx);
            } else {
                tab.view.update(cx, |view, _| view.mark_dirty());
            }
        }
        cx.notify();
    }

    fn set_global_search(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search_filters = split_search_keywords(&value)
            .into_iter()
            .map(|keyword| FilterSpec {
                text: keyword,
                mode: HighlightMode::Field,
                fore: Some(theme::SEARCH_FORE),
                ..Default::default()
            })
            .collect();

        // A title-bar search is intentionally independent of configured
        // filters and is scanned in every imported file.
        let tab_filters = (0..self.tabs.len())
            .map(|index| (index, self.tabs[index].view.clone()))
            .collect::<Vec<_>>();
        for (index, view) in tab_filters {
            self.apply_filters_to_view(index, &view, cx);
            self.apply_search_to_view(&view, cx);
        }
        let has_search = !self.search_filters.is_empty();
        self.search_results_panel.update(cx, |panel, cx| {
            panel.reset_scroll();
            if has_search {
                panel.set_visible(true, cx);
            }
            cx.notify();
        });
        if has_search {
            let dock_area = self.dock_area.clone();
            self.schedule_layout_save(&dock_area, window, cx);
        }
        cx.notify();
    }

    fn begin_add_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_editor_open = true;
        self.editing_filter = None;
        self.filter_text
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_description
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_scope = FilterScope::default();
        // New filters start with the editor's neutral/default colors. Colors
        // are opt-in and can be assigned from the editor when needed.
        self.filter_fore
            .update(cx, |picker, cx| picker.clear_value(window, cx));
        self.filter_back
            .update(cx, |picker, cx| picker.clear_value(window, cx));
        window.focus(&self.filter_text.read(cx).focus_handle(cx), cx);
        self.show_filter_panel(true, window, cx);
    }

    fn begin_edit_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .selected_filter
            .filter(|index| *index < self.filters.len())
        else {
            return;
        };
        let filter = self.filters[index].clone();
        self.filter_editor_open = true;
        self.editing_filter = Some(index);
        self.filter_text
            .update(cx, |state, cx| state.set_value(filter.text, window, cx));
        self.filter_description.update(cx, |state, cx| {
            state.set_value(filter.description, window, cx)
        });
        self.filter_scope = filter.scope;
        set_picker_color(&self.filter_fore, filter.fore, window, cx);
        set_picker_color(&self.filter_back, filter.back, window, cx);
        window.focus(&self.filter_text.read(cx).focus_handle(cx), cx);
    }

    fn commit_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.filter_text.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let description = self.filter_description.read(cx).value().trim().to_string();
        let fore = self.filter_fore.read(cx).value().map(hsla_to_rgb);
        let back = self.filter_back.read(cx).value().map(hsla_to_rgb);
        match self.editing_filter {
            Some(index) if index < self.filters.len() => {
                self.filters[index].text = value;
                self.filters[index].description = description;
                self.filters[index].fore = fore;
                self.filters[index].back = back;
                self.filters[index].scope = self.filter_scope;
                self.selected_filter = Some(index);
            }
            _ => {
                self.filters.push(FilterSpec {
                    text: value,
                    description,
                    mode: HighlightMode::Field,
                    fore,
                    back,
                    scope: self.filter_scope,
                    ..Default::default()
                });
                self.selected_filter = Some(self.filters.len() - 1);
            }
        }
        self.filter_editor_open = false;
        self.editing_filter = None;
        self.filter_text
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_description
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filters_changed(cx);
    }

    fn delete_selected_filter(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self
            .selected_filter
            .filter(|index| *index < self.filters.len())
        else {
            return;
        };
        self.filters.remove(index);
        self.selected_filter = (index < self.filters.len())
            .then_some(index)
            .or_else(|| self.filters.len().checked_sub(1));
        self.filter_editor_open = false;
        self.editing_filter = None;
        self.filters_changed(cx);
    }

    fn set_show_only(&mut self, only: bool, cx: &mut Context<Self>) {
        self.show_only_filtered = only;
        if let Some(view) = self.active_view().cloned() {
            view.update(cx, |view, cx| view.set_show_only_filtered(only, cx));
        }
        cx.notify();
    }

    fn set_encoding(&mut self, encoding: Encoding, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else {
            return;
        };
        if view.read(cx).doc().encoding() == encoding {
            return;
        }
        let filters = self.filters_for_tab(self.active);
        let search_filters = self.search_filters.clone();
        view.update(cx, |view, cx| {
            view.set_encoding(encoding, filters, search_filters, cx)
        });
        cx.notify();
    }

    fn load_tat(&mut self, path: &Path, cx: &mut Context<Self>) {
        match TatFile::load(path) {
            Ok(tat) => {
                self.filters = tat.filters;
                self.show_only_filtered = tat.show_only_filtered;
                self.tat_path = Some(path.to_path_buf());
                self.selected_filter = (!self.filters.is_empty()).then_some(0);
                self.status = Some(format!(
                    "{}: {}",
                    text(Key::Filters, self.language),
                    self.filters.len()
                ));
                self.filters_changed(cx);
            }
            Err(error) => {
                self.status = Some(format!(".tat: {error:#}"));
                cx.notify();
            }
        }
    }

    fn save_tat(&mut self, cx: &mut Context<Self>) {
        let path = self.tat_path.clone().or_else(|| {
            self.tabs
                .get(self.active)
                .map(|tab| tab.path.with_extension("tat"))
        });
        let Some(path) = path else {
            self.status = Some(text(Key::Ready, self.language).to_string());
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
                format!(
                    "{}: {}",
                    text(Key::SaveFilter, self.language),
                    path.display()
                )
            }
            Err(error) => format!("{}: {error:#}", text(Key::SaveFilter, self.language)),
        });
        cx.notify();
    }

    fn show_filter_panel(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_panel
            .update(cx, |panel, cx| panel.set_visible(show, cx));
        let dock_area = self.dock_area.clone();
        self.schedule_layout_save(&dock_area, window, cx);
        cx.notify();
    }

    fn show_search_results(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.search_results_panel
            .update(cx, |panel, cx| panel.set_visible(show, cx));
        let dock_area = self.dock_area.clone();
        self.schedule_layout_save(&dock_area, window, cx);
        cx.notify();
    }

    fn dispatch(&mut self, command: MenuCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            MenuCommand::Open => self.prompt_open(window, cx),
            MenuCommand::OpenRecent(path) => self.open_path(&path, window, cx),
            MenuCommand::Refresh => self.refresh_active(window, cx),
            MenuCommand::SaveEditedCopy => {
                if let Some(view) = self.active_view().cloned() {
                    view.update(cx, |view, cx| view.save_edited_copy(window, cx));
                }
            }
            MenuCommand::ImportFilters => self.prompt_import_filters(window, cx),
            MenuCommand::SetEncoding(encoding) => self.set_encoding(encoding, cx),
            MenuCommand::CopySelection => {
                if let Some(view) = self.active_view().cloned() {
                    view.update(cx, |view, cx| view.copy_selection(cx));
                }
            }
            MenuCommand::ShowAll => self.set_show_only(false, cx),
            MenuCommand::ShowOnlyFiltered => self.set_show_only(true, cx),
            MenuCommand::ToggleFilters => {
                let visible = self.filter_panel.read(cx).visible();
                self.show_filter_panel(!visible, window, cx)
            }
            MenuCommand::ToggleSearchResults => {
                let visible = self.search_results_panel.read(cx).visible();
                self.show_search_results(!visible, window, cx)
            }
            MenuCommand::AddFilter => self.begin_add_filter(window, cx),
            MenuCommand::EditFilter => self.begin_edit_filter(window, cx),
            MenuCommand::DeleteFilter => self.delete_selected_filter(cx),
            MenuCommand::SaveFilters => self.save_tat(cx),
            MenuCommand::ToggleLanguage => {
                self.language = self.language.toggle();
                cx.notify();
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::platform::primary_modifier(&event.keystroke.modifiers) {
            return;
        }
        match event.keystroke.key.as_str() {
            "o" => self.dispatch(MenuCommand::Open, window, cx),
            "r" => self.dispatch(MenuCommand::Refresh, window, cx),
            "s" => self.dispatch(MenuCommand::SaveFilters, window, cx),
            "tab" if !self.tabs.is_empty() => {
                self.set_active((self.active + 1) % self.tabs.len(), cx)
            }
            "w" => self.close_tab(self.active, cx),
            _ => {}
        }
    }

    fn menu_button(
        &self,
        command_group: Key,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let app = cx.entity();
        let lang = self.language;
        let recent = self.recent_files.clone();
        let only = self.show_only_filtered;
        let filters_open = self.filter_panel.read(cx).visible();
        let search_results_open = self.search_results_panel.read(cx).visible();
        let selected = self.selected_filter.is_some();
        let has_view = self.active_view().is_some();
        let active_encoding = self
            .active_view()
            .map(|view| view.read(cx).doc().encoding());
        let id = match command_group {
            Key::File => "menu-file",
            Key::View => "menu-view",
            Key::Encoding => "menu-encoding",
            _ => "menu-filters",
        };
        let button = Button::new(id)
            .xsmall()
            .ghost()
            .text_color(theme::c(theme::FG))
            .label(text(command_group, lang));

        match command_group {
            Key::File => {
                button
                    .dropdown_menu(move |menu, window, cx| {
                        let open_app = app.clone();
                        let refresh_app = app.clone();
                        let save_copy_app = app.clone();
                        let menu = menu
                            .item(PopupMenuItem::new(text(Key::Open, lang)).on_click(
                                window.listener_for(&open_app, |this, _, window, cx| {
                                    this.dispatch(MenuCommand::Open, window, cx)
                                }),
                            ))
                            .item(PopupMenuItem::new(text(Key::Refresh, lang)).on_click(
                                window.listener_for(&refresh_app, |this, _, window, cx| {
                                    this.dispatch(MenuCommand::Refresh, window, cx)
                                }),
                            ))
                            .item(
                                PopupMenuItem::new(text(Key::SaveEditedCopy, lang)).on_click(
                                    window.listener_for(&save_copy_app, |this, _, window, cx| {
                                        this.dispatch(MenuCommand::SaveEditedCopy, window, cx)
                                    }),
                                ),
                            );
                        let submenu_recent = recent.clone();
                        let submenu_app = app.clone();
                        menu.submenu(
                            text(Key::RecentFiles, lang),
                            window,
                            cx,
                            move |menu, window, _| {
                                if submenu_recent.is_empty() {
                                    return menu.item(
                                        PopupMenuItem::new(text(Key::NoRecentFiles, lang))
                                            .disabled(true),
                                    );
                                }
                                submenu_recent.iter().enumerate().fold(
                                    menu.max_w(px(480.)),
                                    |menu, (index, path)| {
                                        let target = path.clone();
                                        let target_app = submenu_app.clone();
                                        let full_path = path.display().to_string();
                                        menu.item(
                                            PopupMenuItem::element(move |_, _| {
                                                let label = full_path.clone();
                                                let tooltip = full_path.clone();
                                                div()
                                                    .id(("recent-file-label", index))
                                                    .w(px(440.))
                                                    .overflow_hidden()
                                                    .text_ellipsis_middle()
                                                    .child(label)
                                                    .tooltip(move |window, cx| {
                                                        gpui_component::tooltip::Tooltip::new(
                                                            tooltip.clone(),
                                                        )
                                                        .build(window, cx)
                                                    })
                                            })
                                            .on_click(window.listener_for(
                                                &target_app,
                                                move |this, _, window, cx| {
                                                    this.dispatch(
                                                        MenuCommand::OpenRecent(target.clone()),
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                        )
                                    },
                                )
                            },
                        )
                    })
                    .into_any_element()
            }
            Key::View => button
                .dropdown_menu(move |menu, window, cx| {
                    let all_app = app.clone();
                    let only_app = app.clone();
                    let panel_app = app.clone();
                    let search_panel_app = app.clone();
                    let language_app = app.clone();
                    let menu = menu
                        .item(
                            PopupMenuItem::new(text(Key::ShowAll, lang))
                                .checked(!only)
                                .on_click(window.listener_for(&all_app, |this, _, window, cx| {
                                    this.dispatch(MenuCommand::ShowAll, window, cx)
                                })),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::ShowOnlyFiltered, lang))
                                .checked(only)
                                .on_click(window.listener_for(&only_app, |this, _, window, cx| {
                                    this.dispatch(MenuCommand::ShowOnlyFiltered, window, cx)
                                })),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::ShowFilters, lang))
                                .checked(filters_open)
                                .on_click(window.listener_for(
                                    &panel_app,
                                    |this, _, window, cx| {
                                        this.dispatch(MenuCommand::ToggleFilters, window, cx)
                                    },
                                )),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::ShowSearchResults, lang))
                                .checked(search_results_open)
                                .on_click(window.listener_for(
                                    &search_panel_app,
                                    |this, _, window, cx| {
                                        this.dispatch(MenuCommand::ToggleSearchResults, window, cx)
                                    },
                                )),
                        );
                    let copy_app = app.clone();
                    let menu = menu.item(
                        PopupMenuItem::new(text(Key::Copy, lang))
                            .disabled(!has_view)
                            .on_click(window.listener_for(&copy_app, |this, _, window, cx| {
                                this.dispatch(MenuCommand::CopySelection, window, cx)
                            })),
                    );
                    menu.submenu(
                        text(Key::Language, lang),
                        window,
                        cx,
                        move |menu, window, _| {
                            menu.item(
                                PopupMenuItem::new(match lang {
                                    Language::ZhCn => text(Key::English, lang),
                                    Language::EnUs => text(Key::Chinese, lang),
                                })
                                .on_click(window.listener_for(
                                    &language_app,
                                    |this, _, window, cx| {
                                        this.dispatch(MenuCommand::ToggleLanguage, window, cx)
                                    },
                                )),
                            )
                        },
                    )
                })
                .into_any_element(),
            Key::Encoding => button
                .dropdown_menu(move |menu, window, _| {
                    Encoding::ALL
                        .iter()
                        .copied()
                        .fold(menu.scrollable(true), |menu, encoding| {
                            let encoding_app = app.clone();
                            menu.item(
                                PopupMenuItem::new(encoding.label())
                                    .checked(active_encoding == Some(encoding))
                                    .disabled(!has_view)
                                    .on_click(window.listener_for(
                                        &encoding_app,
                                        move |this, _, window, cx| {
                                            this.dispatch(
                                                MenuCommand::SetEncoding(encoding),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                            )
                        })
                })
                .into_any_element(),
            _ => button
                .dropdown_menu(move |menu, window, _| {
                    let add_app = app.clone();
                    let edit_app = app.clone();
                    let delete_app = app.clone();
                    let save_app = app.clone();
                    let import_app = app.clone();
                    let menu =
                        menu.item(PopupMenuItem::new(text(Key::ImportFilters, lang)).on_click(
                            window.listener_for(&import_app, |this, _, window, cx| {
                                this.dispatch(MenuCommand::ImportFilters, window, cx)
                            }),
                        ));
                    menu.item(PopupMenuItem::new(text(Key::AddFilter, lang)).on_click(
                        window.listener_for(&add_app, |this, _, window, cx| {
                            this.dispatch(MenuCommand::AddFilter, window, cx)
                        }),
                    ))
                    .item(
                        PopupMenuItem::new(text(Key::EditFilter, lang))
                            .disabled(!selected)
                            .on_click(window.listener_for(&edit_app, |this, _, window, cx| {
                                this.dispatch(MenuCommand::EditFilter, window, cx)
                            })),
                    )
                    .item(
                        PopupMenuItem::new(text(Key::DeleteFilter, lang))
                            .disabled(!selected)
                            .on_click(window.listener_for(&delete_app, |this, _, window, cx| {
                                this.dispatch(MenuCommand::DeleteFilter, window, cx)
                            })),
                    )
                    .item(
                        PopupMenuItem::new(text(Key::SaveFilter, lang)).on_click(
                            window.listener_for(&save_app, |this, _, window, cx| {
                                this.dispatch(MenuCommand::SaveFilters, window, cx)
                            }),
                        ),
                    )
                })
                .into_any_element(),
        }
    }

    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let left = h_flex()
            .h_full()
            .items_center()
            .gap_1()
            .pl_2()
            .child(
                div()
                    .id("title-app-drag")
                    .font_weight(FontWeight::SEMIBOLD)
                    .px_1()
                    .window_control_area(WindowControlArea::Drag)
                    .child("logd"),
            )
            .child(self.menu_button(Key::File, window, cx))
            .child(self.menu_button(Key::View, window, cx))
            .child(self.menu_button(Key::Encoding, window, cx))
            .child(self.menu_button(Key::Filters, window, cx))
            .into_any_element();
        let center = div()
            .w_full()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .bg(theme::c(theme::GUTTER_BG))
            .border_1()
            .border_color(theme::c(theme::BORDER))
            .rounded(px(4.))
            .text_color(theme::c(theme::FG))
            .child(Input::new(&self.keyword).small().appearance(false))
            .into_any_element();
        title_bar::render(left, center, window, self.language).into_any_element()
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let app = cx.entity();
        let lang = self.language;
        self.tab_scroll.scroll_to_item(self.active);
        let tabs = h_flex()
            .id("tab-strip")
            .w_full()
            .h(px(24.))
            .flex_none()
            .bg(theme::c(theme::GUTTER_BG))
            .border_b_1()
            .border_color(theme::c(theme::BORDER))
            .text_size(px(12.))
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == self.active;
                let drag = TabDrag {
                    index,
                    title: tab.title.clone(),
                };
                let drop_app = app.clone();
                let close_app = app.clone();
                let context_app = app.clone();
                h_flex()
                    .id(("tab", index))
                    .flex_none()
                    .h_full()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_r_1()
                    .border_color(theme::c(theme::BORDER))
                    .bg(theme::c(if active { theme::BG } else { theme::GUTTER_BG }))
                    .text_color(theme::c(if active { theme::FG } else { theme::MUTED }))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_active(index, cx)))
                    .on_drag(drag, move |drag, _, _, cx| cx.new(|_| drag.clone()))
                    .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                        this.reorder_tab(drag.index, index, cx);
                    }))
                    .child(tab.title.clone())
                    .child(
                        div()
                            .id(("close", index))
                            .px_1()
                            .child("x")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(index, cx)
                            })),
                    )
                    .context_menu(move |menu, window, _| {
                        let close_app = close_app.clone();
                        let before_app = context_app.clone();
                        let after_app = context_app.clone();
                        let clean_app = drop_app.clone();
                        menu.item(PopupMenuItem::new(text(Key::CloseTab, lang)).on_click(
                            window.listener_for(&close_app, move |this, _, _, cx| {
                                this.close_tab(index, cx);
                            }),
                        ))
                        .item(
                            PopupMenuItem::new(text(Key::CloseTabsBefore, lang)).on_click(
                                window.listener_for(&before_app, move |this, _, _, cx| {
                                    this.close_tabs_before(index, cx);
                                }),
                            ),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::CloseTabsAfter, lang)).on_click(
                                window.listener_for(&after_app, move |this, _, _, cx| {
                                    this.close_tabs_after(index, cx);
                                }),
                            ),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::CloseCleanTabs, lang)).on_click(
                                window.listener_for(&clean_app, move |this, _, _, cx| {
                                    this.close_clean_tabs(cx);
                                }),
                            ),
                        )
                    })
            }));

        // Scrollbars are painted as an overlay by gpui. Keep a small bottom
        // strip for it so the tab labels remain fully visible.
        v_flex()
            .id("tab-strip-frame")
            .w_full()
            .h(px(28.))
            .flex_none()
            .relative()
            .bg(theme::c(theme::GUTTER_BG))
            .child(tabs)
            .child(
                Scrollbar::horizontal(&self.tab_scroll)
                    .id("tab-scrollbar")
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout()
                    .styles(|styles| {
                        styles
                            .track(|track| {
                                track
                                    .width(px(4.))
                                    .bg(Hsla::from(theme::c(theme::SCROLL_TRACK)))
                            })
                            .track_hover(|track| {
                                track
                                    .width(px(4.))
                                    .bg(Hsla::from(theme::c(theme::SCROLL_TRACK)))
                            })
                            .track_active(|track| {
                                track
                                    .width(px(4.))
                                    .bg(Hsla::from(theme::c(theme::SCROLL_TRACK)))
                            })
                            .thumb(|thumb| thumb.width(px(4.)).inset(px(0.)))
                            .thumb_hover(|thumb| thumb.width(px(4.)).inset(px(0.)))
                            .thumb_active(|thumb| thumb.width(px(4.)).inset(px(0.)))
                    }),
            )
            .into_any_element()
    }

    pub fn render_workspace(&self, _cx: &App) -> AnyElement {
        self.active_view()
            .cloned()
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| {
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
                            .child(text(Key::Open, self.language)),
                    )
                    .into_any_element()
            })
    }

    pub fn render_filters(
        app: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<FilterPanel>,
    ) -> AnyElement {
        let (
            filters,
            selected,
            editor_open,
            editor_text,
            editor_description,
            filter_fore,
            filter_back,
            filter_scope,
            lang,
        ) = {
            let state = app.read(cx);
            (
                state.filters.clone(),
                state.selected_filter,
                state.filter_editor_open,
                state.filter_text.clone(),
                state.filter_description.clone(),
                state.filter_fore.clone(),
                state.filter_back.clone(),
                state.filter_scope,
                state.language,
            )
        };

        let add_app = app.clone();
        let cancel_app = app.clone();

        v_flex()
            .size_full()
            .bg(theme::c(theme::BG))
            .text_color(theme::c(theme::FG))
            .text_size(px(12.))
            .child(
                h_flex()
                    .h(px(32.))
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme::c(theme::BORDER))
                    .child(
                        ButtonGroup::new("filter-toolbar")
                            .compact()
                            .xsmall()
                            .outline()
                            .child(
                                Button::new("filter-add")
                                    .icon(IconName::Plus)
                                    .accessibility_label(text(Key::AddFilter, lang))
                                    .tooltip(text(Key::AddFilter, lang))
                                    .on_click(
                                        window.listener_for(&add_app, |this, _, window, cx| {
                                            this.begin_add_filter(window, cx)
                                        }),
                                    ),
                            ),
                    )
                    .child(div().flex_1()),
            )
            .when(editor_open, |column| {
                column.child(
                    v_flex()
                        .p_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme::c(theme::BORDER))
                        .child(Input::new(&editor_text).small())
                        .child(Input::new(&editor_description).small())
                        .child(
                            ButtonGroup::new("filter-editor-scope")
                                .compact()
                                .xsmall()
                                .outline()
                                .child(
                                    Button::new("filter-scope")
                                        .icon(IconName::Globe)
                                        .selected(filter_scope == FilterScope::AllFiles)
                                        .accessibility_label(format!(
                                            "{}: {}",
                                            text(Key::FilterScope, lang),
                                            filter_scope_label(filter_scope, lang)
                                        ))
                                        .tooltip(format!(
                                            "{}: {}",
                                            text(Key::FilterScope, lang),
                                            filter_scope_label(filter_scope, lang)
                                        ))
                                        .on_click(window.listener_for(app, |this, _, _, cx| {
                                            this.filter_scope = match this.filter_scope {
                                                FilterScope::AllFiles => FilterScope::CurrentFile,
                                                FilterScope::CurrentFile => FilterScope::AllFiles,
                                            };
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_4()
                                .items_center()
                                .child(
                                    ColorPicker::new(&filter_fore)
                                        // The component's default featured colors are
                                        // repeated in its full palette and reuse the same
                                        // accessibility IDs in debug builds.
                                        .featured_colors(Vec::new())
                                        .small()
                                        .label(text(Key::ForegroundColor, lang))
                                        .accessibility_label(text(Key::ForegroundColor, lang)),
                                )
                                .child(
                                    ColorPicker::new(&filter_back)
                                        .featured_colors(Vec::new())
                                        .small()
                                        .label(text(Key::BackgroundColor, lang))
                                        .accessibility_label(text(Key::BackgroundColor, lang)),
                                ),
                        )
                        .child(
                            h_flex().justify_end().child(
                                ButtonGroup::new("filter-editor-actions")
                                    .compact()
                                    .xsmall()
                                    .outline()
                                    .child(
                                        Button::new("filter-editor-cancel")
                                            .icon(IconName::Close)
                                            .accessibility_label(text(Key::Cancel, lang))
                                            .tooltip(text(Key::Cancel, lang))
                                            .on_click(window.listener_for(
                                                &cancel_app,
                                                |this, _, _, cx| {
                                                    this.filter_editor_open = false;
                                                    this.editing_filter = None;
                                                    cx.notify();
                                                },
                                            )),
                                    )
                                    .child(
                                        Button::new("filter-editor-save")
                                            .icon(IconName::Check)
                                            .accessibility_label(text(Key::SaveFilter, lang))
                                            .tooltip(text(Key::SaveFilter, lang))
                                            .on_click(window.listener_for(
                                                app,
                                                |this, _, window, cx| {
                                                    this.commit_filter(window, cx)
                                                },
                                            )),
                                    ),
                            ),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("filter-list")
                    .flex_1()
                    .overflow_y_scrollbar()
                    .children(filters.iter().enumerate().map(|(index, filter)| {
                        render_filter_row(app, filter, index, selected, lang, window)
                    }))
                    .when(filters.is_empty(), |list| {
                        list.child(
                            div()
                                .p_3()
                                .text_color(theme::c(theme::MUTED))
                                .child(text(Key::NoFilters, lang)),
                        )
                    }),
            )
            .into_any_element()
    }

    pub fn render_search_results(
        app: &Entity<Self>,
        vertical_scroll: &UniformListScrollHandle,
        collapsed_files: &HashSet<PathBuf>,
        remembered_content_width: f32,
        panel_handle: WeakEntity<SearchResultsPanel>,
        _window: &mut Window,
        cx: &mut Context<SearchResultsPanel>,
    ) -> AnyElement {
        let (has_search, files, total_matches, tree_rows, scanning, file_count, lang) = {
            let state = app.read(cx);
            let has_search = !state.search_filters.is_empty();
            let mut files = Vec::new();
            let mut total_matches = 0usize;
            let mut tree_rows = 0usize;
            let mut scanning = 0usize;
            for (tab_index, tab) in state.tabs.iter().enumerate() {
                let view = tab.view.read(cx);
                if has_search
                    && (view.search_scanning_progress().is_some()
                        || view.indexing_progress().is_some())
                {
                    scanning += 1;
                }
                let Some(lines) = view.search_matches() else {
                    continue;
                };
                if lines.is_empty() {
                    continue;
                }
                let expanded = !collapsed_files.contains(&tab.path);
                let start = tree_rows;
                tree_rows =
                    tree_rows.saturating_add(search_tree_file_row_count(lines.len(), expanded));
                total_matches = total_matches.saturating_add(lines.len());
                files.push(SearchResultFile {
                    tab_index,
                    path: tab.path.clone(),
                    full_path: tab.path.display().to_string(),
                    view: tab.view.clone(),
                    lines,
                    start,
                    expanded,
                });
            }
            (
                has_search,
                Arc::new(files),
                total_matches,
                tree_rows,
                scanning,
                state.tabs.len(),
                state.language,
            )
        };

        let summary = format!(
            "{} {}  |  {} {}",
            group(total_matches as u64),
            text(Key::Matches, lang),
            group(files.len() as u64),
            text(Key::Files, lang)
        );
        let panel =
            v_flex()
                .size_full()
                .min_h_0()
                .bg(theme::c(theme::BG))
                .text_color(theme::c(theme::FG))
                .text_size(px(12.))
                .child(
                    h_flex()
                        .h(px(28.))
                        .flex_none()
                        .px_2()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(theme::c(theme::BORDER))
                        .child(summary)
                        .when(scanning > 0, |header| {
                            header.child(div().text_color(theme::c(theme::SEARCH_FORE)).child(
                                format!(
                                    "{} {}/{}",
                                    text(Key::SearchInProgress, lang),
                                    file_count.saturating_sub(scanning),
                                    file_count,
                                ),
                            ))
                        }),
                );

        if !has_search {
            return panel
                .child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(theme::c(theme::MUTED))
                        .child(text(Key::SearchResultsPrompt, lang)),
                )
                .into_any_element();
        }
        if total_matches == 0 {
            let message = if scanning > 0 {
                text(Key::SearchInProgress, lang)
            } else {
                text(Key::NoSearchResults, lang)
            };
            return panel
                .child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(theme::c(theme::MUTED))
                        .child(message),
                )
                .into_any_element();
        }

        let row_app = app.clone();
        let row_panel = panel_handle.clone();
        let path_width = files
            .iter()
            .map(|file| estimated_search_text_width(&file.full_path) + 150.0)
            .fold(0.0, f32::max);
        let content_width = remembered_content_width.max(path_width).max(720.0);
        let observed_content_width = content_width;
        let rows = uniform_list(
            "multi-file-search-results",
            tree_rows,
            move |range, window, cx| {
                let mut elements = Vec::with_capacity(range.len());
                let mut widest = observed_content_width;
                for result_index in range {
                    let file_index = files
                        .partition_point(|file| file.start <= result_index)
                        .saturating_sub(1);
                    let file = &files[file_index];
                    if result_index == file.start {
                        let target_panel = row_panel.clone();
                        let path = file.path.clone();
                        let expanded = file.expanded;
                        let match_count = file.lines.len();
                        let count = search_file_match_count(match_count, lang);
                        elements.push(
                            h_flex()
                                .id(("search-result-file", result_index))
                                .h(px(24.))
                                .w(px(content_width))
                                .flex_none()
                                .px_2()
                                .gap_1()
                                .items_center()
                                .whitespace_nowrap()
                                .bg(theme::c(theme::GUTTER_BG))
                                .border_b_1()
                                .border_color(theme::c(theme::BORDER))
                                .hover(|row| row.bg(theme::c(theme::CONTROL_HOVER)))
                                .on_click(move |_, _, cx| {
                                    target_panel
                                        .update(cx, |panel, cx| {
                                            panel.toggle_file(path.clone(), result_index, cx)
                                        })
                                        .ok();
                                })
                                .child(
                                    Icon::new(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .xsmall()
                                    .text_color(theme::c(theme::MUTED)),
                                )
                                .child(
                                    Icon::new(IconName::FileText)
                                        .xsmall()
                                        .text_color(theme::c(theme::MUTED)),
                                )
                                .child(file.full_path.clone())
                                .child(div().text_color(theme::c(theme::MUTED)).child(count)),
                        );
                        continue;
                    }

                    let line_index = result_index - file.start - 1;
                    let Some(&file_line) = file.lines.get(line_index) else {
                        continue;
                    };
                    let line_text = file
                        .view
                        .read(cx)
                        .doc()
                        .line_text(file_line)
                        .unwrap_or_default();
                    widest = widest.max(estimated_search_text_width(&line_text) + 142.0);
                    let target_app = row_app.clone();
                    let tab_index = file.tab_index;
                    elements.push(
                        h_flex()
                            .id(("search-result", result_index))
                            .h(px(24.))
                            .w(px(content_width))
                            .flex_none()
                            .items_center()
                            .border_b_1()
                            .border_color(theme::c(theme::BORDER))
                            .hover(|row| row.bg(theme::c(theme::CONTROL_HOVER)))
                            .on_click(window.listener_for(&target_app, move |this, _, _, cx| {
                                this.goto_search_result(tab_index, file_line, cx)
                            }))
                            .child(
                                div()
                                    .w(px(34.))
                                    .h_full()
                                    .flex_none()
                                    .border_r_1()
                                    .border_color(theme::c(theme::BORDER)),
                            )
                            .child(
                                div()
                                    .w(px(98.))
                                    .flex_none()
                                    .px_2()
                                    .text_right()
                                    .text_color(theme::c(theme::MUTED))
                                    .child(search_line_label(file_line + 1, lang)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .px_2()
                                    .whitespace_nowrap()
                                    .child(line_text),
                            ),
                    );
                }
                if widest > observed_content_width {
                    row_panel
                        .update(cx, |panel, cx| panel.observe_content_width(widest, cx))
                        .ok();
                }
                elements
            },
        )
        .size_full()
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(vertical_scroll);

        let scrollbar_width = Scrollbar::width();

        panel
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right(scrollbar_width)
                            .bottom(scrollbar_width)
                            .child(rows),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom(scrollbar_width)
                            .w(scrollbar_width)
                            .child(
                                Scrollbar::vertical(vertical_scroll)
                                    .id("search-results-vscrollbar")
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right(scrollbar_width)
                            .bottom_0()
                            .h(scrollbar_width)
                            .child(
                                Scrollbar::horizontal(vertical_scroll)
                                    .id("search-results-hscrollbar")
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            )
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
        if let Some(view) = self.active_view() {
            let view = view.read(cx);
            let doc = view.doc();
            let suffix = if doc.index_complete() { "" } else { "+" };
            bar = bar
                .child(format!(
                    "{}{} {}",
                    group(doc.total_file_lines()),
                    suffix,
                    text(Key::Lines, self.language)
                ))
                .child(format!(
                    "{} {}",
                    text(Key::Line, self.language),
                    group(doc.top_file_line().map(|line| line + 1).unwrap_or(0))
                ))
                .child(doc.encoding().label().to_string());
            if let Some(count) = doc.match_count() {
                bar = bar.child(format!(
                    "{} {}",
                    text(Key::Matches, self.language),
                    group(count as u64)
                ));
            }
            if let Some(progress) = view.indexing_progress() {
                bar = bar.child(format!(
                    "{} {:.0}%",
                    text(Key::Indexing, self.language),
                    progress * 100.
                ));
            }
            if let Some(progress) = view.scanning_progress() {
                bar = bar.child(format!(
                    "{} {:.0}%",
                    text(Key::Filtering, self.language),
                    progress * 100.
                ));
            }
            if view.from_cache() {
                bar = bar.child(text(Key::Cache, self.language));
            }
            if let Some(error) = view.error() {
                bar = bar.child(error.to_string());
            }
        } else {
            bar = bar.child(text(Key::Ready, self.language));
        }
        if let Some(status) = &self.status {
            bar = bar.child(status.clone());
        }
        bar.into_any_element()
    }
}

impl Render for LogdApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.render_title_bar(window, cx);
        let tabs = self.render_tabs(cx);
        let status = self.render_status(cx);
        v_flex()
            .id("root")
            .key_context("Logd")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme::c(theme::BG))
            .on_key_down(cx.listener(Self::on_key))
            .child(title)
            .when(!self.tabs.is_empty(), |root| root.child(tabs))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.dock_area.clone()),
            )
            .child(status)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                for path in paths.paths() {
                    this.open_path(path, window, cx);
                }
            }))
    }
}

fn render_filter_row(
    app: &Entity<LogdApp>,
    filter: &FilterSpec,
    index: usize,
    selected: Option<usize>,
    lang: Language,
    window: &mut Window,
) -> AnyElement {
    let row_app = app.clone();
    let double_app = app.clone();
    let enable_app = app.clone();
    let exclude_app = app.clone();
    let mode_app = app.clone();
    let regex_app = app.clone();
    let case_app = app.clone();
    let scope_app = app.clone();
    let context_app = app.clone();
    h_flex()
        .id(("filter-row", index))
        .min_h(px(28.))
        .px_2()
        .gap_2()
        .items_center()
        .when(selected == Some(index), |row| {
            row.bg(theme::c(theme::SELECTION))
        })
        .on_click(window.listener_for(&row_app, move |this, _, _, cx| {
            this.selected_filter = Some(index);
            cx.notify();
        }))
        .on_double_click(
            window.listener_for(&double_app, move |this, _, window, cx| {
                this.selected_filter = Some(index);
                this.begin_edit_filter(window, cx);
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            window.listener_for(&row_app, move |this, _, _, cx| {
                this.selected_filter = Some(index);
                cx.notify();
            }),
        )
        .child(
            Checkbox::new(("filter-enabled", index))
                .xsmall()
                .checked(filter.enabled)
                .accessibility_label(text(Key::FilterEnabled, lang))
                .tooltip(text(Key::FilterEnabled, lang))
                .on_click(
                    window.listener_for(&enable_app, move |this, checked, _, cx| {
                        this.filters[index].enabled = *checked;
                        this.filters_changed(cx);
                    }),
                ),
        )
        .child(
            ButtonGroup::new(("filter-options", index))
                .compact()
                .xsmall()
                .outline()
                .multiple(true)
                .child(
                    Button::new(("filter-exclude", index))
                        .icon(IconName::Minus)
                        .selected(filter.excluding)
                        .accessibility_label(text(Key::FilterExcluding, lang))
                        .tooltip(text(Key::FilterExcluding, lang))
                        .on_click(window.listener_for(&exclude_app, move |this, _, _, cx| {
                            this.filters[index].excluding = !this.filters[index].excluding;
                            this.filters_changed(cx);
                        })),
                )
                .child(
                    Button::new(("filter-mode", index))
                        .icon(IconName::GalleryVerticalEnd)
                        .selected(filter.mode == HighlightMode::Line)
                        .accessibility_label(text(Key::FilterHighlightLine, lang))
                        .tooltip(text(Key::FilterHighlightLine, lang))
                        .on_click(window.listener_for(&mode_app, move |this, _, _, cx| {
                            this.filters[index].mode = match this.filters[index].mode {
                                HighlightMode::Field => HighlightMode::Line,
                                HighlightMode::Line => HighlightMode::Field,
                            };
                            this.filters_changed(cx);
                        })),
                )
                .child(
                    Button::new(("filter-regex", index))
                        .icon(IconName::Asterisk)
                        .selected(filter.regex)
                        .accessibility_label(text(Key::FilterRegex, lang))
                        .tooltip(text(Key::FilterRegex, lang))
                        .on_click(window.listener_for(&regex_app, move |this, _, _, cx| {
                            this.filters[index].regex = !this.filters[index].regex;
                            this.filters_changed(cx);
                        })),
                )
                .child(
                    Button::new(("filter-case", index))
                        .icon(IconName::CaseSensitive)
                        .selected(filter.case_sensitive)
                        .accessibility_label(text(Key::FilterCaseSensitive, lang))
                        .tooltip(text(Key::FilterCaseSensitive, lang))
                        .on_click(window.listener_for(&case_app, move |this, _, _, cx| {
                            this.filters[index].case_sensitive =
                                !this.filters[index].case_sensitive;
                            this.filters_changed(cx);
                        })),
                )
                .child(
                    Button::new(("filter-scope", index))
                        .icon(IconName::Globe)
                        .selected(filter.scope == FilterScope::AllFiles)
                        .accessibility_label(format!(
                            "{}: {}",
                            text(Key::FilterScope, lang),
                            filter_scope_label(filter.scope, lang)
                        ))
                        .tooltip(format!(
                            "{}: {}",
                            text(Key::FilterScope, lang),
                            filter_scope_label(filter.scope, lang)
                        ))
                        .on_click(window.listener_for(&scope_app, move |this, _, _, cx| {
                            this.filters[index].scope = match this.filters[index].scope {
                                FilterScope::AllFiles => FilterScope::CurrentFile,
                                FilterScope::CurrentFile => FilterScope::AllFiles,
                            };
                            this.filters_changed(cx);
                        })),
                ),
        )
        .child(v_flex().min_w_0().flex_1().child(filter.text.clone()).when(
            !filter.description.is_empty(),
            |item| {
                item.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme::c(theme::MUTED))
                        .child(filter.description.clone()),
                )
            },
        ))
        .context_menu(move |menu, window, _| {
            let edit_app = context_app.clone();
            let delete_app = context_app.clone();
            menu.item(PopupMenuItem::new(text(Key::EditFilter, lang)).on_click(
                window.listener_for(&edit_app, move |this, _, window, cx| {
                    this.selected_filter = Some(index);
                    this.begin_edit_filter(window, cx);
                }),
            ))
            .item(
                PopupMenuItem::new(text(Key::DeleteFilter, lang)).on_click(window.listener_for(
                    &delete_app,
                    move |this, _, _, cx| {
                        this.selected_filter = Some(index);
                        this.delete_selected_filter(cx);
                    },
                )),
            )
        })
        .into_any_element()
}

fn set_picker_color(
    picker: &Entity<ColorPickerState>,
    color: Option<u32>,
    window: &mut Window,
    cx: &mut App,
) {
    picker.update(cx, |picker, cx| match color {
        Some(color) => picker.set_value(theme::c(color), window, cx),
        None => picker.clear_value(window, cx),
    });
}

fn hsla_to_rgb(color: Hsla) -> u32 {
    u32::from(color.to_rgb()) >> 8
}

fn estimated_search_text_width(value: &str) -> f32 {
    value
        .chars()
        .map(|ch| if ch.is_ascii() { 0.62 } else { 1.0 })
        .sum::<f32>()
        * 12.0
}

fn search_tree_file_row_count(matches: usize, expanded: bool) -> usize {
    1usize.saturating_add(if expanded { matches } else { 0 })
}

fn search_file_match_count(count: usize, lang: Language) -> String {
    match lang {
        Language::ZhCn => format!("（匹配 {} 次）", group(count as u64)),
        Language::EnUs => format!("({} matches)", group(count as u64)),
    }
}

fn search_line_label(line: u64, lang: Language) -> String {
    match lang {
        Language::ZhCn => format!("行 {}:", group(line)),
        Language::EnUs => format!("Ln {}:", group(line)),
    }
}

fn group(number: u64) -> String {
    let source = number.to_string();
    let mut output = String::with_capacity(source.len() + source.len() / 3);
    for (index, ch) in source.chars().enumerate() {
        if index > 0 && (source.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

fn split_search_keywords(value: &str) -> Vec<String> {
    value
        .split('|')
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
        .map(str::to_owned)
        .collect()
}

fn filter_scope_label(scope: FilterScope, lang: Language) -> &'static str {
    text(
        match scope {
            FilterScope::CurrentFile => Key::FilterCurrentFile,
            FilterScope::AllFiles => Key::FilterAllFiles,
        },
        lang,
    )
}

pub fn run(initial: Vec<PathBuf>) {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    app.run(move |cx| {
        gpui_component::init(cx);
        // gpui-component defaults to Light; logd uses a dark client-side shell.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        let initial = initial.clone();
        let options = crate::platform::window_options(cx);
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| LogdApp::new(initial, window, cx));
                cx.new(|cx| Root::new(view, window, cx).bordered(false))
            })
            .expect("failed to create logd window");
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::{group, search_tree_file_row_count, split_search_keywords};

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(500_000_000), "500,000,000");
    }

    #[test]
    fn splits_search_keywords_on_pipe() {
        assert_eq!(
            split_search_keywords("  first | second|| third |  "),
            vec!["first", "second", "third"]
        );
        assert!(split_search_keywords(" | ").is_empty());
    }

    #[test]
    fn collapsed_search_file_keeps_only_its_header_row() {
        assert_eq!(search_tree_file_row_count(1_314, true), 1_315);
        assert_eq!(search_tree_file_row_count(1_314, false), 1);
    }
}
