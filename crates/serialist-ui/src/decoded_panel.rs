//! The Decoded panel: the session's decoded frames as a table, in the right dock above
//! the Script console.
//!
//! # Data flow
//!
//! The panel keeps no frames of its own. On each wake of its doorbell the session view
//! takes a [`FrameSnapshot`] of its frame store (the same wake that brings the terminal
//! its lines, so at most one a frame) and bumps its
//! [`decoded_generation`](SessionView::decoded_generation). The panel observes the
//! session view; when the generation moved it takes the view's snapshots (`Arc` clones)
//! and brings its rows up to date: the frames past the last one it looked at are
//! filtered and appended, and rows whose frames the store evicted are dropped from the
//! front, so a wake costs the new frames, not the whole store. Changing a filter rebuilds
//! the rows once.
//!
//! The table is gpui-kit's [`DataTable`], which is virtualized: it asks
//! [`FrameTable`] for the cells of the rows on screen, which reads them from the
//! snapshot by frame id. The time column stamps frames the way the terminal's gutter
//! stamps lines (the session's timestamp mode and format, absolute while the gutter is
//! off); the raw column reads the frame's bytes from the session's store snapshot by
//! its stream offsets.
//!
//! Selecting a row asks the session view to show the frame in the terminal
//! ([`SessionView::select_frame`]). Like the terminal, the table follows the newest frame
//! until it is scrolled away from the bottom or a row is selected; Follow brings it back.

use std::collections::VecDeque;

use serialist_core::{CodecInfo, Frame, FrameId, FrameSnapshot, Severity, Snapshot, Value};

use crate::codecs::{FrameTime, direction_label};
use crate::prelude::*;
use crate::session_view::SessionView;

/// What the kind filter calls "no filter".
pub const ALL_KINDS: &str = "All kinds";

/// Bytes of a frame the raw column shows.
const RAW_PREVIEW: usize = 24;

/// Characters of one value in the fields column.
const VALUE_PREVIEW: usize = 32;

/// A column of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Col {
    Time,
    Direction,
    Kind,
    Summary,
    /// One declared field of the kind the filter selects.
    Field(String),
    /// Every field, `name=value`, when no kind is selected.
    Fields,
    Raw,
}

impl Col {
    fn column(&self) -> Column {
        let (key, name, width) = match self {
            Col::Time => ("time", "Time", 104.),
            Col::Direction => ("dir", "Dir", 40.),
            Col::Kind => ("kind", "Kind", 88.),
            Col::Summary => ("summary", "Summary", 260.),
            Col::Field(field) => {
                return Column::new(SharedString::from(field.clone()), field.clone())
                    .width(px(96.));
            }
            Col::Fields => ("fields", "Fields", 280.),
            Col::Raw => ("raw", "Raw", 240.),
        };
        Column::new(key, name).width(px(width))
    }
}

/// `value` for a cell: its display form, cut short.
fn preview(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > VALUE_PREVIEW {
        let cut: String = text.chars().take(VALUE_PREVIEW).collect();
        format!("{cut}\u{2026}")
    } else {
        text
    }
}

/// The first bytes of `frame` as hex, from the store; `None` once they were evicted.
fn raw_preview(raw: &Snapshot, frame: &Frame) -> Option<String> {
    let kept = raw.raw_range();
    if frame.raw.start < kept.start || frame.raw.end > kept.end {
        return None;
    }
    let end = frame.raw.end.min(frame.raw.start + RAW_PREVIEW as u64);
    let mut bytes = Vec::with_capacity(RAW_PREVIEW);
    for slice in raw.raw(frame.raw.start..end) {
        bytes.extend_from_slice(slice);
    }
    let mut text = serialist_core::codec::encode_hex(&bytes, " ");
    if frame.raw.end > end {
        text.push_str(" \u{2026}");
    }
    Some(text)
}

/// The table's data: the session's frames and the rows the filters let through.
pub struct FrameTable {
    frames: Option<FrameSnapshot>,
    raw: Option<Snapshot>,
    info: Option<CodecInfo>,
    time: Option<FrameTime>,
    /// Frames shown, in order.
    rows: VecDeque<FrameId>,
    /// Frames below this have been through the filters.
    scanned: FrameId,
    /// The kind filter; `None` for every kind.
    kind: Option<String>,
    /// The text filter, lower case; empty for none.
    query: String,
    columns: Vec<Col>,
}

