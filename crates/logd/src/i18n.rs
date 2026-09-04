//! Minimal localisation for the desktop shell.
//!
//! The application deliberately keeps translations in code so the binary has
//! no runtime network or resource lookup requirement. The TOML files in
//! `locales/` mirror this table for translators and packaging tools.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    EnUs,
}

impl Language {
    pub fn from_env() -> Self {
        let value = std::env::var("LOGD_LANG")
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if value.starts_with("en") {
            Self::EnUs
        } else {
            Self::ZhCn
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::ZhCn => Self::EnUs,
            Self::EnUs => Self::ZhCn,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Key {
    File,
    Open,
    Refresh,
    RecentFiles,
    View,
    ShowAll,
    ShowOnlyFiltered,
    ShowFilters,
    Filters,
    AddFilter,
    EditFilter,
    DeleteFilter,
    SaveFilter,
    SaveEditedCopy,
    SearchPlaceholder,
    FilterTextPlaceholder,
    FilterDescriptionPlaceholder,
    ForegroundColor,
    BackgroundColor,
    FilterPanel,
    NoFilters,
    Ready,
    Language,
    Chinese,
    English,
    Encoding,
    Close,
    Minimize,
    Maximize,
    Restore,
    Copy,
    Cancel,
    NoRecentFiles,
    DockLeft,
    DockRight,
    DockBottom,
    Lines,
    Line,
    Matches,
    Indexing,
    Filtering,
    Cache,
}

pub fn text(key: Key, lang: Language) -> &'static str {
    match (key, lang) {
        (Key::File, Language::ZhCn) => "文件",
        (Key::File, Language::EnUs) => "File",
        (Key::Open, Language::ZhCn) => "打开",
        (Key::Open, Language::EnUs) => "Open",
        (Key::Refresh, Language::ZhCn) => "刷新",
        (Key::Refresh, Language::EnUs) => "Refresh",
        (Key::RecentFiles, Language::ZhCn) => "最近文件",
        (Key::RecentFiles, Language::EnUs) => "Recent Files",
        (Key::View, Language::ZhCn) => "视图",
        (Key::View, Language::EnUs) => "View",
        (Key::ShowAll, Language::ZhCn) => "显示全部",
        (Key::ShowAll, Language::EnUs) => "Show All",
        (Key::ShowOnlyFiltered, Language::ZhCn) => "仅显示过滤",
        (Key::ShowOnlyFiltered, Language::EnUs) => "Show Only Filtered",
        (Key::ShowFilters, Language::ZhCn) => "显示过滤器",
        (Key::ShowFilters, Language::EnUs) => "Show Filters",
        (Key::Filters, Language::ZhCn) => "过滤器",
        (Key::Filters, Language::EnUs) => "Filters",
        (Key::AddFilter, Language::ZhCn) => "新增过滤器",
        (Key::AddFilter, Language::EnUs) => "Add Filter",
        (Key::EditFilter, Language::ZhCn) => "编辑",
        (Key::EditFilter, Language::EnUs) => "Edit",
        (Key::DeleteFilter, Language::ZhCn) => "删除",
        (Key::DeleteFilter, Language::EnUs) => "Delete",
        (Key::SaveFilter, Language::ZhCn) => "保存过滤器",
        (Key::SaveFilter, Language::EnUs) => "Save Filters",
        (Key::SaveEditedCopy, Language::ZhCn) => "保存编辑副本",
        (Key::SaveEditedCopy, Language::EnUs) => "Save Edited Copy",
        (Key::SearchPlaceholder, Language::ZhCn) => "搜索关键字（用 | 分隔多个），回车添加",
        (Key::SearchPlaceholder, Language::EnUs) => {
            "Search keywords (use | for multiple), press Enter"
        }
        (Key::FilterTextPlaceholder, Language::ZhCn) => "过滤关键字（必填）",
        (Key::FilterTextPlaceholder, Language::EnUs) => "Filter keyword (required)",
        (Key::FilterDescriptionPlaceholder, Language::ZhCn) => "描述（可选）",
        (Key::FilterDescriptionPlaceholder, Language::EnUs) => "Description (optional)",
        (Key::ForegroundColor, Language::ZhCn) => "字体颜色",
        (Key::ForegroundColor, Language::EnUs) => "Text color",
        (Key::BackgroundColor, Language::ZhCn) => "背景颜色",
        (Key::BackgroundColor, Language::EnUs) => "Background color",
        (Key::FilterPanel, Language::ZhCn) => "过滤器面板",
        (Key::FilterPanel, Language::EnUs) => "Filter Panel",
        (Key::NoFilters, Language::ZhCn) => "还没有过滤器。使用标题栏搜索框添加。",
        (Key::NoFilters, Language::EnUs) => "No filters. Use the title-bar search to add one.",
        (Key::Ready, Language::ZhCn) => "就绪",
        (Key::Ready, Language::EnUs) => "Ready",
        (Key::Language, Language::ZhCn) => "语言",
        (Key::Language, Language::EnUs) => "Language",
        (Key::Chinese, Language::ZhCn) => "简体中文",
        (Key::Chinese, Language::EnUs) => "Chinese",
        (Key::English, Language::ZhCn) => "英文",
        (Key::English, Language::EnUs) => "English",
        (Key::Encoding, Language::ZhCn) => "编码",
        (Key::Encoding, Language::EnUs) => "Encoding",
        (Key::Close, Language::ZhCn) => "关闭",
        (Key::Close, Language::EnUs) => "Close",
        (Key::Minimize, Language::ZhCn) => "最小化",
        (Key::Minimize, Language::EnUs) => "Minimize",
        (Key::Maximize, Language::ZhCn) => "最大化",
        (Key::Maximize, Language::EnUs) => "Maximize",
        (Key::Restore, Language::ZhCn) => "还原",
        (Key::Restore, Language::EnUs) => "Restore",
        (Key::Copy, Language::ZhCn) => "复制",
        (Key::Copy, Language::EnUs) => "Copy",
        (Key::Cancel, Language::ZhCn) => "取消",
        (Key::Cancel, Language::EnUs) => "Cancel",
        (Key::NoRecentFiles, Language::ZhCn) => "没有最近文件",
        (Key::NoRecentFiles, Language::EnUs) => "No recent files",
        (Key::DockLeft, Language::ZhCn) => "停靠左侧",
        (Key::DockLeft, Language::EnUs) => "Dock Left",
        (Key::DockRight, Language::ZhCn) => "停靠右侧",
        (Key::DockRight, Language::EnUs) => "Dock Right",
        (Key::DockBottom, Language::ZhCn) => "停靠底部",
        (Key::DockBottom, Language::EnUs) => "Dock Bottom",
        (Key::Lines, Language::ZhCn) => "行",
        (Key::Lines, Language::EnUs) => "lines",
        (Key::Line, Language::ZhCn) => "第",
        (Key::Line, Language::EnUs) => "Ln",
        (Key::Matches, Language::ZhCn) => "命中",
        (Key::Matches, Language::EnUs) => "matches",
        (Key::Indexing, Language::ZhCn) => "建索引",
        (Key::Indexing, Language::EnUs) => "Index",
        (Key::Filtering, Language::ZhCn) => "筛选",
        (Key::Filtering, Language::EnUs) => "Filter",
        (Key::Cache, Language::ZhCn) => "索引缓存",
        (Key::Cache, Language::EnUs) => "cache",
    }
}
