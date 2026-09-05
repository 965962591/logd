//! 视口与滚动条的位置计算。纯算术，不碰 UI。
//!
//! 为什么要单独抽出来：gpui 的 `Pixels` 是 `f32`。5 亿行 × 18px = 9e9 px，
//! f32 尾数 24 位，超过约 1.67e7 px（≈93 万行）滚动偏移就开始丢精度、抖动、跳行。
//! 所以**滚动位置不能用像素表示**，必须是 `(anchor_line: u64, pixel_offset: f32)`：
//! 行号走 u64 全精度，`pixel_offset` 恒小于一个行高，f32 绰绰有余。
//!
//! 涉及总量的比例运算（滑块位置、拖拽反算）全程走 f64——f64 尾数 53 位，
//! 5 亿行连零头都用不完。
//!
//! 这里所有逻辑都可单测，把 M1 最容易出错的部分和 gpui 隔离开。

/// 跳转时把目标行摆在视口的什么位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTo {
    /// 目标行贴视口顶端。
    Top,
    /// 目标行居中。
    Center,
    /// 已经可见就不动；在上方则贴顶，在下方则贴底。
    Nearest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Viewport {
    /// 视口顶端所在的行号。
    anchor_line: u64,
    /// 顶端那一行被裁掉的像素数，恒满足 `0 <= pixel_offset < line_height`。
    pixel_offset: f32,
    line_height: f32,
    /// 视口高度（像素）。
    height: f32,
    total_lines: u64,
    /// 横向滚动偏移（像素）。
    h_scroll: f32,
    max_h_scroll: f32,
}

impl Viewport {
    pub fn new(line_height: f32) -> Self {
        Self {
            anchor_line: 0,
            pixel_offset: 0.0,
            line_height: line_height.max(1.0),
            height: 0.0,
            total_lines: 0,
            h_scroll: 0.0,
            max_h_scroll: 0.0,
        }
    }

    // ---- 基本属性 ----

    #[inline]
    pub fn anchor_line(&self) -> u64 {
        self.anchor_line
    }

    #[inline]
    pub fn pixel_offset(&self) -> f32 {
        self.pixel_offset
    }

    #[inline]
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    #[inline]
    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    #[inline]
    pub fn h_scroll(&self) -> f32 {
        self.h_scroll
    }

    #[inline]
    pub fn max_h_scroll(&self) -> f32 {
        self.max_h_scroll
    }

    pub fn set_line_height(&mut self, h: f32) {
        let old_height = self.line_height.max(1.0);
        let row_fraction = self.pixel_offset / old_height;
        self.line_height = h.max(1.0);
        self.pixel_offset = (row_fraction * self.line_height)
            .min(self.line_height - 0.001)
            .max(0.0);
        self.clamp();
    }

    /// 视口尺寸变化（窗口 resize）。
    pub fn set_height(&mut self, h: f32) {
        self.height = h.max(0.0);
        self.clamp();
    }

    /// 后台索引推进时总行数会变大，需要重新夹一次。
    pub fn set_total_lines(&mut self, n: u64) {
        self.total_lines = n;
        self.clamp();
    }

    /// 需要渲染的行数。加 1 是因为顶端那行可能被裁了一半，底部还得补一行。
    pub fn visible_rows(&self) -> usize {
        if self.line_height <= 0.0 {
            return 0;
        }
        let rows = ((self.height + self.pixel_offset) / self.line_height).ceil() as usize;
        rows.min(self.total_lines.saturating_sub(self.anchor_line) as usize)
    }

    /// 完整可见的行数（浮点），用于滑块长度和翻页。
    fn viewport_lines(&self) -> f64 {
        if self.line_height <= 0.0 {
            0.0
        } else {
            (self.height / self.line_height) as f64
        }
    }

    /// 滚动位置的上限，单位「行」。到这个位置时最后一行正好贴视口底端。
    fn max_scroll_lines(&self) -> f64 {
        (self.total_lines as f64 - self.viewport_lines()).max(0.0)
    }