impl FrameTable {
    fn new() -> Self {
        Self {
            frames: None,
            raw: None,
            info: None,
            time: None,
            rows: VecDeque::new(),
            scanned: FrameId::ZERO,
            kind: None,
            query: String::new(),
            columns: Self::columns_for(None, None),
        }
    }

    fn columns_for(kind: Option<&str>, info: Option<&CodecInfo>) -> Vec<Col> {
        let mut columns = vec![Col::Time, Col::Direction, Col::Kind, Col::Summary];
        match kind {
            Some(kind) => columns.extend(
                info.and_then(|info| info.kind(kind))
                    .into_iter()
                    .flat_map(|kind| kind.fields.iter())
                    .map(|field| Col::Field(field.name.clone())),
            ),
            None => columns.push(Col::Fields),
        }
        columns.push(Col::Raw);
        columns
    }

    /// Whether `frame` passes the filters.
    fn passes(&self, frame: &Frame) -> bool {
        if self.kind.as_deref().is_some_and(|kind| frame.kind != kind) {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let query = self.query.as_str();
        frame.summary.to_lowercase().contains(query)
            || frame.kind.to_lowercase().contains(query)
            || frame.fields.iter().any(|(name, value)| {
                name.to_lowercase().contains(query)
                    || value.to_string().to_lowercase().contains(query)
            })
    }

    /// Filter the frames not looked at yet, and drop the rows of evicted ones. Returns
    /// how many rows were dropped from the front.
    fn catch_up(&mut self) -> usize {
        let Some(frames) = self.frames.clone() else {
            let dropped = self.rows.len();
            self.rows.clear();
            self.scanned = FrameId::ZERO;
            return dropped;
        };
        let first = frames.first();
        let mut dropped = 0;
        while self.rows.front().is_some_and(|id| *id < first) {
            self.rows.pop_front();
            dropped += 1;
        }
        let from = self.scanned.max(first);
        for (id, frame) in frames.iter(from..frames.end()) {
            if self.passes(frame) {
                self.rows.push_back(id);
            }
        }
        self.scanned = frames.end();
        dropped
    }

    /// Filter every retained frame again.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.scanned = FrameId::ZERO;
        self.catch_up();
    }

    /// The frames shown, in order.
    pub fn rows(&self) -> &VecDeque<FrameId> {
        &self.rows
    }

    fn frame(&self, row_ix: usize) -> Option<(FrameId, &Frame)> {
        let id = *self.rows.get(row_ix)?;
        Some((id, self.frames.as_ref()?.get(id)?))
    }

    /// The text of a cell, as drawn.
    fn text(&self, row_ix: usize, col: &Col) -> String {
        let Some((id, frame)) = self.frame(row_ix) else {
            return String::new();
        };
        match col {
            Col::Time => {
                let previous =
                    id.0.checked_sub(1)
                        .and_then(|previous| self.frames.as_ref()?.get(FrameId(previous)))
                        .map(|frame| frame.at);
                self.time
                    .as_ref()
                    .map(|time| time.stamp(frame.at, previous))
                    .unwrap_or_default()
            }
            Col::Direction => direction_label(frame).to_owned(),
            Col::Kind => frame.kind.to_string(),
            Col::Summary => frame.summary.clone(),
            Col::Field(name) => frame.field(name).map(preview).unwrap_or_default(),
            Col::Fields => frame
                .fields
                .iter()
                .map(|(name, value)| format!("{name}={}", preview(value)))
                .collect::<Vec<_>>()
                .join(" "),
            Col::Raw => self
                .raw
                .as_ref()
                .and_then(|raw| raw_preview(raw, frame))
                .unwrap_or_else(|| "(evicted)".to_owned()),
        }
    }
}

