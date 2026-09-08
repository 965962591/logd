//! Desktop shell and application command routing.
//!
//! Configured filters can target one tab or all imported tabs. Current-file
//! changes can defer inactive-tab scans; all-files filters rescan every tab so
//! the multi-file results panel stays current. Title-bar searches are temporary
//! and scan every tab.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_component::dialog::DialogFooter;
use gpui_component::dock::{
    panel_handle, DockArea, DockAreaState, DockEvent, DockLayout, DockPlacement,
};
use gpui_component::input::{Enter, Input, InputEvent, InputState};
use gpui_component::link::Link;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::progress::Progress;
use gpui_component::scroll::{ScrollableElement, Scrollbar, ScrollbarMode};
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _,
    Root, Selectable as _, Sizable, WindowExt as _,
};
use logd_core::{Encoding, FilterScope, FilterSpec, HighlightMode, LogdFile, TatFile};

use crate::i18n::{text, Key, Language};
use crate::log_analysis::LogAnalysisPanel;
use crate::log_view::LogView;
use crate::theme;
use crate::ui::dock::{
    logd_dock_area, register_logd_panels, AnalysisPanel, FilterPanel, LogPanel, SearchResultsPanel,
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

const DOCK_LAYOUT_VERSION: usize = 3;
const DEVELOPER: &str = "barry chen";
const GITHUB_REPOSITORY: &str = "https://github.com/965962591/logd";
const CONTACT_EMAIL: &str = "barrymchen@gmail.com";

struct UpdateDialog {
    language: Language,
    sender: Sender<crate::updater::ManualUpdateEvent>,
    receiver: Receiver<crate::updater::ManualUpdateEvent>,
    status: UpdateStatus,
}

enum UpdateStatus {
    Idle,
    Checking,
    Available(crate::updater::ManualRelease),
    Downloading {
        version: String,
        downloaded: u64,
        total: u64,
    },
    UpToDate,
    Error(String),
    Restarting,
}

impl UpdateDialog {
    fn new(language: Language) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        Self {
            language,
            sender,
            receiver,
            status: UpdateStatus::Idle,
        }
    }

    fn check(&mut self) {
        if matches!(
            self.status,
            UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Restarting
        ) {
            return;
        }
        self.status = UpdateStatus::Checking;
        crate::updater::check_manual_update(self.sender.clone());
    }

    fn download(&mut self) {
        let UpdateStatus::Available(release) = &self.status else {
            return;
        };
        let release = release.clone();
        self.status = UpdateStatus::Downloading {
            version: release.version.clone(),
            downloaded: 0,
            total: release.size,
        };
        crate::updater::download_and_restart(release, self.sender.clone());
    }

    fn poll(&mut self) -> bool {
        let mut restarting = false;
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                crate::updater::ManualUpdateEvent::Available(release) => {
                    self.status = UpdateStatus::Available(release)
                }
                crate::updater::ManualUpdateEvent::UpToDate => self.status = UpdateStatus::UpToDate,
                crate::updater::ManualUpdateEvent::Progress { downloaded, total } => {
                    let version = match &self.status {
                        UpdateStatus::Downloading { version, .. } => version.clone(),
                        _ => String::new(),
                    };
                    self.status = UpdateStatus::Downloading {
                        version,
                        downloaded,
                        total,
                    };
                }
                crate::updater::ManualUpdateEvent::Restarting => {
                    self.status = UpdateStatus::Restarting;
                    restarting = true;
                }
                crate::updater::ManualUpdateEvent::Error(error) => {
                    self.status = UpdateStatus::Error(error)
                }
            }
        }
        restarting
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = self.language;
        let check = cx.entity().clone();
        let download = cx.entity().clone();
        let close = cx.entity().clone();
        let (status_text, progress, can_download) = match &self.status {
            UpdateStatus::Idle => (text(Key::UpdateIdle, language).to_string(), None, false),
            UpdateStatus::Checking => (
                text(Key::CheckingForUpdates, language).to_string(),
                None,
                false,
            ),
            UpdateStatus::Available(release) => (
                format!(
                    "{}: {}",
                    text(Key::UpdateAvailable, language),
                    release.version
                ),
                None,
                true,
            ),
            UpdateStatus::Downloading {
                version,
                downloaded,
                total,
            } => {
                let percent = if *total == 0 {
                    0.0
                } else {
                    (*downloaded as f32 / *total as f32) * 100.0
                };
                (
                    format!(
                        "{} {} ({:.0}%, {} / {})",
                        text(Key::DownloadingUpdate, language),
                        version,
                        percent,
                        format_download_size(*downloaded),
                        format_download_size(*total),
                    ),
                    Some(percent),
                    false,
                )
            }
            UpdateStatus::UpToDate => (
                text(Key::AlreadyUpToDate, language).to_string(),
                None,
                false,
            ),
            UpdateStatus::Error(error) => (
                format!("{}: {}", text(Key::UpdateFailed, language), error),
                None,
                false,
            ),
            UpdateStatus::Restarting => (text(Key::Restarting, language).to_string(), None, false),
        };
        let mut content = v_flex().gap_2().child(status_text);
        if let Some(value) = progress {
            content = content.child(Progress::new("update-progress").value(value).w_full());
        }
        content.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("check-update")
                        .label(text(Key::CheckForUpdates, language))
                        .disabled(matches!(
                            self.status,
                            UpdateStatus::Checking
                                | UpdateStatus::Downloading { .. }
                                | UpdateStatus::Restarting
                        ))
                        .on_click(move |_, _, cx| {
                            check.update(cx, |this, cx| {
                                this.check();
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("download-update")
                        .primary()
                        .label(text(Key::InstallUpdate, language))
                        .disabled(!can_download)
                        .on_click(move |_, _, cx| {
                            download.update(cx, |this, cx| {
                                this.download();
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("close-about")
                        .label(text(Key::Close, language))
                        .disabled(matches!(
                            self.status,
                            UpdateStatus::Downloading { .. } | UpdateStatus::Restarting
                        ))
                        .on_click(window.listener_for(
                            &close,
                            |_: &mut UpdateDialog, _, window, cx| {
                                window.close_dialog(cx);
                            },
                        )),
                ),
        )
    }
}

fn format_download_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MIB)
}

#[derive(Clone)]
struct TabDrag {
    index: usize,
    title: String,
}

impl Render for TabDrag {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        div()
            .px_3()
            .h(px(24.))
            .bg(palette.selection)
            .text_color(palette.foreground)
            .child(self.title.clone())
    }
}

#[derive(Clone)]
enum MenuCommand {
    Open,
    OpenRecent(PathBuf),
    ClearRecentFiles,
    Refresh,
    SaveEditedCopy,
    ImportFilters,
    SetEncoding(Encoding),
    SetTheme(gpui_component::ThemeMode),
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
    search_query: String,
    show_only_filtered: bool,
    tat_path: Option<PathBuf>,
    filters_dirty: bool,
    filter_save_prompt_open: bool,
    recent_files: Vec<PathBuf>,
    search_history: Vec<String>,
    keyword: Entity<InputState>,
    search_history_select: Entity<ComboboxState<Vec<String>>>,
    filter_text: Entity<InputState>,
    filter_description: Entity<InputState>,
    filter_fore: Entity<ColorPickerState>,
    filter_back: Entity<ColorPickerState>,
    filter_bold: bool,
    filter_font_size: Option<u16>,
    filter_scope: FilterScope,
    selected_filter: Option<usize>,
    editing_filter: Option<usize>,
    filter_editor_open: bool,
    dock_area: Entity<DockArea>,
    tab_scroll: ScrollHandle,
    filter_panel: Entity<FilterPanel>,
    search_results_panel: Entity<SearchResultsPanel>,
    analysis_panel: Entity<LogAnalysisPanel>,
    analysis_dock_panel: Entity<AnalysisPanel>,
    last_layout_state: Option<DockAreaState>,
    save_layout_task: Option<Task<()>>,
    language: Language,
    status: Option<String>,
    focus: FocusHandle,
}

impl LogdApp {
    pub fn new(initial: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = Language::from_env();
        let search_history = crate::settings::load_search_history();
        let keyword = cx.new(|cx| {
            InputState::new(window, cx).placeholder(text(Key::SearchPlaceholder, language))
        });
        let search_history_select =
            cx.new(|cx| ComboboxState::new(search_history.clone(), Vec::new(), window, cx));
        let filter_text = cx.new(|cx| {
            InputState::new(window, cx).placeholder(text(Key::FilterTextPlaceholder, language))
        });
        let filter_description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(text(Key::FilterDescriptionPlaceholder, language))
        });
        let filter_fore = cx.new(|cx| ColorPickerState::new(window, cx));
        let filter_back = cx.new(|cx| ColorPickerState::new(window, cx));

        let search_history_for_keyword = search_history_select.clone();
        cx.subscribe_in(
            &keyword,
            window,
            move |this, state, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let value = state.read(cx).value().to_string();
                    let selected = search_history_for_keyword.read(cx).selected_value();
                    if selected
                        .as_deref()
                        .is_some_and(|selected| selected != value)
                    {
                        search_history_for_keyword.update(cx, |state, cx| {
                            state.clear_selection(cx);
                        });
                    }
                }
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.set_global_search(state.read(cx).value().to_string(), window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &search_history_select,
            window,
            |this, _, ev: &ComboboxEvent<Vec<String>>, window, cx| {
                if let ComboboxEvent::Change(values) = ev {
                    if let Some(value) = values.first() {
                        let value = value.clone();
                        this.keyword
                            .update(cx, |state, cx| state.set_value(value.clone(), window, cx));
                        this.set_global_search(value, window, cx);
                    }
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
        let search_results_panel = cx.new(|cx| SearchResultsPanel::new(app.clone(), cx));
        let analysis_panel = cx.new(|_| LogAnalysisPanel::new());
        let analysis_dock_panel =
            cx.new(|cx| AnalysisPanel::new(app.clone(), analysis_panel.clone(), cx));
        register_logd_panels(
            &log_panel,
            &filter_panel,
            &search_results_panel,
            &analysis_dock_panel,
            cx,
        );

        let legacy_filter_placement = crate::settings::load_filter_placement();
        let (dock_area, skin) =
            logd_dock_area("logd.main", Some(DOCK_LAYOUT_VERSION), app, window, cx);
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
                &analysis_dock_panel,
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
        let close_app = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            close_app
                .update(cx, |app, cx| app.should_close_window(window, cx))
                .unwrap_or(true)
        });

        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            filters: Vec::new(),
            search_filters: Vec::new(),
            search_query: String::new(),
            show_only_filtered: false,
            tat_path: None,
            filters_dirty: false,
            filter_save_prompt_open: false,
            recent_files: crate::settings::load_recent_files(),
            search_history,
            keyword,
            search_history_select,
            filter_text,
            filter_description,
            filter_fore,
            filter_back,
            filter_bold: false,
            filter_font_size: None,
            filter_scope: FilterScope::default(),
            selected_filter: None,
            editing_filter: None,
            filter_editor_open: false,
            dock_area,
            tab_scroll: ScrollHandle::new(),
            filter_panel,
            search_results_panel,
            analysis_panel,
            analysis_dock_panel,
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
        analysis: &Entity<AnalysisPanel>,
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
            let analysis = DockLayout::tabs().panel_view(panel_handle(analysis.clone()), cx);

            // An outer dock protects its last visible panel from being
            // dragged away. One split tree keeps these tool panels movable.
            let center = match filter_placement {
                DockPlacement::Left => DockLayout::h_split()
                    .child(
                        DockLayout::v_split()
                            .child(filters, Some(px(360.)))
                            .child(analysis, Some(px(360.))),
                        Some(px(360.)),
                    )
                    .child(
                        DockLayout::v_split()
                            .child(workspace, None)
                            .child(search_results, Some(px(240.))),
                        None,
                    ),
                DockPlacement::Bottom => DockLayout::v_split().child(workspace, None).child(
                    DockLayout::h_split().child(search_results, None).child(
                        DockLayout::v_split()
                            .child(filters, Some(px(240.)))
                            .child(analysis, Some(px(240.))),
                        Some(px(360.)),
                    ),
                    Some(px(240.)),
                ),
                DockPlacement::Right | DockPlacement::Center => DockLayout::h_split()
                    .child(
                        DockLayout::v_split()
                            .child(workspace, None)
                            .child(search_results, Some(px(240.))),
                        None,
                    )
                    .child(
                        DockLayout::v_split()
                            .child(filters, Some(px(360.)))
                            .child(analysis, Some(px(360.))),
                        Some(px(360.)),
                    ),
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
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("logd"))
        {
            self.load_logd(path, cx);
        } else {
            self.open_log(path, window, cx);
        }
    }

    fn remember_file(&mut self, path: &Path) {
        self.recent_files.retain(|item| item != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(10);
        let _ = crate::settings::save_recent_files(&self.recent_files);
    }

    fn remember_search(&mut self, query: &str) {
        self.search_history.retain(|item| item != query);
        self.search_history.insert(0, query.to_string());
        self.search_history
            .truncate(crate::settings::MAX_SEARCH_HISTORY);
        let _ = crate::settings::save_search_history(&self.search_history);
    }

    fn clear_search_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_history.clear();
        let _ = crate::settings::save_search_history(&self.search_history);
        self.search_history_select.update(cx, |state, cx| {
            state.set_items(Vec::new(), window, cx);
            state.clear_selection(cx);
        });
        cx.notify();
    }

    fn clear_recent_files(&mut self, cx: &mut Context<Self>) {
        self.recent_files.clear();
        let _ = crate::settings::save_recent_files(&self.recent_files);
        cx.notify();
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
                self.refresh_analysis_if_open(cx);
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
        let configured_filter_count = self.filters.len();
        let only = self.show_only_filtered;
        view.update(cx, |view, cx| {
            view.apply_filters(filters, configured_filter_count, cx);
            view.set_show_only_filtered(only, cx);
        });
    }

    fn apply_search_to_view(&self, view: &Entity<LogView>, cx: &mut App) {
        let query = self.search_query.clone();
        view.update(cx, |view, cx| view.apply_search(query, cx));
    }

    fn has_multi_file_filter_results(&self) -> bool {
        self.filters.iter().any(|filter| {
            filter.is_active() && !filter.excluding && filter.scope == FilterScope::AllFiles
        })
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
                    if path.extension().is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("tat") || ext.eq_ignore_ascii_case("logd")
                    }) {
                        if path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("logd"))
                        {
                            this.load_logd(&path, cx);
                        } else {
                            this.load_tat(&path, cx);
                        }
                    } else {
                        this.status = Some(format!(
                            "{}: .logd / .tat",
                            text(Key::ImportFilters, this.language)
                        ));
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
                self.refresh_analysis_if_open(cx);
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
        self.refresh_analysis_if_open(cx);
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
        view.update(cx, |view, cx| view.reveal_search_result(file_line, cx));
    }

    fn filters_changed(&mut self, cx: &mut Context<Self>) {
        self.filters_dirty = true;
        let refresh_all = self.has_multi_file_filter_results()
            || self
                .tabs
                .iter()
                .any(|tab| tab.view.read(cx).has_multi_file_filter_results());
        for (index, tab) in self.tabs.iter().enumerate() {
            if index == self.active || refresh_all {
                self.apply_filters_to_view(index, &tab.view, cx);
            } else {
                tab.view.update(cx, |view, _| view.mark_dirty());
            }
        }
        self.search_results_panel.update(cx, |panel, cx| {
            panel.reset_scroll();
            cx.notify();
        });
        self.refresh_analysis_if_open(cx);
        cx.notify();
    }

    fn set_global_search(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            self.remember_search(&value);
            let history = self.search_history.clone();
            self.search_history_select.update(cx, |state, cx| {
                state.set_items(history, window, cx);
                state.clear_selection(cx);
            });
        }
        let expression = parse_search_expression(&value);
        self.search_query = expression.query;
        self.search_filters = expression
            .keywords
            .into_iter()
            .map(|keyword| FilterSpec {
                text: keyword,
                mode: HighlightMode::Field,
                fore: Some(theme::search_foreground_rgb(cx)),
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
        let has_search = !self.search_query.is_empty();
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

    pub(crate) fn begin_add_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_editor_open = true;
        self.editing_filter = None;
        self.filter_text
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_description
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_scope = FilterScope::default();
        self.filter_bold = false;
        self.filter_font_size = None;
        // New filters start with the editor's neutral/default colors. Colors
        // are opt-in and can be assigned from the editor when needed.
        self.filter_fore
            .update(cx, |picker, cx| picker.clear_value(window, cx));
        self.filter_back
            .update(cx, |picker, cx| picker.clear_value(window, cx));
        window.focus(&self.filter_text.read(cx).focus_handle(cx), cx);
        self.show_filter_panel(true, window, cx);
    }

    fn begin_add_filter_from_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = self
            .active_view()
            .and_then(|view| view.read(cx).selected_text(cx))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            return;
        };
        self.begin_add_filter(window, cx);
        self.filter_text
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.keyword.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.keyword
            .update(cx, |state, cx| state.select_all(window, cx));
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
        self.filter_bold = filter.bold;
        self.filter_font_size = filter.font_size;
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
                self.filters[index].bold = self.filter_bold;
                self.filters[index].font_size = self.filter_font_size;
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
                    bold: self.filter_bold,
                    font_size: self.filter_font_size,
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

    fn set_show_only(&mut self, only: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_only_filtered == only {
            return;
        }
        self.show_only_filtered = only;
        self.filters_dirty = true;
        if let Some(view) = self.active_view().cloned() {
            let pointer_y = f32::from(window.mouse_position().y);
            view.update(cx, |view, cx| {
                view.set_show_only_filtered_at_pointer(only, pointer_y, cx)
            });
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
        let configured_filter_count = self.filters.len();
        let search_query = self.search_query.clone();
        view.update(cx, |view, cx| {
            view.set_encoding(encoding, filters, configured_filter_count, search_query, cx)
        });
        cx.notify();
    }

    fn set_theme(
        &mut self,
        mode: gpui_component::ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cx.theme().mode == mode {
            return;
        }

        gpui_component::Theme::change(mode, Some(window), cx);
        theme::apply_dark_surface(cx);
        let search_foreground = theme::search_foreground_rgb(cx);
        if !self.search_filters.is_empty() {
            for filter in &mut self.search_filters {
                filter.fore = Some(search_foreground);
            }
            let updates = self
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| (tab.view.clone(), self.filters_for_tab(index)))
                .collect::<Vec<_>>();
            for (view, filters) in updates {
                view.update(cx, |view, cx| view.restyle_filters(filters, cx));
            }
        }

        let _ = crate::settings::save_theme_mode(mode);
        cx.refresh_windows();
        cx.notify();
    }

    fn load_tat(&mut self, path: &Path, cx: &mut Context<Self>) {
        match TatFile::load(path) {
            Ok(tat) => {
                self.filters = tat.filters;
                self.show_only_filtered = tat.show_only_filtered;
                // `.tat` is a compatibility import. Subsequent saves use the
                // native `.logd` format and leave the source TAT untouched.
                self.tat_path = Some(path.with_extension("logd"));
                self.selected_filter = (!self.filters.is_empty()).then_some(0);
                self.status = Some(format!(
                    "{}: {}",
                    text(Key::Filters, self.language),
                    self.filters.len()
                ));
                self.filters_changed(cx);
                self.filters_dirty = false;
            }
            Err(error) => {
                self.status = Some(format!(".tat: {error:#}"));
                cx.notify();
            }
        }
    }

    fn load_logd(&mut self, path: &Path, cx: &mut Context<Self>) {
        match LogdFile::load(path) {
            Ok(file) => {
                self.filters = file.filters;
                self.show_only_filtered = file.show_only_filtered;
                self.tat_path = Some(path.to_path_buf());
                self.selected_filter = (!self.filters.is_empty()).then_some(0);
                self.status = Some(format!(
                    "{}: {}",
                    text(Key::Filters, self.language),
                    self.filters.len()
                ));
                self.filters_changed(cx);
                self.filters_dirty = false;
            }
            Err(error) => {
                self.status = Some(format!(".logd: {error:#}"));
                cx.notify();
            }
        }
    }

    fn save_tat(&mut self, window: &mut Window, close_after_save: bool, cx: &mut Context<Self>) {
        let path = self.tat_path.clone().or_else(|| {
            self.tabs
                .get(self.active)
                .map(|tab| tab.path.with_extension("logd"))
        });
        let Some(path) = path else {
            let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let target = cx.prompt_for_new_path(&directory, Some("filters.logd"));
            cx.spawn_in(window, async move |this, window| {
                let Some(path) = target.await.ok().and_then(Result::ok).flatten() else {
                    return;
                };
                _ = this.update_in(window, |this, window, cx| {
                    if this.save_filters_to(path, cx) && close_after_save {
                        window.remove_window();
                    }
                });
            })
            .detach();
            return;
        };
        if self.save_filters_to(path, cx) && close_after_save {
            window.remove_window();
        }
    }

    fn save_filters_to(&mut self, path: PathBuf, cx: &mut Context<Self>) -> bool {
        let saved = if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tat"))
        {
            TatFile {
                show_only_filtered: self.show_only_filtered,
                filters: self.filters.clone(),
                ..Default::default()
            }
            .save(&path)
        } else {
            LogdFile {
                show_only_filtered: self.show_only_filtered,
                filters: self.filters.clone(),
                ..Default::default()
            }
            .save(&path)
        };
        self.status = Some(match &saved {
            Ok(()) => {
                self.tat_path = Some(path.clone());
                self.filters_dirty = false;
                format!(
                    "{}: {}",
                    text(Key::SaveFilter, self.language),
                    path.display()
                )
            }
            Err(error) => format!("{}: {error:#}", text(Key::SaveFilter, self.language)),
        });
        cx.notify();
        saved.is_ok()
    }

    fn should_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.filters_dirty {
            return true;
        }
        self.prompt_save_filters_before_close(window, cx);
        false
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_close_window(window, cx) {
            window.remove_window();
        }
    }

    fn prompt_save_filters_before_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_save_prompt_open {
            return;
        }
        self.filter_save_prompt_open = true;
        let language = self.language;
        let save_app = cx.entity();
        let discard_app = save_app.clone();
        let cancel_app = save_app.clone();
        let close_app = save_app.clone();
        window.open_alert_dialog(cx, move |alert, window, _| {
            let on_close_app = close_app.clone();
            alert
                .title(text(Key::UnsavedFilters, language))
                .description(text(Key::UnsavedFiltersDetail, language))
                .on_close(move |_, _, cx| {
                    on_close_app.update(cx, |this, _| {
                        this.filter_save_prompt_open = false;
                    });
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("cancel-filter-save")
                                .label(text(Key::Cancel, language))
                                .on_click(window.listener_for(
                                    &cancel_app,
                                    |this: &mut LogdApp, _, window, cx| {
                                        this.filter_save_prompt_open = false;
                                        window.close_dialog(cx);
                                    },
                                )),
                        )
                        .child(
                            Button::new("discard-filter-changes")
                                .label(text(Key::DontSave, language))
                                .on_click(window.listener_for(
                                    &discard_app,
                                    |this: &mut LogdApp, _, window, cx| {
                                        this.filter_save_prompt_open = false;
                                        window.close_dialog(cx);
                                        window.remove_window();
                                    },
                                )),
                        )
                        .child(
                            Button::new("save-filter-changes")
                                .primary()
                                .label(text(Key::SaveFilter, language))
                                .on_click(window.listener_for(
                                    &save_app,
                                    |this: &mut LogdApp, _, window, cx| {
                                        this.filter_save_prompt_open = false;
                                        window.close_dialog(cx);
                                        this.save_tat(window, true, cx);
                                    },
                                )),
                        ),
                )
        });
    }

    fn show_about_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let language = self.language;
        let update = cx.new(|_| UpdateDialog::new(language));
        let poll_update = update.downgrade();
        cx.spawn(async move |_app, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let mut restarting = false;
            if poll_update
                .update(cx, |this, cx| {
                    restarting = this.poll();
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
            if restarting {
                std::process::exit(0);
            }
        })
        .detach();
        window.open_alert_dialog(cx, move |alert, _window, _| {
            alert
                .title("logd")
                .keyboard(false)
                .description(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_3()
                                .child(div().w(px(100.)).child(text(Key::Version, language)))
                                .child(env!("CARGO_PKG_VERSION")),
                        )
                        .child(
                            h_flex()
                                .gap_3()
                                .child(div().w(px(100.)).child(text(Key::Developer, language)))
                                .child(DEVELOPER),
                        )
                        .child(
                            h_flex()
                                .gap_3()
                                .child(
                                    div()
                                        .w(px(100.))
                                        .child(text(Key::GitHubRepository, language)),
                                )
                                .child(
                                    Link::new("about-github")
                                        .href(GITHUB_REPOSITORY)
                                        .child(GITHUB_REPOSITORY),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_3()
                                .child(div().w(px(100.)).child(text(Key::Email, language)))
                                .child(
                                    Link::new("about-email")
                                        .href(format!("mailto:{CONTACT_EMAIL}"))
                                        .child(CONTACT_EMAIL),
                                ),
                        )
                        .child(div().mt_2().w_full().child(update.clone())),
                )
                .footer(
                    DialogFooter::new().child(
                        Button::new("close-about-footer")
                            .primary()
                            .label(text(Key::Close, language))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    pub(crate) fn show_filter_panel(
        &mut self,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter_panel
            .update(cx, |panel, cx| panel.set_visible(show, cx));
        let dock_area = self.dock_area.clone();
        self.schedule_layout_save(&dock_area, window, cx);
        cx.notify();
    }

    pub(crate) fn show_search_results(
        &mut self,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_results_panel
            .update(cx, |panel, cx| panel.set_visible(show, cx));
        let dock_area = self.dock_area.clone();
        self.schedule_layout_save(&dock_area, window, cx);
        cx.notify();
    }

    pub(crate) fn search_results_title_status(&self, cx: &App) -> (String, Option<String>) {
        let has_search = !self.search_query.is_empty();
        let has_filter_results = self.has_multi_file_filter_results();
        let has_results = has_search || has_filter_results;
        let mut total_matches = 0usize;
        let mut matched_files = 0usize;
        let mut scanning = 0usize;

        for tab in &self.tabs {
            let view = tab.view.read(cx);
            let result_scanning = if has_search {
                view.search_scanning_progress().is_some()
            } else {
                has_filter_results && view.scanning_progress().is_some()
            };
            if has_results && (result_scanning || view.indexing_progress().is_some()) {
                scanning += 1;
            }
            let lines = if has_search {
                view.search_matches()
            } else if has_filter_results {
                view.multi_file_filter_matches()
            } else {
                None
            };
            if let Some(lines) = lines.filter(|lines| !lines.is_empty()) {
                total_matches = total_matches.saturating_add(lines.len());
                matched_files += 1;
            }
        }

        let summary = format!(
            "{} {}  |  {} {}",
            group(total_matches as u64),
            text(Key::Matches, self.language),
            group(matched_files as u64),
            text(Key::Files, self.language)
        );
        let progress = (scanning > 0).then(|| {
            format!(
                "{} {}/{}",
                text(Key::SearchInProgress, self.language),
                self.tabs.len().saturating_sub(scanning),
                self.tabs.len(),
            )
        });
        (summary, progress)
    }

    fn dispatch(&mut self, command: MenuCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            MenuCommand::Open => self.prompt_open(window, cx),
            MenuCommand::OpenRecent(path) => self.open_path(&path, window, cx),
            MenuCommand::ClearRecentFiles => self.clear_recent_files(cx),
            MenuCommand::Refresh => self.refresh_active(window, cx),
            MenuCommand::SaveEditedCopy => {
                if let Some(view) = self.active_view().cloned() {
                    view.update(cx, |view, cx| view.save_edited_copy(window, cx));
                }
            }
            MenuCommand::ImportFilters => self.prompt_import_filters(window, cx),
            MenuCommand::SetEncoding(encoding) => self.set_encoding(encoding, cx),
            MenuCommand::SetTheme(mode) => self.set_theme(mode, window, cx),
            MenuCommand::CopySelection => {
                if let Some(view) = self.active_view().cloned() {
                    view.update(cx, |view, cx| view.copy_selection(cx));
                }
            }
            MenuCommand::ShowAll => self.set_show_only(false, window, cx),
            MenuCommand::ShowOnlyFiltered => self.set_show_only(true, window, cx),
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
            MenuCommand::SaveFilters => self.save_tat(window, false, cx),
            MenuCommand::ToggleLanguage => {
                self.language = self.language.toggle();
                cx.notify();
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = &event.keystroke.modifiers;
        if !crate::platform::primary_modifier(modifiers) || modifiers.alt || modifiers.function {
            return;
        }
        let shift = modifiers.shift;
        match (event.keystroke.key.as_str(), shift) {
            ("o", false) => self.dispatch(MenuCommand::Open, window, cx),
            ("r", false) => self.dispatch(MenuCommand::Refresh, window, cx),
            ("s", false) => self.dispatch(MenuCommand::SaveFilters, window, cx),
            ("s", true) => self.dispatch(MenuCommand::SaveEditedCopy, window, cx),
            ("f", false) => self.focus_search(window, cx),
            ("f", true) => self.begin_add_filter_from_selection(window, cx),
            ("n", false) => self.begin_add_filter(window, cx),
            ("e", false) => self.begin_edit_filter(window, cx),
            ("b", false) => self.dispatch(MenuCommand::ToggleFilters, window, cx),
            ("b", true) => self.dispatch(MenuCommand::ToggleSearchResults, window, cx),
            ("l", true) => self.set_show_only(!self.show_only_filtered, window, cx),
            ("tab", false) if !self.tabs.is_empty() => {
                self.set_active((self.active + 1) % self.tabs.len(), cx);
            }
            ("tab", true) if !self.tabs.is_empty() => {
                self.set_active((self.active + self.tabs.len() - 1) % self.tabs.len(), cx);
            }
            ("w", false) => self.close_tab(self.active, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn menu_button(
        &self,
        command_group: Key,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = theme::palette(cx);
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
        let dark_theme = cx.theme().mode.is_dark();
        let id = match command_group {
            Key::File => "menu-file",
            Key::View => "menu-view",
            Key::Encoding => "menu-encoding",
            _ => "menu-filters",
        };
        let button = Button::new(id)
            .xsmall()
            .ghost()
            .text_color(palette.foreground)
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
                    let clear_recent_app = app.clone();
                    menu.submenu(
                        text(Key::RecentFiles, lang),
                        window,
                        cx,
                        move |menu, window, _| {
                            let menu = if submenu_recent.is_empty() {
                                menu.item(
                                    PopupMenuItem::new(text(Key::NoRecentFiles, lang))
                                        .disabled(true),
                                )
                            } else {
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
                            };
                            let has_recent_files = !submenu_recent.is_empty();
                            let clear_recent_app = clear_recent_app.clone();
                            menu.separator().item(
                                PopupMenuItem::new(text(Key::ClearRecentFiles, lang))
                                    .disabled(!has_recent_files)
                                    .on_click(window.listener_for(
                                        &clear_recent_app,
                                        |this, _, window, cx| {
                                            this.dispatch(MenuCommand::ClearRecentFiles, window, cx)
                                        },
                                    )),
                            )
                        },
                    )
                })
                .into_any_element(),
            Key::View => button
                .dropdown_menu(move |menu, window, cx| {
                    let all_app = app.clone();
                    let only_app = app.clone();
                    let panel_app = app.clone();
                    let search_panel_app = app.clone();
                    let language_app = app.clone();
                    let light_theme_app = app.clone();
                    let dark_theme_app = app.clone();
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
                    let menu = menu.submenu(
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
                    );
                    menu.submenu(
                        text(Key::Theme, lang),
                        window,
                        cx,
                        move |menu, window, _| {
                            menu.item(
                                PopupMenuItem::new(text(Key::LightTheme, lang))
                                    .checked(!dark_theme)
                                    .on_click(window.listener_for(
                                        &light_theme_app,
                                        |this, _, window, cx| {
                                            this.dispatch(
                                                MenuCommand::SetTheme(
                                                    gpui_component::ThemeMode::Light,
                                                ),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                            )
                            .item(
                                PopupMenuItem::new(text(Key::DarkTheme, lang))
                                    .checked(dark_theme)
                                    .on_click(window.listener_for(
                                        &dark_theme_app,
                                        |this, _, window, cx| {
                                            this.dispatch(
                                                MenuCommand::SetTheme(
                                                    gpui_component::ThemeMode::Dark,
                                                ),
                                                window,
                                                cx,
                                            )
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

    fn about_button(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let app = cx.entity();
        Button::new("menu-about")
            .xsmall()
            .ghost()
            .text_color(theme::palette(cx).foreground)
            .label(text(Key::About, self.language))
            .on_click(window.listener_for(&app, |this, _, window, cx| {
                cx.stop_propagation();
                this.show_about_dialog(window, cx);
            }))
            .into_any_element()
    }

    fn render_title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = theme::palette(cx);
        let app = cx.entity();
        let filters_open = self.filter_panel.read(cx).visible();
        let search_results_open = self.search_results_panel.read(cx).visible();
        let search_history = self.search_history.clone();
        let has_search_history = !search_history.is_empty();
        let search_history_select = self.search_history_select.clone();
        let keyword = self.keyword.clone();
        let lang = self.language;
        let filter_toggle_app = app.clone();
        let search_results_toggle_app = app.clone();
        let analysis_toggle_app = app.clone();
        let analysis_open = self.analysis_dock_panel.read(cx).visible();
        let left = h_flex()
            .h_full()
            .items_center()
            .gap_1()
            .pl_2()
            .child(
                h_flex()
                    .id("title-app-drag")
                    .h_full()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .window_control_area(WindowControlArea::Drag)
                    .child(title_bar::app_icon()),
            )
            .child(self.menu_button(Key::File, window, cx))
            .child(self.menu_button(Key::View, window, cx))
            .child(self.menu_button(Key::Encoding, window, cx))
            .child(self.menu_button(Key::Filters, window, cx))
            .child(self.about_button(window, cx))
            .into_any_element();
        let right = h_flex()
            .h_full()
            .flex_none()
            .items_center()
            .gap_1()
            .child(title_bar::panel_toggle(
                "title-toggle-filters",
                IconName::PanelLeft,
                IconName::PanelLeftOpen,
                filters_open,
                false,
                text(
                    if filters_open {
                        Key::HideFilters
                    } else {
                        Key::ShowFilters
                    },
                    lang,
                ),
                move |_, window, cx| {
                    filter_toggle_app.update(cx, |app, cx| {
                        app.show_filter_panel(!filters_open, window, cx)
                    });
                },
            ))
            .child(title_bar::panel_toggle(
                "title-toggle-search-results",
                IconName::PanelBottom,
                IconName::PanelBottomOpen,
                search_results_open,
                false,
                text(
                    if search_results_open {
                        Key::HideSearchResults
                    } else {
                        Key::ShowSearchResults
                    },
                    lang,
                ),
                move |_, window, cx| {
                    search_results_toggle_app.update(cx, |app, cx| {
                        app.show_search_results(!search_results_open, window, cx)
                    });
                },
            ))
            .child(title_bar::panel_toggle(
                "title-toggle-analysis",
                IconName::PanelLeft,
                IconName::PanelLeft,
                analysis_open,
                self.tabs.is_empty(),
                "LogDrain 日志分析",
                move |_, window, cx| {
                    analysis_toggle_app.update(cx, |app, cx| {
                        app.show_analysis_panel(!analysis_open, window, cx)
                    });
                },
            ))
            .into_any_element();
        let clear_app = app.clone();
        let search_history_combo = Combobox::new(&search_history_select)
            .small()
            .w_full()
            .h_full()
            .appearance(false)
            .menu_max_h(px(320.))
            .render_trigger(move |_, _, _| {
                h_flex()
                    .w_full()
                    .h_full()
                    .min_w_0()
                    .items_center()
                    .child(
                        div()
                            .id("search-keyword-input")
                            .min_w_0()
                            .flex_1()
                            .h_full()
                            .on_click(|_, _, cx| cx.stop_propagation())
                            // Keep Enter in the text input; otherwise the enclosing
                            // combobox treats it as a request to open history.
                            .on_action(|_: &Enter, _, cx| cx.stop_propagation())
                            .child(Input::new(&keyword).small().appearance(false)),
                    )
                    .child(control_tooltip(
                        "search-history-tooltip",
                        text(Key::SearchHistory, lang),
                        Icon::new(IconName::ChevronDown).xsmall(),
                    ))
            })
            .empty(move |_, _| {
                h_flex()
                    .justify_center()
                    .py_6()
                    .child(text(Key::NoSearchHistory, lang))
            })
            .footer(move |window, _| {
                h_flex().justify_end().child(
                    Button::new("clear-search-history")
                        .small()
                        .ghost()
                        .label(text(Key::ClearSearchHistory, lang))
                        .disabled(!has_search_history)
                        .on_click(window.listener_for(&clear_app, |this, _, window, cx| {
                            this.clear_search_history(window, cx)
                        })),
                )
            });
        let center = h_flex()
            .w_full()
            .h_full()
            .min_w_0()
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .bg(palette.input_background)
            .border_1()
            .border_color(palette.border)
            .rounded(px(8.))
            .text_color(palette.foreground)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .h_full()
                    .child(search_history_combo),
            )
            .into_any_element();
        let close_app = cx.weak_entity();
        title_bar::render(
            left,
            center,
            right,
            window,
            self.language,
            cx,
            move |window, cx| {
                close_app
                    .update(cx, |app, cx| app.request_close(window, cx))
                    .ok();
            },
        )
        .into_any_element()
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = theme::palette(cx);
        let app = cx.entity();
        let lang = self.language;
        self.tab_scroll.scroll_to_item(self.active);
        let tabs = h_flex()
            .id("tab-strip")
            .w_full()
            .h(px(24.))
            .flex_none()
            .bg(palette.tab_bar)
            .border_b_1()
            .border_color(palette.border)
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
                    .relative()
                    .flex_none()
                    .h_full()
                    .px_3()
                    .gap_2()
                    .items_center()
                    .border_r_1()
                    .border_color(palette.border)
                    .bg(if active {
                        palette.tab_active
                    } else {
                        palette.tab
                    })
                    .text_color(if active {
                        palette.tab_active_foreground
                    } else {
                        palette.tab_foreground
                    })
                    .when(active, |tab| {
                        tab.font_weight(FontWeight::SEMIBOLD).child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(2.))
                                .bg(palette.tab_active_indicator),
                        )
                    })
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
            .bg(palette.tab_bar)
            .child(tabs)
            .child(
                Scrollbar::horizontal(&self.tab_scroll)
                    .id("tab-scrollbar")
                    .mode(ScrollbarMode::Always)
                    .viewport_from_layout()
                    .styles(|styles| {
                        styles
                            .track(|track| track.width(px(4.)).bg(palette.scroll_track))
                            .track_hover(|track| track.width(px(4.)).bg(palette.scroll_track))
                            .track_active(|track| track.width(px(4.)).bg(palette.scroll_track))
                            .thumb(|thumb| thumb.width(px(4.)).inset(px(0.)))
                            .thumb_hover(|thumb| thumb.width(px(4.)).inset(px(0.)))
                            .thumb_active(|thumb| thumb.width(px(4.)).inset(px(0.)))
                    }),
            )
            .into_any_element()
    }

    pub fn render_workspace(&self, cx: &App) -> AnyElement {
        let view = self
            .active_view()
            .cloned()
            .map(IntoElement::into_any_element)
            .unwrap_or_else(|| {
                div()
                    .size_full()
                    .bg(theme::palette(cx).background)
                    .into_any_element()
            });
        view
    }

    pub(crate) fn show_analysis_panel(
        &mut self,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.analysis_dock_panel
            .update(cx, |panel, cx| panel.set_visible(show, cx));
        if show {
            self.refresh_analysis_if_open(cx);
        } else {
            self.analysis_panel.update(cx, |panel, cx| panel.clear(cx));
        }
        let dock_area = self.dock_area.clone();
        self.schedule_layout_save(&dock_area, window, cx);
        cx.notify();
    }

    /// Keep an open LogDrain panel synchronized with the active imported log.
    /// Configured filters take priority; title-bar search filters are excluded.
    fn refresh_analysis_if_open(&self, cx: &mut App) {
        if !self.analysis_dock_panel.read(cx).visible() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.analysis_panel.update(cx, |panel, cx| panel.clear(cx));
            return;
        };
        let view = tab.view.read(cx);
        let source = view.doc().source().clone();
        let encoding = view.doc().encoding();
        let filters = self.filters.clone();
        self.analysis_panel
            .update(cx, |panel, cx| panel.analyze(source, filters, encoding, cx));
    }

    pub fn render_filters(
        app: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<FilterPanel>,
    ) -> AnyElement {
        let palette = theme::palette(cx);
        let (
            filters,
            selected,
            editor_open,
            editor_text,
            editor_description,
            filter_fore,
            filter_back,
            filter_scope,
            filter_bold,
            filter_font_size,
            filter_counts,
            filter_counts_pending,
            lang,
        ) = {
            let state = app.read(cx);
            let active_view = state.active_view();
            let filter_counts = active_view.and_then(|view| view.read(cx).filter_match_counts());
            (
                state.filters.clone(),
                state.selected_filter,
                state.filter_editor_open,
                state.filter_text.clone(),
                state.filter_description.clone(),
                state.filter_fore.clone(),
                state.filter_back.clone(),
                state.filter_scope,
                state.filter_bold,
                state.filter_font_size,
                filter_counts.clone(),
                active_view.is_some() && filter_counts.is_none(),
                state.language,
            )
        };

        let cancel_app = app.clone();
        let reset_fore_app = app.clone();
        let reset_back_app = app.clone();
        let bold_app = app.clone();

        v_flex()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .text_size(px(12.))
            .when(editor_open, |column| {
                column.child(
                    v_flex()
                        .p_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(palette.border)
                        .child(Input::new(&editor_text).small())
                        .child(Input::new(&editor_description).small())
                        .child(control_tooltip(
                            "filter-editor-scope-tooltip",
                            format!(
                                "{}: {}",
                                text(Key::FilterScope, lang),
                                filter_scope_label(filter_scope, lang)
                            ),
                            ButtonGroup::new("filter-editor-scope")
                                .compact()
                                .xsmall()
                                .outline()
                                .child(
                                    Button::new("filter-scope")
                                        .icon(IconName::Globe)
                                        .selected(filter_scope == FilterScope::AllFiles)
                                        .when(filter_scope == FilterScope::AllFiles, |button| {
                                            button.primary()
                                        })
                                        .accessibility_label(format!(
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
                        ))
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .items_center()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .child(
                                            ColorPicker::new(&filter_fore)
                                                // The component's default featured colors are
                                                // repeated in its full palette and reuse the same
                                                // accessibility IDs in debug builds.
                                                .featured_colors(Vec::new())
                                                .small()
                                                .label(text(Key::ForegroundColor, lang))
                                                .accessibility_label(text(
                                                    Key::ForegroundColor,
                                                    lang,
                                                )),
                                        )
                                        .child(control_tooltip(
                                            "filter-reset-fore-tooltip",
                                            text(Key::ResetForegroundColor, lang),
                                            ButtonGroup::new("filter-reset-fore-group")
                                                .compact()
                                                .xsmall()
                                                .outline()
                                                .child(
                                                    Button::new("filter-reset-fore")
                                                        .icon(IconName::Undo2)
                                                        .accessibility_label(text(
                                                            Key::ResetForegroundColor,
                                                            lang,
                                                        ))
                                                        .on_click(window.listener_for(
                                                            &reset_fore_app,
                                                            |this, _, window, cx| {
                                                                this.filter_fore.update(
                                                                    cx,
                                                                    |picker, cx| {
                                                                        picker
                                                                            .clear_value(window, cx)
                                                                    },
                                                                );
                                                            },
                                                        )),
                                                ),
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .child(
                                            ColorPicker::new(&filter_back)
                                                .featured_colors(Vec::new())
                                                .small()
                                                .label(text(Key::BackgroundColor, lang))
                                                .accessibility_label(text(
                                                    Key::BackgroundColor,
                                                    lang,
                                                )),
                                        )
                                        .child(control_tooltip(
                                            "filter-reset-back-tooltip",
                                            text(Key::ResetBackgroundColor, lang),
                                            ButtonGroup::new("filter-reset-back-group")
                                                .compact()
                                                .xsmall()
                                                .outline()
                                                .child(
                                                    Button::new("filter-reset-back")
                                                        .icon(IconName::Undo2)
                                                        .accessibility_label(text(
                                                            Key::ResetBackgroundColor,
                                                            lang,
                                                        ))
                                                        .on_click(window.listener_for(
                                                            &reset_back_app,
                                                            |this, _, window, cx| {
                                                                this.filter_back.update(
                                                                    cx,
                                                                    |picker, cx| {
                                                                        picker
                                                                            .clear_value(window, cx)
                                                                    },
                                                                );
                                                            },
                                                        )),
                                                ),
                                        )),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("filter-bold")
                                        .label("B")
                                        .selected(filter_bold)
                                        .when(filter_bold, |button| button.primary())
                                        .accessibility_label(text(Key::FilterBold, lang))
                                        .on_click(window.listener_for(
                                            &bold_app,
                                            |this, _, _, cx| {
                                                this.filter_bold = !this.filter_bold;
                                                cx.notify();
                                            },
                                        )),
                                )
                                .child(
                                    Button::new("filter-font-size")
                                        .label(filter_font_size.map_or_else(
                                            || "A".to_string(),
                                            |size| size.to_string(),
                                        ))
                                        .selected(filter_font_size.is_some())
                                        .when(filter_font_size.is_some(), |button| button.primary())
                                        .accessibility_label(text(Key::FilterFontSize, lang))
                                        .on_click(window.listener_for(app, |this, _, _, cx| {
                                            this.filter_font_size =
                                                next_filter_font_size(this.filter_font_size);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .child(control_tooltip(
                                    "filter-editor-cancel-tooltip",
                                    text(Key::Cancel, lang),
                                    ButtonGroup::new("filter-editor-cancel-group")
                                        .compact()
                                        .xsmall()
                                        .outline()
                                        .child(
                                            Button::new("filter-editor-cancel")
                                                .icon(IconName::Close)
                                                .accessibility_label(text(Key::Cancel, lang))
                                                .on_click(window.listener_for(
                                                    &cancel_app,
                                                    |this, _, _, cx| {
                                                        this.filter_editor_open = false;
                                                        this.editing_filter = None;
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                ))
                                .child(control_tooltip(
                                    "filter-editor-save-tooltip",
                                    text(Key::SaveFilter, lang),
                                    ButtonGroup::new("filter-editor-save-group")
                                        .compact()
                                        .xsmall()
                                        .outline()
                                        .child(
                                            Button::new("filter-editor-save")
                                                .icon(IconName::Check)
                                                .accessibility_label(text(Key::SaveFilter, lang))
                                                .on_click(window.listener_for(
                                                    app,
                                                    |this, _, window, cx| {
                                                        this.commit_filter(window, cx)
                                                    },
                                                )),
                                        ),
                                )),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("filter-list")
                    .flex_1()
                    .overflow_y_scrollbar()
                    .children(filters.iter().enumerate().map(|(index, filter)| {
                        render_filter_row(
                            app,
                            filter,
                            index,
                            selected,
                            filter_counts
                                .as_deref()
                                .and_then(|counts| counts.get(index).copied()),
                            filter_counts_pending,
                            lang,
                            palette,
                            window,
                        )
                    }))
                    .when(filters.is_empty(), |list| {
                        list.child(
                            div()
                                .p_3()
                                .text_color(palette.muted)
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
        let palette = theme::palette(cx);
        let (has_results, files, total_matches, tree_rows, scanning, lang) = {
            let state = app.read(cx);
            let has_search = !state.search_query.is_empty();
            let has_filter_results = state.has_multi_file_filter_results();
            let has_results = has_search || has_filter_results;
            let mut files = Vec::new();
            let mut total_matches = 0usize;
            let mut tree_rows = 0usize;
            let mut scanning = 0usize;
            for (tab_index, tab) in state.tabs.iter().enumerate() {
                let view = tab.view.read(cx);
                let result_scanning = if has_search {
                    view.search_scanning_progress().is_some()
                } else {
                    has_filter_results && view.scanning_progress().is_some()
                };
                if has_results && (result_scanning || view.indexing_progress().is_some()) {
                    scanning += 1;
                }
                let lines = if has_search {
                    view.search_matches()
                } else if has_filter_results {
                    view.multi_file_filter_matches()
                } else {
                    None
                };
                let Some(lines) = lines else {
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
                has_results,
                Arc::new(files),
                total_matches,
                tree_rows,
                scanning,
                state.language,
            )
        };

        let panel = v_flex()
            .size_full()
            .min_h_0()
            .bg(palette.background)
            .text_color(palette.foreground)
            .text_size(px(12.));

        if !has_results {
            return panel
                .child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(palette.muted), // .child(text(Key::SearchResultsPrompt, lang)),
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
                        .text_color(palette.muted)
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
                                .bg(palette.gutter)
                                .border_b_1()
                                .border_color(palette.border)
                                .hover(|row| row.bg(palette.control_hover))
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
                                    .text_color(palette.muted),
                                )
                                .child(
                                    Icon::new(IconName::FileText)
                                        .xsmall()
                                        .text_color(palette.muted),
                                )
                                .child(file.full_path.clone())
                                .child(div().text_color(palette.muted).child(count)),
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
                            .border_color(palette.border)
                            .hover(|row| row.bg(palette.control_hover))
                            .on_click(window.listener_for(&target_app, move |this, _, _, cx| {
                                this.goto_search_result(tab_index, file_line, cx)
                            }))
                            .child(
                                div()
                                    .w(px(34.))
                                    .h_full()
                                    .flex_none()
                                    .border_r_1()
                                    .border_color(palette.border),
                            )
                            .child(
                                div()
                                    .w(px(98.))
                                    .flex_none()
                                    .px_2()
                                    .text_right()
                                    .text_color(palette.muted)
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
        let palette = theme::palette(cx);
        let mut bar = h_flex()
            .h(px(22.))
            .w_full()
            .px_2()
            .gap_4()
            .bg(palette.status)
            .text_color(palette.foreground)
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
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let palette = theme::palette(cx);
        let title = self.render_title_bar(window, cx);
        let tabs = self.render_tabs(cx);
        let status = self.render_status(cx);
        v_flex()
            .id("root")
            .key_context("Logd")
            .track_focus(&self.focus)
            .size_full()
            .bg(palette.background)
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
            .children(dialog_layer)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                for path in paths.paths() {
                    this.open_path(path, window, cx);
                }
            }))
    }
}

fn control_tooltip(
    id: impl Into<ElementId>,
    tooltip: impl Into<SharedString>,
    child: impl IntoElement,
) -> Stateful<Div> {
    let tooltip = tooltip.into();
    div()
        .id(id)
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
        })
        .child(child)
}

fn render_filter_row(
    app: &Entity<LogdApp>,
    filter: &FilterSpec,
    index: usize,
    selected: Option<usize>,
    match_count: Option<u64>,
    match_count_pending: bool,
    lang: Language,
    palette: theme::Palette,
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
        .when(selected == Some(index), |row| row.bg(palette.selection))
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
        .child(control_tooltip(
            ("filter-enabled-tooltip", index),
            text(Key::FilterEnabled, lang),
            Checkbox::new(("filter-enabled", index))
                .xsmall()
                .checked(filter.enabled)
                .accessibility_label(text(Key::FilterEnabled, lang))
                .on_click(
                    window.listener_for(&enable_app, move |this, checked, _, cx| {
                        this.filters[index].enabled = *checked;
                        this.filters_changed(cx);
                    }),
                ),
        ))
        .child(
            h_flex()
                .child(control_tooltip(
                    ("filter-exclude-tooltip", index),
                    text(Key::FilterExcluding, lang),
                    ButtonGroup::new(("filter-exclude-group", index))
                        .compact()
                        .xsmall()
                        .outline()
                        .child(
                            Button::new(("filter-exclude", index))
                                .icon(IconName::Minus)
                                .selected(filter.excluding)
                                .when(filter.excluding, |button| button.primary())
                                .accessibility_label(text(Key::FilterExcluding, lang))
                                .on_click(window.listener_for(
                                    &exclude_app,
                                    move |this, _, _, cx| {
                                        this.filters[index].excluding =
                                            !this.filters[index].excluding;
                                        this.filters_changed(cx);
                                    },
                                )),
                        ),
                ))
                .child(control_tooltip(
                    ("filter-mode-tooltip", index),
                    text(Key::FilterHighlightLine, lang),
                    ButtonGroup::new(("filter-mode-group", index))
                        .compact()
                        .xsmall()
                        .outline()
                        .child(
                            Button::new(("filter-mode", index))
                                .icon(IconName::GalleryVerticalEnd)
                                .selected(filter.mode == HighlightMode::Line)
                                .when(filter.mode == HighlightMode::Line, |button| {
                                    button.primary()
                                })
                                .accessibility_label(text(Key::FilterHighlightLine, lang))
                                .on_click(window.listener_for(&mode_app, move |this, _, _, cx| {
                                    this.filters[index].mode = match this.filters[index].mode {
                                        HighlightMode::Field => HighlightMode::Line,
                                        HighlightMode::Line => HighlightMode::Field,
                                    };
                                    this.filters_changed(cx);
                                })),
                        ),
                ))
                .child(control_tooltip(
                    ("filter-regex-tooltip", index),
                    text(Key::FilterRegex, lang),
                    ButtonGroup::new(("filter-regex-group", index))
                        .compact()
                        .xsmall()
                        .outline()
                        .child(
                            Button::new(("filter-regex", index))
                                .icon(IconName::Asterisk)
                                .selected(filter.regex)
                                .when(filter.regex, |button| button.primary())
                                .accessibility_label(text(Key::FilterRegex, lang))
                                .on_click(window.listener_for(
                                    &regex_app,
                                    move |this, _, _, cx| {
                                        this.filters[index].regex = !this.filters[index].regex;
                                        this.filters_changed(cx);
                                    },
                                )),
                        ),
                ))
                .child(control_tooltip(
                    ("filter-case-tooltip", index),
                    text(Key::FilterCaseSensitive, lang),
                    ButtonGroup::new(("filter-case-group", index))
                        .compact()
                        .xsmall()
                        .outline()
                        .child(
                            Button::new(("filter-case", index))
                                .icon(IconName::CaseSensitive)
                                .selected(filter.case_sensitive)
                                .when(filter.case_sensitive, |button| button.primary())
                                .accessibility_label(text(Key::FilterCaseSensitive, lang))
                                .on_click(window.listener_for(&case_app, move |this, _, _, cx| {
                                    this.filters[index].case_sensitive =
                                        !this.filters[index].case_sensitive;
                                    this.filters_changed(cx);
                                })),
                        ),
                ))
                .child(control_tooltip(
                    ("filter-scope-tooltip", index),
                    format!(
                        "{}: {}",
                        text(Key::FilterScope, lang),
                        filter_scope_label(filter.scope, lang)
                    ),
                    ButtonGroup::new(("filter-scope-group", index))
                        .compact()
                        .xsmall()
                        .outline()
                        .child(
                            Button::new(("filter-scope", index))
                                .icon(IconName::Globe)
                                .selected(filter.scope == FilterScope::AllFiles)
                                .when(filter.scope == FilterScope::AllFiles, |button| {
                                    button.primary()
                                })
                                .accessibility_label(format!(
                                    "{}: {}",
                                    text(Key::FilterScope, lang),
                                    filter_scope_label(filter.scope, lang)
                                ))
                                .on_click(window.listener_for(
                                    &scope_app,
                                    move |this, _, _, cx| {
                                        this.filters[index].scope = match this.filters[index].scope
                                        {
                                            FilterScope::AllFiles => FilterScope::CurrentFile,
                                            FilterScope::CurrentFile => FilterScope::AllFiles,
                                        };
                                        this.filters_changed(cx);
                                    },
                                )),
                        ),
                )),
        )
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .min_w_0()
                        .max_w_full()
                        .self_start()
                        .overflow_hidden()
                        .text_ellipsis()
                        .px_1()
                        .when_some(filter.fore, |preview, color| {
                            preview.text_color(theme::c(color))
                        })
                        .when_some(filter.back, |preview, color| preview.bg(theme::c(color)))
                        .when(filter.bold, |preview| preview.font_weight(FontWeight::BOLD))
                        .when(filter.italic, |preview| preview.italic())
                        .child(filter.text.clone()),
                )
                .when(!filter.description.is_empty(), |item| {
                    item.child(
                        div()
                            .text_size(px(10.))
                            .text_color(palette.muted)
                            .child(filter.description.clone()),
                    )
                }),
        )
        .child(control_tooltip(
            ("filter-match-count-tooltip", index),
            text(Key::FilterMatchCount, lang),
            h_flex()
                .flex_none()
                .gap_1()
                .text_size(px(10.))
                .text_color(palette.muted)
                .child(Icon::new(IconName::Search).xsmall())
                .child(match match_count {
                    Some(count) => group(count),
                    None if match_count_pending => "...".to_string(),
                    None => "-".to_string(),
                }),
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

#[derive(Debug, PartialEq, Eq)]
struct SearchExpression {
    keywords: Vec<String>,
    query: String,
}

fn parse_search_expression(value: &str) -> SearchExpression {
    let groups = value
        .split('|')
        .map(|group| {
            group
                .split('&')
                .map(str::trim)
                .filter(|keyword| !keyword.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|group| !group.is_empty())
        .collect::<Vec<_>>();
    let mut keywords = Vec::new();
    let query = groups
        .iter()
        .map(|group| {
            let terms = group
                .iter()
                .map(|keyword| {
                    parse_time_shorthand(keyword).unwrap_or_else(|| {
                        keywords.push(keyword.clone());
                        quote_query_literal(keyword)
                    })
                })
                .collect::<Vec<_>>()
                .join(" and ");
            if group.len() > 1 {
                format!("({terms})")
            } else {
                terms
            }
        })
        .collect::<Vec<_>>()
        .join(" or ");
    SearchExpression { keywords, query }
}

fn parse_time_shorthand(value: &str) -> Option<String> {
    let prefix = value.get(..2)?;
    if !prefix.eq_ignore_ascii_case("t:") {
        return None;
    }
    let parts = value.get(2..)?.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }

    for start_len in [2, 1] {
        if start_len > parts.len() {
            continue;
        }
        let start = parts[..start_len].join(" ");
        if logd_core::query::parse_time_value(&start, None).is_none() {
            continue;
        }
        if start_len == parts.len() {
            return Some(format!("time>={}", quote_query_literal(&start)));
        }

        let end_parts = &parts[start_len..];
        if end_parts.len() > 2 {
            continue;
        }
        let end = end_parts.join(" ");
        if logd_core::query::parse_time_value(&end, None).is_some() {
            return Some(format!(
                "(time>={} and time<={})",
                quote_query_literal(&start),
                quote_query_literal(&end)
            ));
        }
    }
    None
}

fn quote_query_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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

fn next_filter_font_size(size: Option<u16>) -> Option<u16> {
    match size {
        None => Some(12),
        Some(12) => Some(14),
        Some(14) => Some(16),
        Some(16) => Some(18),
        Some(18) => None,
        Some(_) => Some(12),
    }
}

pub fn run(initial: Vec<PathBuf>) {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    app.run(move |cx| {
        cx.set_app_identity("com.github.965962591.logd", "logd");
        gpui_component::init(cx);
        gpui_component::Theme::change(crate::settings::load_theme_mode(), None, cx);
        crate::theme::apply_dark_surface(cx);
        let initial = initial.clone();
        let options = crate::platform::window_options(cx);
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| LogdApp::new(initial, window, cx));
                // Keep the release executable's first client area identical to dev builds.
                // Some Windows packaging/runtime combinations adjust the requested bounds
                // while creating the native window; resize after the platform window exists.
                window.resize(crate::platform::initial_window_size());
                cx.new(|cx| Root::new(view, window, cx).bordered(false))
            })
            .expect("failed to create logd window");
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::{group, parse_search_expression, search_tree_file_row_count, SearchExpression};

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(500_000_000), "500,000,000");
    }

    #[test]
    fn parses_and_or_search_expression() {
        assert_eq!(
            parse_search_expression("  first & second | third||  "),
            SearchExpression {
                keywords: vec!["first".into(), "second".into(), "third".into()],
                query: "(\"first\" and \"second\") or \"third\"".into(),
            }
        );
        assert_eq!(
            parse_search_expression(" | "),
            SearchExpression {
                keywords: Vec::new(),
                query: String::new(),
            }
        );
    }

    #[test]
    fn parses_time_range_search_expression() {
        assert_eq!(
            parse_search_expression("t:05-09 21:41:07:788638188 05-09 21:41:08:001129178"),
            SearchExpression {
                keywords: Vec::new(),
                query: concat!(
                    "(time>=\"05-09 21:41:07:788638188\" and ",
                    "time<=\"05-09 21:41:08:001129178\")"
                )
                .into(),
            }
        );
    }

    #[test]
    fn parses_dot_millisecond_time_range_search_expression() {
        assert_eq!(
            parse_search_expression("t:06-17 04:18:19.809 06-17 04:28:39.571"),
            SearchExpression {
                keywords: Vec::new(),
                query: concat!(
                    "(time>=\"06-17 04:18:19.809\" and ",
                    "time<=\"06-17 04:28:39.571\")"
                )
                .into(),
            }
        );
    }

    #[test]
    fn parses_open_ended_time_search_expression() {
        assert_eq!(
            parse_search_expression("t:05-09 21:41:08:001129178"),
            SearchExpression {
                keywords: Vec::new(),
                query: "time>=\"05-09 21:41:08:001129178\"".into(),
            }
        );
    }

    #[test]
    fn combines_time_and_text_search_terms() {
        assert_eq!(
            parse_search_expression("error & t:05-09 21:41:08:001129178"),
            SearchExpression {
                keywords: vec!["error".into()],
                query: "(\"error\" and time>=\"05-09 21:41:08:001129178\")".into(),
            }
        );
    }

    #[test]
    fn search_expression_quotes_literal_query_syntax() {
        assert_eq!(
            parse_search_expression(r#"say "hi" & C:\logs"#).query,
            r#"("say \"hi\"" and "C:\\logs")"#
        );
    }

    #[test]
    fn collapsed_search_file_keeps_only_its_header_row() {
        assert_eq!(search_tree_file_row_count(1_314, true), 1_315);
        assert_eq!(search_tree_file_row_count(1_314, false), 1);
    }
}
