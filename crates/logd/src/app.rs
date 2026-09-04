//! Desktop shell and application command routing.
//!
//! Configured filters can target one tab or all imported tabs. Only the active
//! tab is rescanned immediately after a filter edit; inactive tabs are marked
//! dirty until activated. Title-bar searches are temporary and scan every tab.

use std::path::{Path, PathBuf};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::dock::{panel_handle, DockArea, DockLayout, DockPlacement};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{h_flex, v_flex, InteractiveElementExt as _, Root, Selectable as _, Sizable};
use logd_core::{Encoding, FilterScope, FilterSpec, HighlightMode, TatFile};

use crate::i18n::{text, Key, Language};
use crate::log_view::LogView;
use crate::theme;
use crate::ui::dock::{logd_dock_area, FilterPanel, LogPanel};
use crate::ui::title_bar;

struct Tab {
    view: Entity<LogView>,
    title: String,
    path: PathBuf,
}

#[derive(Clone)]
struct TabDrag {
    index: usize,
    title: String,
}

impl Render for TabDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .h(px(28.))
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
    ToggleEncoding,
    CopySelection,
    ShowAll,
    ShowOnlyFiltered,
    ToggleFilters,
    AddFilter,
    EditFilter,
    DeleteFilter,
    SaveFilters,
    ToggleLanguage,
    DockFilters(DockPlacement),
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
    filter_placement: DockPlacement,
    filters_open: bool,
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

        cx.subscribe_in(&keyword, window, |this, state, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.set_global_search(state.read(cx).value().to_string(), cx);
            }
        })
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
        let filter_panel = cx.new(|cx| FilterPanel::new(app, cx));
        let (dock_area, skin) = logd_dock_area("logd.main", Some(1), window, cx);
        // The View menu is the single visibility control; avoid a duplicate dock toggle button.
        skin.set_toggle_button_visible(false, cx);
        dock_area.update(cx, |dock, cx| {
            dock.set_center(
                DockLayout::tabs().panel_view(panel_handle(log_panel), cx),
                window,
                cx,
            );
            dock.set_dock(
                DockPlacement::Right,
                DockLayout::tabs().panel_view(panel_handle(filter_panel.clone()), cx),
                window,
                cx,
            );
            dock.set_dock_size(DockPlacement::Right, px(360.), window, cx);
            dock.set_dock_collapsible(DockPlacement::Right, true, window, cx);
        });

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
            filter_placement: DockPlacement::Right,
            filters_open: true,
            language,
            status: None,
            focus: cx.focus_handle(),
        };
        for path in initial {
            this.open_path(&path, window, cx);
        }
        this
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

    fn refresh_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let path = tab.path.clone();
        match LogView::load(&path) {
            Ok(loaded) => {
                let view = cx.new(|cx| LogView::new(loaded, window, cx));
                self.apply_filters_to_view(self.active, &view, cx);
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
        cx.notify();
    }

    fn active_view(&self) -> Option<&Entity<LogView>> {
        self.tabs.get(self.active).map(|tab| &tab.view)
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

    fn set_global_search(&mut self, value: String, cx: &mut Context<Self>) {
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
        let fore = theme::PALETTE[self.filters.len() % theme::PALETTE.len()];
        self.filter_fore.update(cx, |picker, cx| {
            picker.set_value(theme::c(fore), window, cx)
        });
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

    fn toggle_encoding(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.active_view().cloned() else {
            return;
        };
        let next = match view.read(cx).doc().encoding() {
            Encoding::Utf8 => Encoding::Gb18030,
            Encoding::Gb18030 => Encoding::Utf8,
        };
        let filters = self.filters_for_tab(self.active);
        view.update(cx, |view, cx| view.set_encoding(next, filters, cx));
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
        if self.filters_open != show {
            self.dock_area.update(cx, |dock, cx| {
                dock.toggle_dock(self.filter_placement, window, cx)
            });
            self.filters_open = show;
        }
        cx.notify();
    }

    fn move_filter_panel(
        &mut self,
        placement: DockPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if placement == self.filter_placement {
            self.show_filter_panel(true, window, cx);
            return;
        }
        let old = self.filter_placement;
        let panel = self.filter_panel.clone();
        self.dock_area.update(cx, |dock, cx| {
            dock.remove_dock(old, window, cx);
            dock.set_dock(
                placement,
                DockLayout::tabs().panel_view(panel_handle(panel), cx),
                window,
                cx,
            );
            let size = if placement == DockPlacement::Bottom {
                px(240.)
            } else {
                px(360.)
            };
            dock.set_dock_size(placement, size, window, cx);
            dock.set_dock_collapsible(placement, true, window, cx);
        });
        self.filter_placement = placement;
        self.filters_open = true;
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
            MenuCommand::ToggleEncoding => self.toggle_encoding(cx),
            MenuCommand::CopySelection => {
                if let Some(view) = self.active_view().cloned() {
                    view.update(cx, |view, cx| view.copy_selection(cx));
                }
            }
            MenuCommand::ShowAll => self.set_show_only(false, cx),
            MenuCommand::ShowOnlyFiltered => self.set_show_only(true, cx),
            MenuCommand::ToggleFilters => self.show_filter_panel(!self.filters_open, window, cx),
            MenuCommand::AddFilter => self.begin_add_filter(window, cx),
            MenuCommand::EditFilter => self.begin_edit_filter(window, cx),
            MenuCommand::DeleteFilter => self.delete_selected_filter(cx),
            MenuCommand::SaveFilters => self.save_tat(cx),
            MenuCommand::ToggleLanguage => {
                self.language = self.language.toggle();
                cx.notify();
            }
            MenuCommand::DockFilters(placement) => self.move_filter_panel(placement, window, cx),
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
        let filters_open = self.filters_open;
        let selected = self.selected_filter.is_some();
        let placement = self.filter_placement;
        let has_view = self.active_view().is_some();
        let id = match command_group {
            Key::File => "menu-file",
            Key::View => "menu-view",
            _ => "menu-filters",
        };
        let button = Button::new(id)
            .xsmall()
            .ghost()
            .text_color(theme::c(theme::FG))
            .label(text(command_group, lang));

        match command_group {
            Key::File => button
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
                            submenu_recent.iter().fold(menu, |menu, path| {
                                let target = path.clone();
                                let target_app = submenu_app.clone();
                                menu.item(PopupMenuItem::new(path.display().to_string()).on_click(
                                    window.listener_for(&target_app, move |this, _, window, cx| {
                                        this.dispatch(
                                            MenuCommand::OpenRecent(target.clone()),
                                            window,
                                            cx,
                                        )
                                    }),
                                ))
                            })
                        },
                    )
                })
                .into_any_element(),
            Key::View => button
                .dropdown_menu(move |menu, window, cx| {
                    let all_app = app.clone();
                    let only_app = app.clone();
                    let panel_app = app.clone();
                    let left_app = app.clone();
                    let right_app = app.clone();
                    let bottom_app = app.clone();
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
                        .separator()
                        .item(
                            PopupMenuItem::new(text(Key::DockLeft, lang))
                                .checked(placement == DockPlacement::Left)
                                .on_click(window.listener_for(&left_app, |this, _, window, cx| {
                                    this.dispatch(
                                        MenuCommand::DockFilters(DockPlacement::Left),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::DockRight, lang))
                                .checked(placement == DockPlacement::Right)
                                .on_click(window.listener_for(
                                    &right_app,
                                    |this, _, window, cx| {
                                        this.dispatch(
                                            MenuCommand::DockFilters(DockPlacement::Right),
                                            window,
                                            cx,
                                        )
                                    },
                                )),
                        )
                        .item(
                            PopupMenuItem::new(text(Key::DockBottom, lang))
                                .checked(placement == DockPlacement::Bottom)
                                .on_click(window.listener_for(
                                    &bottom_app,
                                    |this, _, window, cx| {
                                        this.dispatch(
                                            MenuCommand::DockFilters(DockPlacement::Bottom),
                                            window,
                                            cx,
                                        )
                                    },
                                )),
                        );
                    let encoding_app = app.clone();
                    let copy_app = app.clone();
                    let menu = menu
                        .item(
                            PopupMenuItem::new(text(Key::Encoding, lang))
                                .disabled(!has_view)
                                .on_click(window.listener_for(
                                    &encoding_app,
                                    |this, _, window, cx| {
                                        this.dispatch(MenuCommand::ToggleEncoding, window, cx)
                                    },
                                )),
                        )
                        .item(
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
            _ => button
                .dropdown_menu(move |menu, window, _| {
                    let add_app = app.clone();
                    let edit_app = app.clone();
                    let delete_app = app.clone();
                    let save_app = app.clone();
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
        let right = self
            .active_view()
            .map(|view| view.read(cx).doc().encoding().label().to_string())
            .unwrap_or_default();
        title_bar::render(
            left,
            center,
            div()
                .pr_2()
                .text_size(px(11.))
                .text_color(theme::c(theme::MUTED))
                .child(right)
                .into_any_element(),
            window,
            self.language,
        )
        .into_any_element()
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let app = cx.entity();
        let lang = self.language;
        self.tab_scroll.scroll_to_item(self.active);
        h_flex()
            .id("tab-strip")
            .w_full()
            .h(px(28.))
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
            }))
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
                        Button::new("filter-add")
                            .xsmall()
                            .label("+")
                            .tooltip(text(Key::AddFilter, lang))
                            .on_click(window.listener_for(&add_app, |this, _, window, cx| {
                                this.begin_add_filter(window, cx)
                            })),
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
                            Button::new("filter-scope")
                                .xsmall()
                                .label(filter_scope_label(filter_scope, lang))
                                .tooltip(text(Key::FilterScope, lang))
                                .on_click(window.listener_for(app, |this, _, _, cx| {
                                    this.filter_scope = match this.filter_scope {
                                        FilterScope::AllFiles => FilterScope::CurrentFile,
                                        FilterScope::CurrentFile => FilterScope::AllFiles,
                                    };
                                    cx.notify();
                                })),
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
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("filter-editor-cancel")
                                        .xsmall()
                                        .label(text(Key::Cancel, lang))
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
                                        .xsmall()
                                        .label(text(Key::SaveFilter, lang))
                                        .on_click(
                                            window.listener_for(app, |this, _, window, cx| {
                                                this.commit_filter(window, cx)
                                            }),
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
    let fg_app = app.clone();
    let bg_app = app.clone();
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
            Button::new(("filter-enabled", index))
                .xsmall()
                .label(if filter.enabled { "[x]" } else { "[ ]" })
                .on_click(window.listener_for(&enable_app, move |this, _, _, cx| {
                    this.filters[index].enabled = !this.filters[index].enabled;
                    this.filters_changed(cx);
                })),
        )
        .child(
            Button::new(("filter-exclude", index))
                .xsmall()
                .label(if filter.excluding { "-" } else { "+" })
                .on_click(window.listener_for(&exclude_app, move |this, _, _, cx| {
                    this.filters[index].excluding = !this.filters[index].excluding;
                    this.filters_changed(cx);
                })),
        )
        .child(
            Button::new(("filter-mode", index))
                .xsmall()
                .label(match filter.mode {
                    HighlightMode::Field => "Aa",
                    HighlightMode::Line => "Ln",
                })
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
                .xsmall()
                .label(".*")
                .selected(filter.regex)
                .on_click(window.listener_for(&regex_app, move |this, _, _, cx| {
                    this.filters[index].regex = !this.filters[index].regex;
                    this.filters_changed(cx);
                })),
        )
        .child(
            Button::new(("filter-case", index))
                .xsmall()
                .label("Aa")
                .selected(filter.case_sensitive)
                .on_click(window.listener_for(&case_app, move |this, _, _, cx| {
                    this.filters[index].case_sensitive = !this.filters[index].case_sensitive;
                    this.filters_changed(cx);
                })),
        )
        .child(
            Button::new(("filter-scope", index))
                .xsmall()
                .label(filter_scope_label(filter.scope, lang))
                .tooltip(text(Key::FilterScope, lang))
                .on_click(window.listener_for(&scope_app, move |this, _, _, cx| {
                    this.filters[index].scope = match this.filters[index].scope {
                        FilterScope::AllFiles => FilterScope::CurrentFile,
                        FilterScope::CurrentFile => FilterScope::AllFiles,
                    };
                    this.filters_changed(cx);
                })),
        )
        .child(swatch(
            ("filter-fg", index),
            "A",
            filter.fore,
            window.listener_for(&fg_app, move |this, _, _, cx| {
                this.filters[index].fore = theme::next_color(this.filters[index].fore);
                this.filters_changed(cx);
            }),
        ))
        .child(swatch(
            ("filter-bg", index),
            "#",
            filter.back,
            window.listener_for(&bg_app, move |this, _, _, cx| {
                this.filters[index].back = theme::next_color(this.filters[index].back);
                this.filters_changed(cx);
            }),
        ))
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

fn swatch(
    id: (&'static str, usize),
    glyph: &'static str,
    color: Option<u32>,
    click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .w(px(18.))
        .text_color(theme::c(color.unwrap_or(theme::BORDER)))
        .child(glyph)
        .on_click(click)
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
    use super::{group, split_search_keywords};

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
}