impl TableDelegate for FrameTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns
            .get(col_ix)
            .map_or_else(|| Column::new("", ""), Col::column)
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let severity = self.frame(row_ix).map(|(_, frame)| frame.severity);
        let theme = cx.theme();
        div()
            .id(("decoded-row", row_ix))
            .when(severity == Some(Severity::Error), |row| {
                row.text_color(theme.danger)
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(col) = self.columns.get(col_ix).cloned() else {
            return div().into_any_element();
        };
        let text = SharedString::from(self.text(row_ix, &col));
        let severity = self.frame(row_ix).map(|(_, frame)| frame.severity);
        let theme = cx.theme();
        let mono = matches!(col, Col::Raw | Col::Time | Col::Fields | Col::Field(_));
        div()
            .w_full()
            .truncate()
            .when(mono, |cell| {
                cell.font_family(theme.mono_font_family.clone())
            })
            .when(col == Col::Kind, |cell| {
                cell.text_color(match severity {
                    Some(Severity::Error) => theme.danger,
                    Some(Severity::Warning) => theme.warning,
                    _ => theme.info,
                })
            })
            .when(
                col == Col::Summary && severity == Some(Severity::Warning),
                |cell| cell.text_color(theme.warning),
            )
            .when(matches!(col, Col::Raw | Col::Direction), |cell| {
                cell.text_color(theme.muted_foreground)
            })
            .child(text)
            .into_any_element()
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let message = if self
            .frames
            .as_ref()
            .is_some_and(|frames| !frames.is_empty())
        {
            "No frame matches the filters."
        } else {
            "No frames decoded yet."
        };
        div()
            .p_3()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.columns
            .get(col_ix)
            .map(|col| self.text(row_ix, col))
            .unwrap_or_default()
    }
}