    /// 当前滚动位置，单位「行」（含小数）。
    fn scroll_lines(&self) -> f64 {
        self.anchor_line as f64 + (self.pixel_offset / self.line_height) as f64
    }

    /// 最后一个（哪怕只露一点的）可见行号。
    pub fn last_visible_line(&self) -> u64 {
        self.anchor_line
            .saturating_add(self.visible_rows() as u64)
            .saturating_sub(1)
            .min(self.total_lines.saturating_sub(1))
    }

    // ---- 纵向滚动 ----

    /// `dy > 0` 表示内容上移（视图往文件末尾走）。
    pub fn scroll_by_pixels(&mut self, dy: f32) {
        if self.line_height <= 0.0 {
            return;
        }
        let acc = self.pixel_offset + dy;
        if acc >= 0.0 {
            let whole = (acc / self.line_height).floor();
            self.anchor_line = self.anchor_line.saturating_add(whole as u64);
            self.pixel_offset = acc - whole * self.line_height;
        } else {
            let whole = (-acc / self.line_height).ceil();
            let back = whole as u64;
            if self.anchor_line >= back {
                self.anchor_line -= back;
                self.pixel_offset = acc + whole * self.line_height;
            } else {
                self.anchor_line = 0;
                self.pixel_offset = 0.0;
            }
        }
        self.clamp();
    }

    pub fn scroll_by_lines(&mut self, dl: i64) {
        if dl >= 0 {
            self.anchor_line = self.anchor_line.saturating_add(dl as u64);
        } else {
            self.anchor_line = self.anchor_line.saturating_sub((-dl) as u64);
        }
        self.clamp();
    }

    /// 翻页留一行重叠，方便对照上下文。
    pub fn page(&mut self, pages: i32) {
        let step = (self.height - self.line_height).max(self.line_height);
        self.scroll_by_pixels(step * pages as f32);
    }

    pub fn scroll_to_top(&mut self) {
        self.anchor_line = 0;
        self.pixel_offset = 0.0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.set_scroll_lines(self.max_scroll_lines());
    }

    pub fn scroll_to_line(&mut self, line: u64, how: ScrollTo) {
        let line = line.min(self.total_lines.saturating_sub(1));
        match how {
            ScrollTo::Top => {
                self.anchor_line = line;
                self.pixel_offset = 0.0;
            }
            ScrollTo::Center => {
                let half = (self.viewport_lines() / 2.0) as u64;
                self.anchor_line = line.saturating_sub(half);
                self.pixel_offset = 0.0;
            }
            ScrollTo::Nearest => {
                // 底部那行可能只露了一半，用 -1 留出完整可见的余量
                let bottom = self.anchor_line + (self.viewport_lines().floor() as u64);
                if line < self.anchor_line {
                    self.anchor_line = line;
                    self.pixel_offset = 0.0;
                } else if line >= bottom {
                    let back = self.viewport_lines().floor() as u64;
                    self.anchor_line = line.saturating_sub(back.saturating_sub(1));
                    self.pixel_offset = 0.0;
                }
            }
        }
        self.clamp();
    }

    fn set_scroll_lines(&mut self, pos: f64) {
        let pos = pos.clamp(0.0, self.max_scroll_lines());
        let whole = pos.floor();
        self.anchor_line = whole as u64;
        self.pixel_offset = ((pos - whole) * self.line_height as f64) as f32;
    }

    fn clamp(&mut self) {
        if self.pixel_offset < 0.0 {
            self.pixel_offset = 0.0;
        }
        let max = self.max_scroll_lines();
        if self.scroll_lines() > max {
            self.set_scroll_lines(max);
        }
        self.h_scroll = self.h_scroll.clamp(0.0, self.max_h_scroll);
    }

    // ---- 滚动条 ----

    /// 0.0..=1.0。0 表示在文件头，1 表示最后一行贴底。
    pub fn scroll_fraction(&self) -> f32 {
        let max = self.max_scroll_lines();
        if max <= 0.0 {
            return 0.0;
        }
        (self.scroll_lines() / max).clamp(0.0, 1.0) as f32
    }

