use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::collections::HashMap;
use std::io::{self, Stdout, Write};
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

use crate::image::{clip_sixel, visible_images, ImageProtocol, InlineImage};

// ---------------------------------------------------------------------------
// ANSI parsing types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ParsedLineSegment {
    text: String,
    range: Range<usize>,
    style: AnsiStyle,
    hyperlink: Option<ParsedHyperlink>,
}

#[derive(Clone, Debug)]
pub struct ParsedLine {
    segments: Vec<ParsedLineSegment>,
    pub plain: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedHyperlink {
    params: Option<String>,
    id: Option<String>,
    url: String,
}

impl ParsedHyperlink {
    fn new(params: Option<String>, id: Option<String>, url: String) -> Self {
        Self { params, id, url }
    }

    fn params_fragment(&self) -> &str {
        self.params.as_deref().unwrap_or("")
    }
}

#[derive(Clone, Debug, Default)]
struct TextAttributes {
    bold: bool,
    dim: bool,
    italic: bool,
    underlined: bool,
    slow_blink: bool,
    rapid_blink: bool,
    reversed: bool,
    hidden: bool,
    crossed_out: bool,
}

impl TextAttributes {
    fn reset(&mut self) {
        *self = TextAttributes::default();
    }

    fn attribute_list(&self) -> impl Iterator<Item = Attribute> {
        let mut attrs = Vec::new();
        if self.bold {
            attrs.push(Attribute::Bold);
        }
        if self.dim {
            attrs.push(Attribute::Dim);
        }
        if self.italic {
            attrs.push(Attribute::Italic);
        }
        if self.underlined {
            attrs.push(Attribute::Underlined);
        }
        if self.slow_blink {
            attrs.push(Attribute::SlowBlink);
        }
        if self.rapid_blink {
            attrs.push(Attribute::RapidBlink);
        }
        if self.reversed {
            attrs.push(Attribute::Reverse);
        }
        if self.hidden {
            attrs.push(Attribute::Hidden);
        }
        if self.crossed_out {
            attrs.push(Attribute::CrossedOut);
        }
        attrs.into_iter()
    }
}

#[derive(Clone, Debug, Default)]
struct AnsiStyleState {
    fg: Option<Color>,
    bg: Option<Color>,
    attributes: TextAttributes,
    hyperlink: Option<ParsedHyperlink>,
}

impl AnsiStyleState {
    fn reset(&mut self) {
        self.fg = None;
        self.bg = None;
        self.attributes.reset();
    }