pub struct DecodedPanel {
    session: Option<WeakEntity<SessionView>>,
    table: Entity<TableState<FrameTable>>,
    kinds: Entity<SelectState<Vec<String>>>,
    filter: Entity<InputState>,
    /// The codec whose kinds the kind filter lists.
    codec: Option<String>,
    /// Keep the newest frame in view.
    follow: bool,
    /// The session's decoded generation the rows are up to date with.
    seen: Option<u64>,
    focus_handle: FocusHandle,
    _session_observer: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for DecodedPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl DecodedPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(FrameTable::new(), window, cx)
                .row_selectable(true)
                .col_selectable(false)
                .sortable(false)
                .col_movable(false)
        });
        let row_events = cx.subscribe(&table, |this, _, event: &TableEvent, cx| {
            if let TableEvent::SelectRow(row_ix) = event {
                this.row_selected(*row_ix, cx);
            }
        });
        let kinds = cx.new(|cx| {
            SelectState::new(
                vec![ALL_KINDS.to_owned()],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let kind_events = cx.subscribe(&kinds, |this, _, event: &SelectEvent<Vec<String>>, cx| {
            let SelectEvent::Confirm(value) = event;
            let kind = value.clone().filter(|kind| kind != ALL_KINDS);
            this.set_kind(kind, cx);
        });
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter summaries and fields"));
        let filter_events = cx.subscribe(&filter, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let query = input.read(cx).value().to_string();
                this.set_query(&query, cx);
            }
        });
        Self {
            session: None,
            table,
            kinds,
            filter,
            codec: None,
            follow: true,
            seen: None,
            focus_handle: cx.focus_handle(),
            _session_observer: None,
            _subscriptions: vec![row_events, kind_events, filter_events],
        }
    }

    // --- Reading -----------------------------------------------------------------------

    pub fn table(&self) -> &Entity<TableState<FrameTable>> {
        &self.table
    }

    /// The ids of the frames the table shows, in order.
    pub fn rows(&self, cx: &App) -> Vec<FrameId> {
        self.table
            .read(cx)
            .delegate()
            .rows()
            .iter()
            .copied()
            .collect()
    }

    /// The frames the table shows, in order.
    pub fn frames(&self, cx: &App) -> Vec<Frame> {
        let table = self.table.read(cx).delegate();
        (0..table.rows.len())
            .filter_map(|ix| table.frame(ix).map(|(_, frame)| frame.clone()))
            .collect()
    }

    /// The column headers and every row's cells, as drawn.
    pub fn dump(&self, cx: &App) -> (Vec<String>, Vec<Vec<String>>) {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        let headers = delegate
            .columns
            .iter()
            .map(|col| col.column().name.to_string())
            .collect();
        let rows = (0..delegate.rows.len())
            .map(|row| {
                delegate
                    .columns
                    .iter()
                    .map(|col| delegate.text(row, col))
                    .collect()
            })
            .collect();
        (headers, rows)
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// The kind filter; `None` for every kind.
    pub fn kind(&self, cx: &App) -> Option<String> {
        self.table.read(cx).delegate().kind.clone()
    }

    pub fn filter_input(&self) -> &Entity<InputState> {
        &self.filter
    }

    // --- Changing ----------------------------------------------------------------------

    /// Show `session`'s frames, following them as they arrive.
    pub fn set_session(
        &mut self,
        session: Option<Entity<SessionView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_observer = session.as_ref().map(|session| {
            cx.observe_in(session, window, |this, _, window, cx| {
                this.session_changed(window, cx);
            })
        });
        self.session = session.as_ref().map(Entity::downgrade);
        self.seen = None;
        self.follow = true;
        self.session_changed(window, cx);
    }

    /// Bring the rows up to the session's newest frames, if they moved.
    fn session_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.session.as_ref().and_then(WeakEntity::upgrade);
        let Some(view) = view else {
            if self.seen.take().is_some() || self.codec.take().is_some() {
                self.table.update(cx, |table, cx| {
                    let delegate = table.delegate_mut();
                    delegate.frames = None;
                    delegate.raw = None;
                    delegate.info = None;
                    delegate.catch_up();
                    cx.notify();
                });
                cx.notify();
            }
            return;
        };
        let generation = view.read(cx).decoded_generation();
        if self.seen == Some(generation) {
            return;
        }
        self.seen = Some(generation);
        let (frames, raw, codec, time) = view.read_with(cx, |view, cx| {
            (
                view.frames().clone(),
                view.snapshot().clone(),
                view.codec()
                    .map(|codec| (codec.name.clone(), codec.info.clone())),
                view.frame_time(cx),
            )
        });
        let codec_name = codec.as_ref().map(|(name, _)| name.clone());
        let codec_changed = codec_name != self.codec;
        self.codec = codec_name;
        // Following unless scrolled away from the newest row since the last look.
        if let Some(at_end) = self.at_bottom(cx) {
            self.follow = at_end;
        }
        let follow = self.follow;
        let kind_items = codec_changed.then(|| {
            std::iter::once(ALL_KINDS.to_owned())
                .chain(
                    codec
                        .iter()
                        .flat_map(|(_, info)| info.kinds.iter().map(|kind| kind.kind.clone())),
                )
                .collect::<Vec<_>>()
        });
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.frames = Some(frames);
            delegate.raw = Some(raw);
            delegate.time = Some(time);
            if codec_changed {
                delegate.info = codec.map(|(_, info)| info);
                // The kinds of another codec mean nothing to this one.
                delegate.kind = None;
                delegate.columns = FrameTable::columns_for(None, delegate.info.as_ref());
                delegate.rebuild();
                table.refresh(cx);
                table.clear_selection(cx);
            } else if delegate.catch_up() > 0 {
                // Rows went from the front: the selected index names another frame.
                table.clear_selection(cx);
            }
            if follow {
                table.vertical_scroll_handle.scroll_to_bottom();
            }
            cx.notify();
        });
        if let Some(items) = kind_items {
            self.kinds.update(cx, |select, cx| {
                select.set_items(items, window, cx);
                select.set_selected_index(Some(IndexPath::new(0)), window, cx);
            });
        }
        cx.notify();
    }

    /// Whether the table is scrolled to its newest row, when that can be told: not while
    /// a scroll is pending, and not while every row fits.
    fn at_bottom(&self, cx: &App) -> Option<bool> {
        let handle = &self.table.read(cx).vertical_scroll_handle;
        if handle.0.borrow().deferred_scroll_to_item.is_some() {
            return None;
        }
        handle.is_scrolled_to_end()
    }

    /// Show only frames of `kind` (with its fields as columns), or every kind.
    pub fn set_kind(&mut self, kind: Option<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            if delegate.kind == kind {
                return;
            }
            delegate.columns = FrameTable::columns_for(kind.as_deref(), delegate.info.as_ref());
            delegate.kind = kind;
            delegate.rebuild();
            table.refresh(cx);
            table.clear_selection(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Show only frames whose summary, kind or fields contain `query` (any case).
    pub fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = query.trim().to_lowercase();
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            if delegate.query == query {
                return;
            }
            delegate.query = query;
            delegate.rebuild();
            table.clear_selection(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Keep the newest frame in view again.
    pub fn follow(&mut self, cx: &mut Context<Self>) {
        self.follow = true;
        self.table.update(cx, |table, cx| {
            table.vertical_scroll_handle.scroll_to_bottom();
            cx.notify();
        });
        cx.notify();
    }

    /// Select row `row_ix`, as a click does: the terminal shows the frame.
    pub fn select_row(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        self.table
            .update(cx, |table, cx| table.set_selected_row(row_ix, cx));
    }

    fn row_selected(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        let id = self.table.read(cx).delegate().rows.get(row_ix).copied();
        let (Some(id), Some(view)) = (id, self.session.as_ref().and_then(WeakEntity::upgrade))
        else {
            return;
        };
        // Looking at a frame stops following the newest.
        self.follow = false;
        view.update(cx, |view, cx| view.select_frame(id, cx));
        cx.notify();
    }

    // --- Rendering ---------------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let table = self.table.read(cx).delegate();
        let total = table.frames.as_ref().map_or(0, FrameSnapshot::count);
        let count = if table.rows.len() == total {
            format!("{total} frames")
        } else {
            format!("{} of {total} frames", table.rows.len())
        };
        h_flex()
            .flex_none()
            .justify_between()
            .gap_1()
            .px_3()
            .h(px(32.))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child("DECODED")
                    .children(self.codec.clone().map(|codec| {
                        div()
                            .truncate()
                            .text_color(theme.info)
                            .child(SharedString::from(codec))
                    })),
            )
            .child(
                h_flex().gap_2().child(SharedString::from(count)).child(
                    Button::new("decoded-follow")
                        .label("Follow")
                        .tooltip("Keep the newest frame in view")
                        .xsmall()
                        .ghost()
                        .toggled(self.follow)
                        .on_click(cx.listener(|this, _, _, cx| this.follow(cx))),
                ),
            )
    }
}