    /// 返回 `(滑块顶端在轨道上的偏移, 滑块长度)`，单位像素。
    ///
    /// 内容装得下时返回整条轨道，调用方可据此隐藏滚动条。
    pub fn thumb(&self, track: f32, min_thumb: f32) -> (f32, f32) {
        let total = self.total_lines as f64;
        let visible = self.viewport_lines();
        if track <= 0.0 || total <= 0.0 || visible >= total {
            return (0.0, track.max(0.0));
        }
        let len = (((visible / total) as f32) * track).clamp(min_thumb.min(track), track);
        let span = track - len;
        (span * self.scroll_fraction(), len)
    }

    /// 滑块被拖到轨道上的 `offset` 像素处，反算滚动位置。
    pub fn set_thumb_offset(&mut self, offset: f32, track: f32, min_thumb: f32) {
        let (_, len) = self.thumb(track, min_thumb);
        let span = track - len;
        // 比例在 f64 下算，避免和 5 亿行相乘时把 f32 的误差放大
        let frac = if span > 0.0 {
            (offset as f64 / span as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.set_scroll_lines(self.max_scroll_lines() * frac);
    }

    // ---- 横向滚动 ----

    pub fn set_max_h_scroll(&mut self, max: f32) {
        self.max_h_scroll = max.max(0.0);
        self.h_scroll = self.h_scroll.clamp(0.0, self.max_h_scroll);
    }

    pub fn scroll_h_by(&mut self, dx: f32) {
        self.h_scroll = (self.h_scroll + dx).clamp(0.0, self.max_h_scroll);
    }

    /// Return the horizontal scrollbar thumb offset and length.
    pub fn h_thumb(&self, track: f32, viewport_width: f32, min_thumb: f32) -> (f32, f32) {
        let viewport = viewport_width.max(0.0) as f64;
        let total = viewport + self.max_h_scroll as f64;
        if track <= 0.0 || total <= viewport || viewport <= 0.0 {
            return (0.0, track.max(0.0));
        }
        let len = ((viewport / total) as f32 * track).clamp(min_thumb.min(track), track);
        let span = track - len;
        let fraction = if self.max_h_scroll > 0.0 {
            self.h_scroll / self.max_h_scroll
        } else {
            0.0
        };
        (span * fraction.clamp(0.0, 1.0), len)
    }

    /// Set horizontal scrolling from a scrollbar thumb offset.
    pub fn set_h_thumb_offset(
        &mut self,
        offset: f32,
        track: f32,
        viewport_width: f32,
        min_thumb: f32,
    ) {
        let (_, len) = self.h_thumb(track, viewport_width, min_thumb);
        let span = track - len;
        let fraction = if span > 0.0 {
            (offset / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.h_scroll = self.max_h_scroll * fraction;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 亿行、18px 行高、800px 视口 —— 目标场景。
    fn huge() -> Viewport {
        let mut v = Viewport::new(18.0);
        v.set_height(800.0);
        v.set_total_lines(500_000_000);
        v
    }

    #[test]
    fn visible_rows_covers_partial_top_and_bottom() {
        let v = huge();
        // 800/18 = 44.4 → 45 行才能盖满
        assert_eq!(v.visible_rows(), 45);
    }

    #[test]
    fn visible_rows_clamped_by_total() {
        let mut v = Viewport::new(18.0);
        v.set_height(800.0);
        v.set_total_lines(3);
        assert_eq!(v.visible_rows(), 3);
    }

    #[test]
    fn pixel_scroll_accumulates_into_lines() {
        let mut v = huge();
        v.scroll_by_pixels(50.0);
        assert_eq!(v.anchor_line(), 2); // 50/18 = 2 行余 14px
        assert!((v.pixel_offset() - 14.0).abs() < 0.001);
    }

    #[test]
    fn pixel_scroll_up_borrows_from_anchor() {
        let mut v = huge();
        v.scroll_by_pixels(50.0);
        v.scroll_by_pixels(-20.0);
        assert_eq!(v.anchor_line(), 1);
        assert!((v.pixel_offset() - 12.0).abs() < 0.001);
    }

    #[test]
    fn cannot_scroll_above_top() {
        let mut v = huge();
        v.scroll_by_pixels(-99999.0);
        assert_eq!(v.anchor_line(), 0);
        assert_eq!(v.pixel_offset(), 0.0);
    }

    #[test]
    fn bottom_leaves_no_blank_space() {
        let mut v = huge();
        v.scroll_to_bottom();
        // 最后一行贴底：滚动位置 = 总行数 - 视口行数
        let expected = 500_000_000.0_f64 - (800.0 / 18.0) as f64;
        let got = v.anchor_line() as f64 + (v.pixel_offset() / 18.0) as f64;
        assert!((got - expected).abs() < 0.01, "got {got}, want {expected}");
        assert_eq!(v.last_visible_line(), 499_999_999);
    }

    #[test]
    fn scroll_past_bottom_is_clamped() {
        let mut v = huge();
        v.scroll_by_pixels(1e18);
        assert_eq!(v.last_visible_line(), 499_999_999);
        assert!((v.scroll_fraction() - 1.0).abs() < 1e-6);
    }

    /// 核心回归：5 亿行下，滑块拖到哪就该落到哪，不能被 f32 精度毁掉。
    #[test]
    fn thumb_drag_round_trips_at_500m_lines() {
        let mut v = huge();
        let (track, min_thumb) = (760.0_f32, 24.0_f32);
        let max = v.max_scroll_lines();

        for frac in [0.0_f64, 0.001, 0.25, 0.5, 0.75, 0.999, 1.0] {
            let target = max * frac;
            v.set_scroll_lines(target);
            let (offset, _) = v.thumb(track, min_thumb);

            let mut w = huge();
            w.set_thumb_offset(offset, track, min_thumb);

            // 误差只该来自轨道的像素粒度：一像素代表 max/span 行
            let span = (track - v.thumb(track, min_thumb).1) as f64;
            let lines_per_px = max / span;
            let got = w.anchor_line() as f64 + (w.pixel_offset() / 18.0) as f64;
            assert!(
                (got - target).abs() <= lines_per_px * 1.5,
                "frac {frac}: got {got}, want {target}, 容差 {}",
                lines_per_px * 1.5
            );
        }
    }

    #[test]
    fn thumb_has_minimum_length() {
        let v = huge();
        let (_, len) = v.thumb(760.0, 24.0);
        // 45/5e8 的比例算出来几乎是 0，必须被 min_thumb 抬起来
        assert!((len - 24.0).abs() < 0.001, "len = {len}");
    }

    #[test]
    fn thumb_fills_track_when_content_fits() {
        let mut v = Viewport::new(18.0);
        v.set_height(800.0);
        v.set_total_lines(10);
        let (offset, len) = v.thumb(760.0, 24.0);
        assert_eq!(offset, 0.0);
        assert_eq!(len, 760.0);
    }

    #[test]
    fn goto_line_top() {
        let mut v = huge();
        v.scroll_to_line(400_000_000, ScrollTo::Top);
        assert_eq!(v.anchor_line(), 400_000_000);
        assert_eq!(v.pixel_offset(), 0.0);
    }

    #[test]
    fn goto_line_center() {
        let mut v = huge();
        v.scroll_to_line(400_000_000, ScrollTo::Center);
        // 视口 44.4 行，一半是 22
        assert_eq!(v.anchor_line(), 400_000_000 - 22);
    }

    #[test]
    fn goto_line_nearest_is_noop_when_already_visible() {
        let mut v = huge();
        v.scroll_to_line(1000, ScrollTo::Top);
        v.scroll_to_line(1010, ScrollTo::Nearest);
        assert_eq!(v.anchor_line(), 1000, "已经可见就不该动");
    }

    #[test]
    fn goto_line_nearest_scrolls_up() {
        let mut v = huge();
        v.scroll_to_line(1000, ScrollTo::Top);
        v.scroll_to_line(500, ScrollTo::Nearest);
        assert_eq!(v.anchor_line(), 500);
    }

    #[test]
    fn goto_line_nearest_scrolls_down_to_bottom_edge() {
        let mut v = huge();
        v.scroll_to_line(1000, ScrollTo::Top);
        v.scroll_to_line(2000, ScrollTo::Nearest);
        // 2000 该落在视口底端：44 行完整可见 → anchor = 2000 - 43
        assert_eq!(v.anchor_line(), 2000 - 43);
        assert!(v.last_visible_line() >= 2000);
    }

    #[test]
    fn goto_line_clamped_to_total() {
        let mut v = huge();
        v.scroll_to_line(u64::MAX, ScrollTo::Top);
        assert_eq!(v.last_visible_line(), 499_999_999);
    }

    #[test]
    fn growing_total_lines_keeps_position() {
        let mut v = Viewport::new(18.0);
        v.set_height(800.0);
        v.set_total_lines(1_000_000);
        v.scroll_to_line(500_000, ScrollTo::Top);
        // 后台索引推进，总行数变大，位置不该跳
        v.set_total_lines(500_000_000);
        assert_eq!(v.anchor_line(), 500_000);
    }

    #[test]
    fn shrinking_total_lines_pulls_position_back() {
        let mut v = huge();
        v.scroll_to_line(400_000_000, ScrollTo::Top);
        v.set_total_lines(1000);
        assert!(v.last_visible_line() < 1000);
    }

    #[test]
    fn resize_reclamps_at_bottom() {
        let mut v = huge();
        v.scroll_to_bottom();
        v.set_height(1600.0);
        assert_eq!(v.last_visible_line(), 499_999_999);
        assert!((v.scroll_fraction() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn page_down_then_up_returns_close() {
        let mut v = huge();
        v.scroll_to_line(10_000, ScrollTo::Top);
        v.page(1);
        assert!(v.anchor_line() > 10_000);
        v.page(-1);
        assert_eq!(v.anchor_line(), 10_000, "翻页应可逆");
    }

    #[test]
    fn horizontal_scroll_is_clamped() {
        let mut v = huge();
        v.set_max_h_scroll(1200.0);
        v.scroll_h_by(-100.0);
        assert_eq!(v.h_scroll(), 0.0);
        v.scroll_h_by(99999.0);
        assert_eq!(v.h_scroll(), 1200.0);
        // 内容变窄后要收回来
        v.set_max_h_scroll(300.0);
        assert_eq!(v.h_scroll(), 300.0);
    }

    #[test]
    fn changing_line_height_preserves_fractional_row_offset() {
        let mut v = huge();
        v.scroll_by_pixels(9.0);
        v.set_line_height(28.0);
        assert_eq!(v.anchor_line(), 0);
        assert!((v.pixel_offset() - 14.0).abs() < 0.001);
    }

    #[test]
    fn empty_file_is_inert() {
        let mut v = Viewport::new(18.0);
        v.set_height(800.0);
        v.set_total_lines(0);
        assert_eq!(v.visible_rows(), 0);
        assert_eq!(v.scroll_fraction(), 0.0);
        v.scroll_by_pixels(500.0);
        assert_eq!(v.anchor_line(), 0);
        v.scroll_to_bottom();
        assert_eq!(v.anchor_line(), 0);
    }

    /// 单行滚动在 5 亿行的尾部依然精确 —— 用像素表示位置的方案在这里就废了。
    #[test]
    fn single_line_steps_are_exact_near_end() {
        let mut v = huge();
        v.scroll_to_line(499_000_000, ScrollTo::Top);
        for i in 1..=100u64 {
            v.scroll_by_lines(1);
            assert_eq!(v.anchor_line(), 499_000_000 + i);
        }
        for i in (0..100u64).rev() {
            v.scroll_by_lines(-1);
            assert_eq!(v.anchor_line(), 499_000_000 + i);
        }
    }
}