    fn to_style(&self) -> AnsiStyle {
        AnsiStyle {
            fg: self.fg,
            bg: self.bg,
            attributes: self.attributes.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct AnsiStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    attributes: TextAttributes,
}

impl AnsiStyle {
    fn with_highlight(&self, fg: Color, bg: Color, emphasize: bool) -> Self {
        let mut style = self.clone();
        style.fg = Some(fg);
        style.bg = Some(bg);
        if emphasize {
            style.attributes.bold = true;
        }
        style
    }

    fn with_link_style(&self, focused: bool, hovered: bool) -> Self {
        let mut style = self.clone();
        style.attributes.underlined = true;
        if focused {
            style.fg = Some(Color::White);
            style.bg = Some(Color::Blue);
        } else if hovered {
            style.fg = Some(Color::Blue);
            style.bg = Some(Color::Grey);
        } else {
            style.fg = Some(Color::Blue);
            style.bg = None;
        }
        style
    }

    fn apply(&self, stdout: &mut Stdout) -> io::Result<()> {
        queue!(
            stdout,
            SetAttribute(Attribute::Reset),
            ResetColor,
            SetForegroundColor(self.fg.unwrap_or(Color::Reset)),
            SetBackgroundColor(self.bg.unwrap_or(Color::Reset))
        )?;
        for attr in self.attributes.attribute_list() {
            queue!(stdout, SetAttribute(attr))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RenderChunk {
    text: String,
    style: AnsiStyle,
    hyperlink: Option<ParsedHyperlink>,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SearchMatch {
    line_idx: usize,
    start: usize,
    end: usize,
}

#[derive(Clone)]
enum SearchMode {
    Normal,
    EnteringQuery,
    Active {
        query: String,
        matches: Vec<SearchMatch>,
        current_match: usize,
    },
}

// ---------------------------------------------------------------------------
// Scrollbar / drag
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ScrollbarGeometry {
    knob_start: usize,
    knob_size: usize,
}

#[derive(Clone, Debug)]
struct ScrollbarDrag {
    anchor_within_knob: usize,
}

#[derive(Clone, Debug)]
struct ContentDrag {
    origin_row: usize,
    origin_scroll_offset: usize,
}

#[derive(Clone, Debug)]
enum DragState {
    Scrollbar(ScrollbarDrag),
    Content(ContentDrag),
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct LinkSpan {
    line_idx: usize,
    start_char: usize,
    end_char: usize,
    start_col: usize,
    end_col: usize,
}

#[derive(Clone)]
struct LinkInfo {
    id: Option<String>,
    url: String,
    spans: Vec<LinkSpan>,
}

impl LinkInfo {
    fn primary_span(&self) -> &LinkSpan {
        &self.spans[0]
    }

    fn line_idx(&self) -> usize {
        self.primary_span().line_idx
    }

    fn start_char(&self) -> usize {
        self.primary_span().start_char
    }

    fn spans_on_line(&self, line_idx: usize) -> impl Iterator<Item = &LinkSpan> {
        self.spans
            .iter()
            .filter(move |span| span.line_idx == line_idx)
    }

    fn contains_column(&self, line_idx: usize, column: usize) -> bool {
        self.spans_on_line(line_idx)
            .any(|span| column >= span.start_col && column < span.end_col.max(span.start_col + 1))
    }

    fn intersects_chars(&self, line_idx: usize, start: usize, end: usize) -> bool {
        self.spans_on_line(line_idx)
            .any(|span| span.start_char < end && span.end_char > start)
    }

    fn visible_in_range(&self, start_line: usize, end_line: usize) -> bool {
        self.spans
            .iter()
            .any(|span| span.line_idx >= start_line && span.line_idx < end_line)
    }
}

// ---------------------------------------------------------------------------
// PagerState
// ---------------------------------------------------------------------------

struct PagerState {
    scroll_offset: usize,
    total_lines: usize,
    viewport_height: usize,
    search_mode: SearchMode,
    search_input: String,
    last_terminal_height: usize,
    last_terminal_width: usize,
    links: Vec<LinkInfo>,
    focused_link: Option<usize>,
    hovered_link: Option<usize>,
    drag_state: Option<DragState>,
    filename: Option<String>,
    cell_h: usize,
}

impl PagerState {
    fn new(total_lines: usize, viewport_height: usize) -> Self {
        Self {
            scroll_offset: 0,
            total_lines,
            viewport_height,
            search_mode: SearchMode::Normal,
            search_input: String::new(),
            last_terminal_height: 0,
            last_terminal_width: 0,
            links: Vec::new(),
            focused_link: None,
            hovered_link: None,
            drag_state: None,
            filename: None,
            cell_h: 16,
        }
    }

    fn scrollbar_column(&self) -> Option<usize> {
        if self.last_terminal_width == 0 {
            None
        } else {
            Some(self.last_terminal_width.saturating_sub(1))
        }
    }

    fn scrollbar_geometry(&self) -> Option<ScrollbarGeometry> {
        if self.viewport_height == 0 || self.total_lines <= self.viewport_height {
            return None;
        }
        let mut knob_size = (self.viewport_height * self.viewport_height) / self.total_lines;
        knob_size = knob_size.max(1).min(self.viewport_height);
        let max_scroll = self.total_lines.saturating_sub(self.viewport_height);
        let knob_travel = self.viewport_height.saturating_sub(knob_size);
        let knob_start = if max_scroll == 0 || knob_travel == 0 {
            0
        } else {
            (self.scroll_offset * knob_travel) / max_scroll
        };
        Some(ScrollbarGeometry {
            knob_start,
            knob_size,
        })
    }

    fn scroll_offset_from_knob_start(&self, knob_start: usize, knob_size: usize) -> usize {
        let max_scroll = self.max_scroll();
        if max_scroll == 0 {
            return 0;
        }
        let knob_travel = self.viewport_height.saturating_sub(knob_size);
        if knob_travel == 0 {
            return self.scroll_offset.min(max_scroll);
        }
        let clamped_start = knob_start.min(knob_travel);
        (clamped_start * max_scroll + knob_travel / 2) / knob_travel
    }

    fn begin_scrollbar_drag(&mut self, pointer_row: usize) -> bool {
        self.drag_state = None;
        let Some(geometry) = self.scrollbar_geometry() else {
            return false;
        };
        let knob_start = geometry.knob_start;
        let knob_size = geometry.knob_size;
        let knob_end = knob_start.saturating_add(knob_size);
        let knob_travel = self.viewport_height.saturating_sub(knob_size);

        let mut anchor = if knob_size <= 1 || pointer_row < knob_start {
            0
        } else if pointer_row >= knob_end {
            knob_size.saturating_sub(1)
        } else {
            pointer_row - knob_start
        };
        anchor = anchor.min(knob_size.saturating_sub(1));

        let mut new_scroll = self.scroll_offset;
        if pointer_row < knob_start || pointer_row >= knob_end {
            let desired_anchor = knob_size / 2;
            anchor = desired_anchor.min(knob_size.saturating_sub(1));
            let target_start = pointer_row.saturating_sub(anchor).min(knob_travel);
            new_scroll = self.scroll_offset_from_knob_start(target_start, knob_size);
        }

        self.drag_state = Some(DragState::Scrollbar(ScrollbarDrag {
            anchor_within_knob: anchor,
        }));

        let previous = self.scroll_offset;
        self.scroll_offset = new_scroll.min(self.max_scroll());
        self.clamp_scroll();
        previous != self.scroll_offset
    }

    fn update_scrollbar_drag(&mut self, pointer_row: usize) -> bool {
        let anchor = match self.drag_state {
            Some(DragState::Scrollbar(ref drag)) => drag.anchor_within_knob,
            _ => return false,
        };
        let Some(geometry) = self.scrollbar_geometry() else {
            return false;
        };
        let knob_size = geometry.knob_size;
        let knob_travel = self.viewport_height.saturating_sub(knob_size);
        let adjusted_anchor = anchor.min(knob_size.saturating_sub(1));
        let target_start = pointer_row.saturating_sub(adjusted_anchor).min(knob_travel);
        let new_scroll = self
            .scroll_offset_from_knob_start(target_start, knob_size)
            .min(self.max_scroll());
        if new_scroll != self.scroll_offset {
            self.scroll_offset = new_scroll;
            true
        } else {
            false
        }
    }

    fn begin_content_drag(&mut self, pointer_row: usize) {
        self.drag_state = Some(DragState::Content(ContentDrag {
            origin_row: pointer_row,
            origin_scroll_offset: self.scroll_offset,
        }));
    }

    fn update_content_drag(&mut self, pointer_row: usize) -> bool {
        let drag = match self.drag_state {
            Some(DragState::Content(ref drag)) => drag,
            _ => return false,
        };
        let delta = pointer_row as isize - drag.origin_row as isize;
        let origin = drag.origin_scroll_offset as isize;
        let max_scroll = self.max_scroll() as isize;
        let new_scroll = (origin - delta).clamp(0, max_scroll) as usize;
        if new_scroll != self.scroll_offset {
            self.scroll_offset = new_scroll;
            true
        } else {
            false
        }
    }

    fn end_drag(&mut self) {
        self.drag_state = None;
    }

    fn is_dragging(&self) -> bool {
        self.drag_state.is_some()
    }

    fn update_drag(&mut self, pointer_row: usize) -> bool {
        match self.drag_state {
            Some(DragState::Scrollbar(_)) => self.update_scrollbar_drag(pointer_row),
            Some(DragState::Content(_)) => self.update_content_drag(pointer_row),
            None => false,
        }
    }

    fn dragging_scrollbar(&self) -> bool {
        matches!(self.drag_state, Some(DragState::Scrollbar(_)))
    }

    fn update_viewport_height(&mut self, height: usize) {
        self.viewport_height = height;
        self.clamp_scroll();
    }

    fn max_scroll(&self) -> usize {
        if self.viewport_height == 0 {
            self.total_lines.saturating_sub(1)
        } else {
            self.total_lines.saturating_sub(self.viewport_height)
        }
    }

    fn clamp_scroll(&mut self) {
        let max_scroll = self.max_scroll();
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
    }

    fn scroll_down(&mut self) {
        if self.scroll_offset < self.max_scroll() {
            self.scroll_offset += 1;
        }
    }

    fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    fn page_down(&mut self) {
        let max_scroll = self.max_scroll();
        self.scroll_offset = (self.scroll_offset + self.viewport_height).min(max_scroll);
    }

    fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(self.viewport_height);
    }

    fn half_page_down(&mut self) {
        let half = self.viewport_height / 2;
        let max_scroll = self.max_scroll();
        self.scroll_offset = (self.scroll_offset + half).min(max_scroll);
    }

    fn half_page_up(&mut self) {
        let half = self.viewport_height / 2;
        self.scroll_offset = self.scroll_offset.saturating_sub(half);
    }

    fn jump_to_start(&mut self) {
        self.scroll_offset = 0;
    }

    fn jump_to_end(&mut self) {
        self.scroll_offset = self.max_scroll();
    }

    fn start_search(&mut self) {
        self.search_mode = SearchMode::EnteringQuery;
        self.search_input.clear();
    }

    fn perform_search(&mut self, content: &[ParsedLine]) {
        if self.search_input.is_empty() {
            self.search_mode = SearchMode::Normal;
            return;
        }
        let query = self.search_input.clone();
        let matches = find_search_matches(&query, content);
        if matches.is_empty() {
            self.search_mode = SearchMode::Normal;
        } else {
            let first_line = matches[0].line_idx;
            self.scroll_offset = first_line.saturating_sub(self.viewport_height / 2);
            self.clamp_scroll();
            self.search_mode = SearchMode::Active {
                query,
                matches,
                current_match: 0,
            };
        }
    }

    #[allow(dead_code)]
    fn rebuild_search_results(&mut self, content: &[ParsedLine], target_line: Option<usize>) {
        let mut reset_to_normal = false;
        if let SearchMode::Active {
            query,
            matches,
            current_match,
            ..
        } = &mut self.search_mode
        {
            let new_matches = find_search_matches(query, content);
            if new_matches.is_empty() {
                reset_to_normal = true;
            } else {
                let desired_index = target_line.and_then(|line| {
                    new_matches
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, m)| m.line_idx.abs_diff(line))
                        .map(|(idx, _)| idx)
                });
                let new_index = desired_index
                    .unwrap_or_else(|| (*current_match).min(new_matches.len().saturating_sub(1)));
                *current_match = new_index;
                *matches = new_matches;
            }
        }
        if reset_to_normal {
            self.search_mode = SearchMode::Normal;
        }
    }

    fn rebuild_links(&mut self, content: &[ParsedLine]) {
        let previous_focus = self
            .focused_link
            .and_then(|idx| self.links.get(idx).cloned());
        let previous_hover = self
            .hovered_link
            .and_then(|idx| self.links.get(idx).cloned());
        self.links = collect_links(content);
        if let Some(prev_link) = previous_focus {
            self.focused_link =
                self.links
                    .iter()
                    .position(|link| match (&link.id, &prev_link.id) {
                        (Some(a), Some(b)) => a == b,
                        _ => {
                            link.url == prev_link.url
                                && link.line_idx() == prev_link.line_idx()
                                && link.start_char() == prev_link.start_char()
                        }
                    });
        } else {
            self.focused_link = None;
        }
        if let Some(prev_link) = previous_hover {
            self.hovered_link =
                self.links
                    .iter()
                    .position(|link| match (&link.id, &prev_link.id) {
                        (Some(a), Some(b)) => a == b,
                        _ => {
                            link.url == prev_link.url
                                && link.line_idx() == prev_link.line_idx()
                                && link.start_char() == prev_link.start_char()
                        }
                    });
        } else {
            self.hovered_link = None;
        }
        if let Some(idx) = self.focused_link {
            if idx >= self.links.len() {
                self.focused_link = None;
            }
        }
        if let Some(idx) = self.hovered_link {
            if idx >= self.links.len() {
                self.hovered_link = None;
            }
        }
    }

    fn focused_link_visible(&self) -> bool {
        self.focused_link
            .and_then(|idx| self.links.get(idx))
            .map(|link| {
                if self.viewport_height == 0 {
                    false
                } else {
                    let start = self.scroll_offset;
                    let end = start.saturating_add(self.viewport_height);
                    link.visible_in_range(start, end)
                }
            })
            .unwrap_or(false)
    }

    fn first_visible_link(&self) -> Option<usize> {
        if self.viewport_height == 0 {
            return None;
        }
        let start = self.scroll_offset;
        let end = start.saturating_add(self.viewport_height);
        self.links
            .iter()
            .enumerate()
            .find(|(_, link)| link.visible_in_range(start, end))
            .map(|(idx, _)| idx)
    }

    fn last_visible_link(&self) -> Option<usize> {
        if self.viewport_height == 0 {
            return None;
        }
        let start = self.scroll_offset;
        let end = start.saturating_add(self.viewport_height);
        self.links
            .iter()
            .enumerate()
            .rev()
            .find(|(_, link)| link.visible_in_range(start, end))
            .map(|(idx, _)| idx)
    }

    fn focus_next_link(&mut self) -> bool {
        if !self.focused_link_visible() {
            if let Some(idx) = self.first_visible_link() {
                let changed = self.focused_link != Some(idx);
                self.focused_link = Some(idx);
                self.ensure_link_visible(idx);
                return changed;
            }
        }
        if self.links.is_empty() {
            return false;
        }
        let next_index = match self.focused_link {
            Some(current) => (current + 1) % self.links.len(),
            None => 0,
        };
        let changed = self.focused_link != Some(next_index);
        self.focused_link = Some(next_index);
        self.ensure_link_visible(next_index);
        changed
    }

    fn focus_prev_link(&mut self) -> bool {
        if !self.focused_link_visible() {
            if let Some(idx) = self.last_visible_link() {
                let changed = self.focused_link != Some(idx);
                self.focused_link = Some(idx);
                self.ensure_link_visible(idx);
                return changed;
            }
        }
        if self.links.is_empty() {
            return false;
        }
        let prev_index = match self.focused_link {
            Some(current) => {
                if current == 0 {
                    self.links.len() - 1
                } else {
                    current - 1
                }
            }
            None => self.links.len() - 1,
        };
        let changed = self.focused_link != Some(prev_index);
        self.focused_link = Some(prev_index);
        self.ensure_link_visible(prev_index);
        changed
    }

    fn ensure_link_visible(&mut self, index: usize) {
        if let Some(link) = self.links.get(index) {
            let line_idx = link.line_idx();
            if line_idx < self.scroll_offset {
                self.scroll_offset = line_idx;
            } else if self.viewport_height > 0
                && line_idx >= self.scroll_offset + self.viewport_height
            {
                let desired = line_idx.saturating_sub(self.viewport_height.saturating_sub(1));
                self.scroll_offset = desired;
            }
            self.clamp_scroll();
        }
    }

    fn focus_link_at(&mut self, line_idx: usize, column: usize) -> Option<usize> {
        if let Some((idx, _)) = self
            .links
            .iter()
            .enumerate()
            .find(|(_, link)| link.contains_column(line_idx, column))
        {
            let changed = self.focused_link != Some(idx);
            self.focused_link = Some(idx);
            if changed {
                self.ensure_link_visible(idx);
            }
            Some(idx)
        } else {
            None
        }
    }

    fn focused_link(&self) -> Option<&LinkInfo> {
        self.focused_link.and_then(|idx| self.links.get(idx))
    }

    fn current_link_target(&self) -> Option<&str> {
        self.focused_link().map(|link| link.url.as_str())
    }

    fn hovered_link(&self) -> Option<&LinkInfo> {
        self.hovered_link.and_then(|idx| self.links.get(idx))
    }

    fn hovered_link_target(&self) -> Option<&str> {
        self.hovered_link().map(|link| link.url.as_str())
    }

    fn hover_link_at(&mut self, line_idx: usize, column: usize) -> bool {
        let new_hover = self
            .links
            .iter()
            .position(|link| link.contains_column(line_idx, column));
        if new_hover != self.hovered_link {
            self.hovered_link = new_hover;
            return true;
        }
        false
    }

    fn clear_hover(&mut self) -> bool {
        self.hovered_link.take().is_some()
    }

    fn next_match(&mut self) {
        if let SearchMode::Active {
            matches,
            current_match,
            ..
        } = &mut self.search_mode
        {
            if matches.is_empty() {
                return;
            }
            *current_match = (*current_match + 1) % matches.len();
            let line = matches[*current_match].line_idx;
            self.scroll_offset = line.saturating_sub(self.viewport_height / 2);
            self.clamp_scroll();
        }
    }

    fn prev_match(&mut self) {
        if let SearchMode::Active {
            matches,
            current_match,
            ..
        } = &mut self.search_mode
        {
            if matches.is_empty() {
                return;
            }
            *current_match = if *current_match == 0 {
                matches.len() - 1
            } else {
                *current_match - 1
            };
            let line = matches[*current_match].line_idx;
            self.scroll_offset = line.saturating_sub(self.viewport_height / 2);
            self.clamp_scroll();
        }
    }

    fn clear_search(&mut self) {
        self.search_mode = SearchMode::Normal;
        self.search_input.clear();
    }
}

// ---------------------------------------------------------------------------
// Search helpers
// ---------------------------------------------------------------------------

fn find_search_matches(query: &str, content: &[ParsedLine]) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let query_len = query.len();
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    for (line_idx, line) in content.iter().enumerate() {
        let line_lower = line.plain.to_lowercase();
        let mut start = 0;
        while let Some(pos) = line_lower[start..].find(&query_lower) {
            let match_start = start + pos;
            matches.push(SearchMatch {
                line_idx,
                start: match_start,
                end: match_start + query_len,
            });
            start += pos + 1;
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Link collection
// ---------------------------------------------------------------------------

fn collect_links(content: &[ParsedLine]) -> Vec<LinkInfo> {
    let mut links: Vec<LinkInfo> = Vec::new();
    let mut links_by_id: HashMap<String, usize> = HashMap::new();

    for (line_idx, line) in content.iter().enumerate() {
        let mut current_without_id: Option<usize> = None;
        let mut char_index = 0usize;
        let mut col_index = 0usize;

        for segment in &line.segments {
            for ch in segment.text.chars() {
                let width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if let Some(hyperlink) = &segment.hyperlink {
                    if let Some(idx) = current_without_id.take() {
                        ensure_span_width(&mut links[idx]);
                    }
                    if let Some(id) = &hyperlink.id {
                        let entry = links_by_id.entry(id.clone()).or_insert_with(|| {
                            links.push(LinkInfo {
                                id: Some(id.clone()),
                                url: hyperlink.url.clone(),
                                spans: Vec::new(),
                            });
                            links.len() - 1
                        });
                        add_char_to_link(
                            &mut links[*entry],
                            line_idx,
                            char_index,
                            col_index,
                            width,
                        );
                    } else {
                        let continuation = current_without_id
                            .and_then(|idx| links.get(idx))
                            .map(|link| {
                                link.url == hyperlink.url
                                    && link
                                        .spans
                                        .last()
                                        .map(|span| {
                                            span.line_idx == line_idx && span.end_char == char_index
                                        })
                                        .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        let idx = if continuation {
                            current_without_id.unwrap()
                        } else {
                            if let Some(idx) = current_without_id.take() {
                                ensure_span_width(&mut links[idx]);
                            }
                            links.push(LinkInfo {
                                id: None,
                                url: hyperlink.url.clone(),
                                spans: Vec::new(),
                            });
                            links.len() - 1
                        };
                        add_char_to_link(&mut links[idx], line_idx, char_index, col_index, width);
                        current_without_id = Some(idx);
                    }
                } else if let Some(idx) = current_without_id.take() {
                    ensure_span_width(&mut links[idx]);
                }
                col_index += width;
                char_index += 1;
            }
            if segment.hyperlink.is_none() {
                if let Some(idx) = current_without_id.take() {
                    ensure_span_width(&mut links[idx]);
                }
            }
        }
        if let Some(idx) = current_without_id.take() {
            ensure_span_width(&mut links[idx]);
        }
    }

    links
}

fn add_char_to_link(
    link: &mut LinkInfo,
    line_idx: usize,
    char_index: usize,
    col_index: usize,
    width: usize,
) {
    if let Some(span) = link.spans.last_mut() {
        if span.line_idx == line_idx && span.end_char == char_index {
            span.end_char = char_index + 1;
            if width > 0 {
                span.end_col = col_index + width;
            } else if span.end_col == span.start_col {
                span.end_col = span.start_col + 1;
            }
            return;
        }
    }
    let end_col = if width > 0 {
        col_index + width
    } else {
        col_index + 1
    };
    link.spans.push(LinkSpan {
        line_idx,
        start_char: char_index,
        end_char: char_index + 1,
        start_col: col_index,
        end_col,
    });
}

fn ensure_span_width(link: &mut LinkInfo) {
    if let Some(span) = link.spans.last_mut() {
        if span.end_col == span.start_col {
            span.end_col = span.start_col + 1;
        }
    }
}

// ---------------------------------------------------------------------------
// ANSI parsing
// ---------------------------------------------------------------------------

impl ParsedLine {
    pub fn from_ansi(line: &str) -> Self {
        let mut plain = String::new();
        let mut segments = Vec::new();
        let mut current_text = String::new();
        let mut style_state = AnsiStyleState::default();
        let mut current_style = style_state.to_style();
        let mut segment_start = 0usize;

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\x1b' => {
                    flush_segment(
                        &mut segments,
                        &mut current_text,
                        &current_style,
                        &mut segment_start,
                        plain.len(),
                        style_state.hyperlink.clone(),
                    );
                    i += 1;
                    if i >= bytes.len() {
                        break;
                    }
                    match bytes[i] {
                        b'[' => {
                            i += 1;
                            i += parse_csi_sequence(line, i, &mut style_state);
                            current_style = style_state.to_style();
                        }
                        b']' => {
                            i += 1;
                            i += parse_osc_sequence(line, i, &mut style_state);
                            current_style = style_state.to_style();
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }
                b'\r' | b'\x07' => {
                    i += 1;
                }
                _ => {
                    if current_text.is_empty() {
                        segment_start = plain.len();
                    }
                    let ch = line[i..].chars().next().unwrap();
                    let len = ch.len_utf8();
                    current_text.push(ch);
                    plain.push(ch);
                    i += len;
                }
            }
        }

        flush_segment(
            &mut segments,
            &mut current_text,
            &current_style,
            &mut segment_start,
            plain.len(),
            style_state.hyperlink,
        );

        Self { segments, plain }
    }

    fn to_render_chunks(&self, highlights: &[(usize, usize, bool)]) -> Vec<RenderChunk> {
        let mut chunks = Vec::new();
        let mut highlight_iter = highlights.iter().cloned().peekable();

        for segment in &self.segments {
            let mut cursor = segment.range.start;
            while cursor < segment.range.end {
                let (end, style) =
                    if let Some(&(hl_start, hl_end, is_current)) = highlight_iter.peek() {
                        if hl_end <= cursor {
                            highlight_iter.next();
                            continue;
                        }
                        if hl_start > cursor {
                            (hl_start.min(segment.range.end), segment.style.clone())
                        } else {
                            let end = hl_end.min(segment.range.end);
                            let highlight_style = if is_current {
                                segment
                                    .style
                                    .with_highlight(Color::Black, Color::Yellow, true)
                            } else {
                                segment
                                    .style
                                    .with_highlight(Color::Black, Color::Cyan, false)
                            };
                            if end >= hl_end {
                                highlight_iter.next();
                            }
                            (end, highlight_style)
                        }
                    } else {
                        (segment.range.end, segment.style.clone())
                    };

                if cursor >= end {
                    continue;
                }
                let rel_start = cursor - segment.range.start;
                let rel_end = end - segment.range.start;
                let slice = segment.text[rel_start..rel_end].to_string();
                if slice.is_empty() {
                    cursor = end;
                    continue;
                }
                chunks.push(RenderChunk {
                    text: slice,
                    style: style.clone(),
                    hyperlink: segment.hyperlink.clone(),
                });
                cursor = end;
            }
        }

        chunks
    }
}

fn flush_segment(
    segments: &mut Vec<ParsedLineSegment>,
    current_text: &mut String,
    current_style: &AnsiStyle,
    segment_start: &mut usize,
    plain_len: usize,
    hyperlink: Option<ParsedHyperlink>,
) {
    if current_text.is_empty() {
        return;
    }
    let text = std::mem::take(current_text);
    let start = *segment_start;
    let end = start + text.len();
    segments.push(ParsedLineSegment {
        text,
        range: start..end,
        style: current_style.clone(),
        hyperlink,
    });
    *segment_start = plain_len;
}

fn parse_csi_sequence(line: &str, start: usize, style_state: &mut AnsiStyleState) -> usize {
    let bytes = line.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if (0x40..=0x7e).contains(&b) {
            if b == b'm' {
                apply_sgr(&line[start..i], style_state);
            }
            return i + 1 - start;
        }
        i += 1;
    }
    bytes.len().saturating_sub(start)
}

fn parse_osc_sequence(line: &str, start: usize, style_state: &mut AnsiStyleState) -> usize {
    let bytes = line.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\x07' => {
                apply_osc(&line[start..i], style_state);
                return i + 1 - start;
            }
            b'\x1b' if i + 1 < bytes.len() && bytes[i + 1] == b'\\' => {
                apply_osc(&line[start..i], style_state);
                return i + 2 - start;
            }
            _ => {}
        }
        i += 1;
    }
    apply_osc(&line[start..], style_state);
    bytes.len().saturating_sub(start)
}

fn apply_osc(content: &str, style_state: &mut AnsiStyleState) {
    if let Some(rest) = content.strip_prefix('8') {
        let rest = rest.strip_prefix(';').unwrap_or(rest);
        let mut parts = rest.splitn(2, ';');
        let params = parts.next().unwrap_or("");
        if let Some(url) = parts.next() {
            if url.is_empty() {
                style_state.hyperlink = None;
            } else {
                let params_string = if params.is_empty() {
                    None
                } else {
                    Some(params.to_string())
                };
                let id = params
                    .split(':')
                    .find_map(|part| part.strip_prefix("id="))
                    .map(|value| value.to_string());
                style_state.hyperlink =
                    Some(ParsedHyperlink::new(params_string, id, url.to_string()));
            }
        }
    }
}

fn apply_sgr(params: &str, style_state: &mut AnsiStyleState) {
    let mut numbers: Vec<i64> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .filter_map(|part| part.parse::<i64>().ok())
            .collect()
    };
    if numbers.is_empty() {
        numbers.push(0);
    }
    let mut iter = numbers.into_iter();
    while let Some(code) = iter.next() {
        match code {
            0 => style_state.reset(),
            1 | 21 => style_state.attributes.bold = true,
            2 => style_state.attributes.dim = true,
            3 => style_state.attributes.italic = true,
            4 => style_state.attributes.underlined = true,
            5 => style_state.attributes.slow_blink = true,
            6 => style_state.attributes.rapid_blink = true,
            7 => style_state.attributes.reversed = true,
            8 => style_state.attributes.hidden = true,
            9 => style_state.attributes.crossed_out = true,
            22 => {
                style_state.attributes.bold = false;
                style_state.attributes.dim = false;
            }
            23 => style_state.attributes.italic = false,
            24 => style_state.attributes.underlined = false,
            25 => {
                style_state.attributes.slow_blink = false;
                style_state.attributes.rapid_blink = false;
            }
            27 => style_state.attributes.reversed = false,
            28 => style_state.attributes.hidden = false,
            29 => style_state.attributes.crossed_out = false,
            30..=37 => style_state.fg = Some(map_basic_color((code - 30) as u8, false)),
            38 => apply_extended_color(&mut iter, style_state, true),
            39 => style_state.fg = None,
            40..=47 => style_state.bg = Some(map_basic_color((code - 40) as u8, false)),
            48 => apply_extended_color(&mut iter, style_state, false),
            49 => style_state.bg = None,
            90..=97 => style_state.fg = Some(map_basic_color((code - 90) as u8, true)),
            100..=107 => style_state.bg = Some(map_basic_color((code - 100) as u8, true)),
            _ => {}
        }
    }
}

fn apply_extended_color(
    iter: &mut impl Iterator<Item = i64>,
    style_state: &mut AnsiStyleState,
    is_fg: bool,
) {
    match iter.next() {
        Some(5) => {
            if let Some(value) = iter.next() {
                let color = Color::AnsiValue(value as u8);
                if is_fg {
                    style_state.fg = Some(color);
                } else {
                    style_state.bg = Some(color);
                }
            }
        }
        Some(2) => {
            let r = iter.next().unwrap_or(0).clamp(0, 255) as u8;
            let g = iter.next().unwrap_or(0).clamp(0, 255) as u8;
            let b = iter.next().unwrap_or(0).clamp(0, 255) as u8;
            let color = Color::Rgb { r, g, b };
            if is_fg {
                style_state.fg = Some(color);
            } else {
                style_state.bg = Some(color);
            }
        }
        _ => {}
    }
}

fn map_basic_color(index: u8, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::DarkRed,
        (2, false) => Color::DarkGreen,
        (3, false) => Color::DarkYellow,
        (4, false) => Color::DarkBlue,
        (5, false) => Color::DarkMagenta,
        (6, false) => Color::DarkCyan,
        (7, false) => Color::Grey,
        (0, true) => Color::DarkGrey,
        (1, true) => Color::Red,
        (2, true) => Color::Green,
        (3, true) => Color::Yellow,
        (4, true) => Color::Blue,
        (5, true) => Color::Magenta,
        (6, true) => Color::Cyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_pager(
    stdout: &mut Stdout,
    content: &[ParsedLine],
    images: &[InlineImage],
    state: &mut PagerState,
) -> io::Result<()> {
    let (terminal_width, terminal_height) = terminal::size()?;
    if terminal_height == 0 {
        return Ok(());
    }

    let terminal_height_usize = terminal_height as usize;
    let previous_height = state.last_terminal_height;
    state.last_terminal_height = terminal_height_usize;
    state.last_terminal_width = terminal_width as usize;
    state.total_lines = content.len();
    state.update_viewport_height(terminal_height_usize.saturating_sub(1));
    let content_width = terminal_width.saturating_sub(1) as usize;

    // Build highlight map for search
    let mut highlight_map: HashMap<usize, Vec<(usize, usize, bool)>> = HashMap::new();
    if let SearchMode::Active {
        matches,
        current_match,
        ..
    } = &state.search_mode
    {
        for (idx, search_match) in matches.iter().enumerate() {
            if search_match.line_idx >= state.scroll_offset
                && search_match.line_idx < state.scroll_offset + state.viewport_height
            {
                highlight_map
                    .entry(search_match.line_idx)
                    .or_default()
                    .push((search_match.start, search_match.end, idx == *current_match));
            }
        }
        for ranges in highlight_map.values_mut() {
            ranges.sort_by_key(|(start, _, _)| *start);
        }
    }

    // Render text lines
    for row in 0..state.viewport_height {
        let line_idx = state.scroll_offset + row;
        queue!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
        if let Some(line) = content.get(line_idx) {
            let highlights = highlight_map.get(&line_idx).cloned().unwrap_or_default();
            let link_context = LinkRenderContext {
                focused: state.focused_link(),
                hovered: state.hovered_link(),
            };
            render_line(stdout, line, line_idx, &highlights, content_width, link_context)?;
        }
    }

    // Draw scrollbar
    if state.total_lines > state.viewport_height && state.viewport_height > 0 {
        draw_scrollbar(
            stdout,
            state.scroll_offset,
            state.total_lines,
            state.viewport_height,
            terminal_width.saturating_sub(1),
        )?;
    }

    // Draw status line
    let status_row = state.viewport_height as u16;
    draw_status_line(stdout, state, terminal_width, status_row)?;

    // Clear leftover rows from previous taller terminal
    if previous_height > terminal_height_usize {
        for row in terminal_height_usize..previous_height {
            queue!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
        }
    }

    // Flush text before drawing images
    stdout.flush()?;

    // Render visible images
    render_images(stdout, images, state.scroll_offset, state.viewport_height, state.cell_h)?;

    stdout.flush()
}

/// Render images that are visible in the current viewport.
fn render_images(
    stdout: &mut Stdout,
    images: &[InlineImage],
    scroll_offset: usize,
    viewport_height: usize,
    cell_h: usize,
) -> io::Result<()> {
    let vis = visible_images(images, scroll_offset, viewport_height);
    for img in vis {
        // How many terminal rows are clipped from the top?
        let skip_top = scroll_offset.saturating_sub(img.line_idx);
        // Where does the image start in the viewport?
        let viewport_row = if img.line_idx >= scroll_offset {
            img.line_idx - scroll_offset
        } else {
            0
        };
        // How many rows remain between viewport_row and the status line?
        let available = viewport_height.saturating_sub(viewport_row);
        if available == 0 {
            continue;
        }

        let needs_clip = skip_top > 0 || img.height_rows > available;

        match img.protocol {
            ImageProtocol::Sixel => {
                if needs_clip {
                    if let Some(clipped) = clip_sixel(img, skip_top, available, cell_h) {
                        queue!(stdout, MoveTo(img.col as u16, viewport_row as u16))?;
                        stdout.flush()?;
                        stdout.write_all(&clipped)?;
                    }
                } else {
                    queue!(stdout, MoveTo(img.col as u16, viewport_row as u16))?;
                    stdout.flush()?;
                    stdout.write_all(&img.data)?;
                }
            }
            ImageProtocol::Kitty => {
                if !needs_clip {
                    queue!(stdout, MoveTo(img.col as u16, viewport_row as u16))?;
                    stdout.flush()?;
                    stdout.write_all(&img.data)?;
                }
                // Kitty clipping would require re-encoding; skip partial images.
            }
        }
        queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    }

    Ok(())
}

#[derive(Copy, Clone)]
struct LinkRenderContext<'a> {
    focused: Option<&'a LinkInfo>,
    hovered: Option<&'a LinkInfo>,
}

fn render_line(
    stdout: &mut Stdout,
    line: &ParsedLine,
    line_idx: usize,
    highlights: &[(usize, usize, bool)],
    width: usize,
    link_context: LinkRenderContext<'_>,
) -> io::Result<()> {
    if width == 0 {
        return Ok(());
    }
    let chunks = line.to_render_chunks(highlights);
    let mut remaining = width;
    let mut char_cursor = 0usize;

    for chunk in chunks {
        if remaining == 0 {
            break;
        }
        let (render_text, used_width, complete) = clip_to_width(chunk.text.as_str(), remaining);
        if render_text.is_empty() && used_width == 0 && !complete {
            break;
        }
        let render_char_count = render_text.chars().count();
        let chunk_start_char = char_cursor;
        let chunk_end_char = char_cursor + render_char_count;

        let mut style = chunk.style.clone();
        let hyperlink_info = chunk.hyperlink.as_ref();
        if hyperlink_info.is_some() {
            let is_focused = link_context
                .focused
                .map(|link| link.intersects_chars(line_idx, chunk_start_char, chunk_end_char))
                .unwrap_or(false);
            let is_hovered = link_context
                .hovered
                .map(|link| link.intersects_chars(line_idx, chunk_start_char, chunk_end_char))
                .unwrap_or(false);
            style = style.with_link_style(is_focused, is_hovered && !is_focused);
        }

        style.apply(stdout)?;
        if let Some(hyperlink) = hyperlink_info {
            queue!(
                stdout,
                Print(format!(
                    "\x1b]8;{};{}\x07",
                    hyperlink.params_fragment(),
                    hyperlink.url
                ))
            )?;
            queue!(stdout, Print(render_text.as_str()))?;
            queue!(stdout, Print("\x1b]8;;\x07"))?;
        } else {
            queue!(stdout, Print(render_text.as_str()))?;
        }

        char_cursor = chunk_end_char;
        if used_width >= remaining || !complete {
            break;
        }
        remaining = remaining.saturating_sub(used_width);
    }

    queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    Ok(())
}

fn clip_to_width(text: &str, max_width: usize) -> (String, usize, bool) {
    if max_width == 0 {
        return (String::new(), 0, false);
    }
    if text.is_empty() {
        return (String::new(), 0, true);
    }
    let mut width = 0usize;
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        end = idx + ch.len_utf8();
    }
    if end == 0 {
        return (String::new(), 0, false);
    }
    let complete = end == text.len();
    (text[..end].to_string(), width, complete)
}

fn draw_scrollbar(
    stdout: &mut Stdout,
    scroll_offset: usize,
    total_lines: usize,
    viewport_height: usize,
    column: u16,
) -> io::Result<()> {
    if viewport_height == 0 || total_lines <= viewport_height {
        return Ok(());
    }
    let knob_size = ((viewport_height * viewport_height) / total_lines)
        .max(1)
        .min(viewport_height);
    let max_scroll = total_lines - viewport_height;
    let knob_start = if max_scroll == 0 {
        0
    } else {
        (scroll_offset * (viewport_height - knob_size)) / max_scroll
    };
    let knob_end = knob_start + knob_size;

    for row in 0..viewport_height {
        queue!(stdout, MoveTo(column, row as u16))?;
        if row >= knob_start && row < knob_end {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(" "),
                SetAttribute(Attribute::NoReverse)
            )?;
        } else {
            queue!(stdout, Print(" "))?;
        }
    }