impl Render for DecodedPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        // Frames decoded before the codec was turned off still show.
        let has_frames = self
            .table
            .read(cx)
            .delegate()
            .frames
            .as_ref()
            .is_some_and(|frames| !frames.is_empty());
        let theme = cx.theme();
        let body = if self.codec.is_none() && !has_frames {
            let message = if self.session.is_some() {
                "No codec decodes this session. Pick one with the Codec menu in the session's \
                 toolbar, or name one as a device profile's \"plugin\"."
            } else {
                "Decoded frames of the session show here."
            };
            div()
                .flex_1()
                .px_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(message)
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .px_2()
                        .pb_1()
                        .child(
                            div()
                                .w(px(140.))
                                .flex_none()
                                .child(Select::new(&self.kinds).small().id("decoded-kind")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.filter).small().id("decoded-filter")),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(DataTable::new(&self.table).small().bordered(false)),
                )
                .into_any_element()
        };
        v_flex()
            .id("decoded-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_l_1()
            .border_color(theme.border)
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn frame(kind: &str, summary: &str) -> Frame {
        Frame::new(kind, 0..1, Instant::now()).with_summary(summary)
    }

    #[test]
    fn the_filters_look_at_kind_summary_and_fields() {
        let mut table = FrameTable::new();
        let log = frame("log", "log 0x0F40 len 4").with_field("cmd_id", 0x0F40u64);
        assert!(table.passes(&log));
        table.kind = Some("response".into());
        assert!(!table.passes(&log));
        table.kind = None;
        table.query = "0x0f40".into();
        assert!(table.passes(&log));
        table.query = "3904".into();
        assert!(table.passes(&log), "a field's value");
        table.query = "heartbeat".into();
        assert!(!table.passes(&log));
    }

    #[test]
    fn columns_follow_the_kind_filter() {
        let info = serialist_plugins::race::race_info();
        let all = FrameTable::columns_for(None, Some(&info));
        assert_eq!(
            all,
            [
                Col::Time,
                Col::Direction,
                Col::Kind,
                Col::Summary,
                Col::Fields,
                Col::Raw
            ]
        );
        let response = FrameTable::columns_for(Some("response"), Some(&info));
        assert_eq!(response[4], Col::Field("type".into()));
        assert_eq!(response.len(), 4 + 5 + 1);
        assert_eq!(response.last(), Some(&Col::Raw));
    }
}