    queue!(
        stdout,
        MoveTo(column, viewport_height as u16),
        Print(" ")
    )?;
    Ok(())
}

fn draw_status_line(
    stdout: &mut Stdout,
    state: &PagerState,
    width: u16,
    row: u16,
) -> io::Result<()> {
    let status_text = match &state.search_mode {
        SearchMode::EnteringQuery => format!("/{}", state.search_input),
        SearchMode::Active {
            query,
            matches,
            current_match,
        } => {
            let position_text = format_position(state);
            format!(
                "{} -- Searching: '{}' ({}/{}) -- n/N: next/prev, Esc: clear",
                position_text,
                query,
                current_match + 1,
                matches.len()
            )
        }
        SearchMode::Normal => {
            let position_text = format_position(state);
            let name = state
                .filename
                .as_deref()
                .unwrap_or("lessi");
            format!(
                "{} {} -- q: quit, j/k/↑/↓: scroll, d/u: half-page, /: search",
                position_text, name,
            )
        }
    };

    let mut display = status_text;
    if let Some(target) = state.hovered_link_target() {
        display.push_str(" -- Link: ");
        display.push_str(target);
    }

    let display = truncate_with_padding(&display, width as usize);

    queue!(
        stdout,
        MoveTo(0, row),
        Clear(ClearType::CurrentLine),
        SetAttribute(Attribute::Reverse),
        Print(display),
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    Ok(())
}

fn format_position(state: &PagerState) -> String {
    if state.total_lines == 0 {
        " (empty)".to_string()
    } else {
        let percentage = if state.max_scroll() == 0 {
            100
        } else {
            (state.scroll_offset * 100) / state.max_scroll()
        };
        format!(
            " {}-{}/{} ({}%)",
            state.scroll_offset + 1,
            (state.scroll_offset + state.viewport_height).min(state.total_lines),
            state.total_lines,
            percentage
        )
    }
}

fn truncate_with_padding(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        result.push(ch);
        used += ch_width;
    }
    if used < width {
        result.push_str(&" ".repeat(width - used));
    }
    result
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

fn handle_key_event(
    key_event: KeyEvent,
    state: &mut PagerState,
    content: &[ParsedLine],
    needs_redraw: &mut bool,
    link_to_open: &mut Option<String>,
) -> bool {
    if matches!(state.search_mode, SearchMode::EnteringQuery) {
        match key_event.code {
            KeyCode::Enter => {
                state.perform_search(content);
                *needs_redraw = true;
                return true;
            }
            KeyCode::Esc => {
                state.search_mode = SearchMode::Normal;
                state.search_input.clear();
                *needs_redraw = true;
                return true;
            }
            KeyCode::Backspace => {
                if state.search_input.pop().is_some() {
                    *needs_redraw = true;
                }
                return true;
            }
            KeyCode::Char(c) => {
                state.search_input.push(c);
                *needs_redraw = true;
                return true;
            }
            _ => return true,
        }
    }

    if key_event.modifiers.contains(KeyModifiers::CONTROL) {
        match key_event.code {
            KeyCode::Char('c') => return false,
            KeyCode::Char('f') => {
                state.page_down();
                *needs_redraw = true;
            }
            KeyCode::Char('b') => {
                state.page_up();
                *needs_redraw = true;
            }
            KeyCode::Char('d') => {
                state.half_page_down();
                *needs_redraw = true;
            }
            KeyCode::Char('u') => {
                state.half_page_up();
                *needs_redraw = true;
            }
            _ => {}
        }
        return true;
    }

    match key_event.code {
        KeyCode::Char('q') => return false,
        KeyCode::Esc => {
            if matches!(state.search_mode, SearchMode::Active { .. }) {
                state.clear_search();
                *needs_redraw = true;
            } else {
                return false;
            }
        }
        KeyCode::Char('/') => {
            state.start_search();
            *needs_redraw = true;
        }
        KeyCode::Char('n') => {
            state.next_match();
            *needs_redraw = true;
        }
        KeyCode::Char('N') => {
            state.prev_match();
            *needs_redraw = true;
        }
        KeyCode::Tab => {
            let changed = if key_event.modifiers.contains(KeyModifiers::SHIFT) {
                state.focus_prev_link()
            } else {
                state.focus_next_link()
            };
            if changed {
                *needs_redraw = true;
            }
        }
        KeyCode::BackTab => {
            if state.focus_prev_link() {
                *needs_redraw = true;
            }
        }
        KeyCode::Enter => {
            if let Some(target) = state.current_link_target() {
                *link_to_open = Some(target.to_string());
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.scroll_down();
            *needs_redraw = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll_up();
            *needs_redraw = true;
        }
        KeyCode::Char('d') => {
            state.half_page_down();
            *needs_redraw = true;
        }
        KeyCode::Char('u') => {
            state.half_page_up();
            *needs_redraw = true;
        }
        KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
            state.page_down();
            *needs_redraw = true;
        }
        KeyCode::PageUp | KeyCode::Char('b') => {
            state.page_up();
            *needs_redraw = true;
        }
        KeyCode::Home | KeyCode::Char('g') => {
            state.jump_to_start();
            *needs_redraw = true;
        }
        KeyCode::End | KeyCode::Char('G') => {
            state.jump_to_end();
            *needs_redraw = true;
        }
        _ => {}
    }

    true
}

fn handle_mouse_event(
    mouse_event: MouseEvent,
    state: &mut PagerState,
    needs_redraw: &mut bool,
    link_to_open: &mut Option<String>,
) {
    let row = mouse_event.row as usize;
    let column = mouse_event.column as usize;

    match mouse_event.kind {
        MouseEventKind::ScrollUp => {
            let previous = state.scroll_offset;
            state.scroll_up();
            let hover_changed = if row < state.viewport_height {
                let line_idx = state.scroll_offset + row;
                state.hover_link_at(line_idx, column)
            } else {
                state.clear_hover()
            };
            if state.scroll_offset != previous || hover_changed {
                *needs_redraw = true;
            }
        }
        MouseEventKind::ScrollDown => {
            let previous = state.scroll_offset;
            state.scroll_down();
            let hover_changed = if row < state.viewport_height {
                let line_idx = state.scroll_offset + row;
                state.hover_link_at(line_idx, column)
            } else {
                state.clear_hover()
            };
            if state.scroll_offset != previous || hover_changed {
                *needs_redraw = true;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            state.end_drag();
            if row < state.viewport_height {
                let mut handled = false;
                if let Some(scrollbar_column) = state.scrollbar_column() {
                    if column == scrollbar_column && state.scrollbar_geometry().is_some() {
                        let scroll_changed = state.begin_scrollbar_drag(row);
                        let hover_cleared = state.clear_hover();
                        if scroll_changed || hover_cleared {
                            *needs_redraw = true;
                        }
                        handled = true;
                    }
                }
                if !handled {
                    let line_idx = state.scroll_offset + row;
                    let hover_changed = state.hover_link_at(line_idx, column);
                    let focus_result = state.focus_link_at(line_idx, column);
                    if hover_changed || focus_result.is_some() {
                        *needs_redraw = true;
                    }
                    if let Some(idx) = focus_result {
                        if let Some(link) = state.links.get(idx) {
                            *link_to_open = Some(link.url.clone());
                        }
                    } else {
                        state.begin_content_drag(row);
                        if state.clear_hover() {
                            *needs_redraw = true;
                        }
                    }
                }
            } else if state.clear_hover() {
                *needs_redraw = true;
            }
        }
        MouseEventKind::Moved => {
            if !state.is_dragging() {
                if row < state.viewport_height {
                    let line_idx = state.scroll_offset + row;
                    if state.hover_link_at(line_idx, column) {
                        *needs_redraw = true;
                    }
                } else if state.clear_hover() {
                    *needs_redraw = true;
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if state.is_dragging() {
                let dragging_scrollbar = state.dragging_scrollbar();
                let scroll_changed = state.update_drag(row);
                let hover_cleared = if dragging_scrollbar {
                    state.clear_hover()
                } else {
                    false
                };
                if scroll_changed || hover_cleared {
                    *needs_redraw = true;
                }
            } else if row < state.viewport_height {
                let line_idx = state.scroll_offset + row;
                if state.hover_link_at(line_idx, column) {
                    *needs_redraw = true;
                }
            } else if state.clear_hover() {
                *needs_redraw = true;
            }
        }
        MouseEventKind::Up(_) => {
            let was_scrollbar_drag = state.dragging_scrollbar();
            let was_dragging = state.is_dragging();
            if was_dragging {
                state.end_drag();
            }
            if was_scrollbar_drag {
                if state.clear_hover() {
                    *needs_redraw = true;
                }
            } else if row < state.viewport_height {
                let line_idx = state.scroll_offset + row;
                if state.hover_link_at(line_idx, column) {
                    *needs_redraw = true;
                }
            } else if state.clear_hover() {
                *needs_redraw = true;
            }
        }
        _ => {}
    };
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_content_to_lines(content: &[String]) -> Vec<ParsedLine> {
    content.iter().map(|s| ParsedLine::from_ansi(s)).collect()
}

/// Open a URL using the platform's default handler.
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Run the interactive pager with the given content and images.
pub fn run_pager(
    content: Vec<ParsedLine>,
    images: Vec<InlineImage>,
    filename: Option<String>,
    cell_h: usize,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    execute!(stdout, EnableMouseCapture)?;
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

    let (_, current_height) = terminal::size()?;
    let viewport_height = current_height.saturating_sub(1) as usize;
    let mut state = PagerState::new(content.len(), viewport_height);
    state.filename = filename;
    state.cell_h = cell_h;
    state.rebuild_links(&content);

    let mut result = Ok(());
    let mut needs_redraw = true;
    let mut pending_link: Option<String> = None;

    loop {
        if needs_redraw {
            if let Err(err) = render_pager(&mut stdout, &content, &images, &mut state) {
                result = Err(err);
                break;
            }
            needs_redraw = false;
        }

        if let Some(target) = pending_link.take() {
            open_url(&target);
            continue;
        }

        match event::read()? {
            Event::Key(key_event) => {
                let mut key_redraw = false;
                if !handle_key_event(
                    key_event,
                    &mut state,
                    &content,
                    &mut key_redraw,
                    &mut pending_link,
                ) {
                    break;
                }
                needs_redraw |= key_redraw;
            }
            Event::Mouse(mouse_event) => {
                handle_mouse_event(
                    mouse_event,
                    &mut state,
                    &mut needs_redraw,
                    &mut pending_link,
                );
            }
            Event::Resize(_new_width, new_height) => {
                let new_viewport_height = new_height.saturating_sub(1) as usize;
                let relative_position = if state.total_lines <= 1 {
                    0.0
                } else {
                    let center_line = state.scroll_offset + state.viewport_height / 2;
                    let denom = (state.total_lines.saturating_sub(1)) as f64;
                    (center_line as f64 / denom).clamp(0.0, 1.0)
                };

                state.viewport_height = new_viewport_height;

                let target_center = if state.total_lines <= 1 {
                    0
                } else {
                    let denom = (state.total_lines.saturating_sub(1)) as f64;
                    (relative_position * denom).round() as usize
                };
                let half_viewport = new_viewport_height / 2;
                let new_max_scroll = if new_viewport_height == 0 {
                    state.total_lines.saturating_sub(1)
                } else {
                    state.total_lines.saturating_sub(new_viewport_height)
                };
                let mut new_scroll_offset = target_center.saturating_sub(half_viewport);
                if new_scroll_offset > new_max_scroll {
                    new_scroll_offset = new_max_scroll;
                }
                state.scroll_offset = new_scroll_offset;
                needs_redraw = true;
            }
            _ => {}
        }
    }

    execute!(stdout, DisableMouseCapture)?;
    execute!(stdout, Show, LeaveAlternateScreen)?;
    disable_raw_mode()?;

    result
}

/// Check if content fits in viewport (no paging needed).
pub fn fits_in_viewport(line_count: usize) -> bool {
    if let Ok((_, height)) = terminal::size() {
        let viewport_height = (height as usize).saturating_sub(1);
        line_count <= viewport_height
    } else {
        false
    }
}
