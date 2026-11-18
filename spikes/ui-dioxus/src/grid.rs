//! Own grid: virtual scrolling over server-paged rows, sorting and filtering
//! in the API, keyboard navigation between cells, inline editing of `name`.
//!
//! Only the rows in view (plus a few above and below) are in the DOM. The
//! scroll container holds a spacer as tall as all loaded rows; the visible
//! rows are absolutely positioned inside it. Rows have a fixed height.

use std::rc::Rc;

use dioxus::html::geometry::PixelsVector2D;
use dioxus::prelude::*;
use web_time::Instant;

use crate::Shared;
use crate::api::{self, Partner};

const ROW_HEIGHT: f64 = 32.0;
/// Rows rendered above and below the visible ones.
const OVERSCAN: usize = 8;
/// Rows per API request.
const PAGE: usize = 200;
/// Start loading the next page when this close to the end of the loaded rows.
const PREFETCH: usize = 60;

/// Columns: header, API sort key.
const COLUMNS: [(&str, &str); 4] = [
    ("Name", "name"),
    ("Art", "kind"),
    ("E-Mail", "email"),
    ("Angelegt", "created"),
];
const NAME: usize = 0;

#[derive(Default)]
struct Rows {
    items: Vec<Partner>,
    cursor: Option<String>,
    complete: bool,
    loading: bool,
    error: Option<String>,
    last_request_ms: Option<u128>,
}

#[derive(Clone, PartialEq)]
struct Edit {
    row: usize,
    id: String,
    original: String,
    draft: String,
    error: Option<String>,
    saving: bool,
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "customer" => "Kunde",
        "supplier" => "Lieferant",
        "both" => "Kunde + Lieferant",
        other => other,
    }
}

#[component]
pub fn PartnerGrid() -> Element {
    let Shared { token, filter } = use_context();
    let mut sort = use_signal(|| "name".to_string());
    let query = use_memo(move || api::Query {
        sort: sort(),
        ..filter()
    });

    let mut rows = use_signal(Rows::default);
    // Bumped on every new query; responses for older queries are dropped.
    let mut generation = use_signal(|| 0_u64);
    let mut active = use_signal(|| (0_usize, NAME));
    let mut editing = use_signal(|| None::<Edit>);
    let mut scroll_top = use_signal(|| 0.0_f64);
    let mut viewport_height = use_signal(|| 600.0_f64);
    let mut viewport = use_signal(|| None::<Rc<MountedData>>);
    let mut pending_scroll = use_signal(|| None::<f64>);

    // Fetches one page and appends it (or replaces all rows for the first page).
    let mut fetch = move |r#gen: u64, after: Option<String>| {
        rows.write().loading = true;
        spawn(async move {
            let started = Instant::now();
            let token = token.peek().clone();
            let query = query.peek().clone();
            let result = api::list(&token, &query, after.as_deref(), PAGE).await;
            if *generation.peek() != r#gen {
                return;
            }
            let mut rows = rows.write();
            rows.loading = false;
            rows.last_request_ms = Some(started.elapsed().as_millis());
            match result {
                Ok(page) => {
                    if after.is_none() {
                        rows.items.clear();
                    }
                    // Keyset paging over a sort key that can change: a row renamed
                    // after its page was loaded comes again at its new position.
                    let seen: std::collections::HashSet<String> =
                        rows.items.iter().map(|p| p.id.clone()).collect();
                    rows.items
                        .extend(page.items.into_iter().filter(|p| !seen.contains(&p.id)));
                    rows.complete = page.next_cursor.is_none();
                    rows.cursor = page.next_cursor;
                    rows.error = None;
                }
                Err(err) => rows.error = Some(err),
            }
        });
    };

    // New query: debounce typing, then load the first page and jump to the top.
    use_effect(move || {
        let _ = (query(), token());
        let r#gen = generation.peek().wrapping_add(1);
        generation.set(r#gen);
        editing.set(None);
        spawn(async move {
            api::sleep_ms(200).await;
            if *generation.peek() != r#gen {
                return;
            }
            let col = active.peek().1;
            active.set((0, col));
            if let Some(el) = viewport.peek().clone() {
                let _ = el
                    .scroll(PixelsVector2D::new(0.0, 0.0), ScrollBehavior::Instant)
                    .await;
            }
            scroll_top.set(0.0);
            fetch(r#gen, None);
        });
    });

    // Loads the next page when `row` is close to the end of what is loaded.
    let mut load_more_near = move |row: usize| {
        let (len, loading, complete, cursor) = {
            let r = rows.peek();
            (r.items.len(), r.loading, r.complete, r.cursor.clone())
        };
        if !loading && !complete && row + PREFETCH >= len {
            fetch(*generation.peek(), cursor);
        }
    };

    // Scrolls so that `row` is fully visible.
    let mut reveal = move |row: usize| {
        let top = row as f64 * ROW_HEIGHT;
        let (current, height) = (*scroll_top.peek(), *viewport_height.peek());
        let target = if top < current {
            top
        } else if top + ROW_HEIGHT > current + height {
            top + ROW_HEIGHT - height
        } else {
            return;
        };
        scroll_top.set(target);
        pending_scroll.set(Some(target));
    };

    // Scroll after the render that made the spacer tall enough; scrolling
    // right away would be clamped to the old height.
    use_effect(move || {
        let Some(target) = pending_scroll() else {
            return;
        };
        pending_scroll.set(None);
        if let Some(el) = viewport.peek().clone() {
            spawn(async move {
                let _ = el
                    .scroll(PixelsVector2D::new(0.0, target), ScrollBehavior::Instant)
                    .await;
            });
        }
    });

    let focus_grid = move || {
        if let Some(el) = viewport.peek().clone() {
            spawn(async move {
                let _ = el.set_focus(true).await;
            });
        }
    };

    let mut start_edit = move |row: usize, draft: Option<String>| {
        let Some(partner) = rows.peek().items.get(row).cloned() else {
            return;
        };
        editing.set(Some(Edit {
            row,
            id: partner.id,
            draft: draft.unwrap_or_else(|| partner.name.clone()),
            original: partner.name,
            error: None,
            saving: false,
        }));
    };

    // Saves the edited name; on success optionally moves down one row.
    let commit = move |advance: bool| {
        let Some(edit) = editing.peek().clone() else {
            return;
        };
        if edit.saving {
            return;
        }
        let name = edit.draft.trim().to_string();
        if name == edit.original {
            editing.set(None);
            focus_grid();
            return;
        }
        if let Some(e) = editing.write().as_mut() {
            e.saving = true;
            e.error = None;
        }
        spawn(async move {
            let token = token.peek().clone();
            match api::rename(&token, &edit.id, &name).await {
                Ok(saved) => {
                    if let Some(row) = rows.write().items.iter_mut().find(|p| p.id == saved.id) {
                        *row = saved;
                    }
                    editing.set(None);
                    if advance {
                        let last = rows.peek().items.len().saturating_sub(1);
                        let next = (edit.row + 1).min(last);
                        active.set((next, NAME));
                        reveal(next);
                    }
                    focus_grid();
                }
                Err(err) => {
                    if let Some(e) = editing.write().as_mut() {
                        e.saving = false;
                        e.error = Some(err);
                    }
                }
            }
        });
    };

    let onkeydown = move |e: KeyboardEvent| {
        if editing.peek().is_some() {
            if e.key() == Key::Escape {
                editing.set(None);
            }
            return;
        }
        let total = rows.peek().items.len();
        if total == 0 {
            return;
        }
        let page = ((*viewport_height.peek() / ROW_HEIGHT) as usize).max(1);
        let (row, col) = *active.peek();
        let last_row = total - 1;
        let last_col = COLUMNS.len() - 1;
        let ctrl = e.modifiers().ctrl() || e.modifiers().meta();
        let next = match e.key() {
            Key::ArrowDown => ((row + 1).min(last_row), col),
            Key::ArrowUp => (row.saturating_sub(1), col),
            Key::ArrowRight => (row, (col + 1).min(last_col)),
            Key::ArrowLeft => (row, col.saturating_sub(1)),
            Key::PageDown => ((row + page).min(last_row), col),
            Key::PageUp => (row.saturating_sub(page), col),
            Key::Home if ctrl => (0, col),
            Key::End if ctrl => (last_row, col),
            Key::Home => (row, 0),
            Key::End => (row, last_col),
            Key::Enter | Key::F2 if col == NAME => {
                e.prevent_default();
                start_edit(row, None);
                return;
            }
            // Typing on a name cell starts editing with that character, as in a spreadsheet.
            Key::Character(c) if col == NAME && !ctrl && !e.modifiers().alt() => {
                e.prevent_default();
                start_edit(row, Some(c));
                return;
            }
            _ => return,
        };
        e.prevent_default();
        active.set(next);
        reveal(next.0);
        load_more_near(next.0);
    };

    let r = rows.read();
    let total = r.items.len();
    let first = ((scroll_top() / ROW_HEIGHT) as usize)
        .saturating_sub(OVERSCAN)
        .min(total);
    let count = (viewport_height() / ROW_HEIGHT).ceil() as usize + 2 * OVERSCAN;
    let last = (first + count).min(total);
    let current_sort = sort();
    let (active_row, active_col) = active();
    let edit = editing();

    let status = {
        let more = if r.complete {
            ""
        } else {
            " (weitere vorhanden)"
        };
        let ms = r
            .last_request_ms
            .map(|ms| format!(" · letzte Anfrage {ms} ms"))
            .unwrap_or_default();
        let loading = if r.loading { " · lädt …" } else { "" };
        format!(
            "{total} Partner geladen{more}{ms}{loading} · {} Zeilen im DOM",
            last - first
        )
    };

    rsx! {
        div { class: "grid-wrap",
            div {
                class: "grid",
                role: "grid",
                "aria-label": "Partner",
                "aria-rowcount": if r.complete { (total + 1) as i64 } else { -1 },
                "aria-colcount": COLUMNS.len(),
                div { class: "grid-header", role: "row", "aria-rowindex": 1,
                    for (label, key) in COLUMNS {
                        {
                            let state = if current_sort == key {
                                "ascending"
                            } else if current_sort.strip_prefix('-') == Some(key) {
                                "descending"
                            } else {
                                "none"
                            };
                            let arrow = match state {
                                "ascending" => " ▲",
                                "descending" => " ▼",
                                _ => "",
                            };
                            rsx! {
                                div {
                                    class: "cell header-cell",
                                    role: "columnheader",
                                    "aria-sort": state,
                                    onclick: move |_| {
                                        let next = if *sort.peek() == key { format!("-{key}") } else { key.to_string() };
                                        sort.set(next);
                                    },
                                    "{label}{arrow}"
                                }
                            }
                        }
                    }
                }
                div {
                    class: "viewport",
                    tabindex: 0,
                    onmounted: move |e| viewport.set(Some(e.data())),
                    // At mount the element has no layout yet; a resize observer
                    // reports the real height, also when the window changes.
                    onresize: move |e| {
                        if let Ok(size) = e.data().get_border_box_size() {
                            if size.height > 0.0 {
                                viewport_height.set(size.height);
                            }
                        }
                    },
                    onscroll: move |e| {
                        scroll_top.set(e.data().scroll_top());
                        viewport_height.set(e.data().client_height() as f64);
                        let bottom_row = ((e.data().scroll_top() + e.data().client_height() as f64) / ROW_HEIGHT) as usize;
                        load_more_near(bottom_row);
                    },
                    onkeydown,
                    div { class: "spacer", style: "height: {total as f64 * ROW_HEIGHT}px",
                        for i in first..last {
                            {
                                let p = &r.items[i];
                                let cells = [
                                    p.name.clone(),
                                    kind_label(&p.kind).to_string(),
                                    p.email.clone().unwrap_or_default(),
                                    p.created_at.get(..10).unwrap_or_default().to_string(),
                                ];
                                rsx! {
                                    div {
                                        key: "{p.id}",
                                        class: if i == active_row { "grid-row active-row" } else { "grid-row" },
                                        role: "row",
                                        "aria-rowindex": i + 2,
                                        style: "top: {i as f64 * ROW_HEIGHT}px",
                                        for (c, text) in cells.into_iter().enumerate() {
                                            if c == NAME && edit.as_ref().is_some_and(|e| e.row == i) {
                                                EditCell {
                                                    edit: edit.clone().unwrap(),
                                                    oninput: move |v: String| {
                                                        if let Some(e) = editing.write().as_mut() {
                                                            e.draft = v;
                                                        }
                                                    },
                                                    oncommit: commit,
                                                    oncancel: move |_| {
                                                        editing.set(None);
                                                        focus_grid();
                                                    },
                                                }
                                            } else {
                                                div {
                                                    class: if (i, c) == (active_row, active_col) { "cell active" } else { "cell" },
                                                    role: "gridcell",
                                                    "aria-selected": (i, c) == (active_row, active_col),
                                                    onclick: move |_| active.set((i, c)),
                                                    ondoubleclick: move |_| {
                                                        active.set((i, c));
                                                        if c == NAME {
                                                            start_edit(i, None);
                                                        }
                                                    },
                                                    "{text}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "statusbar",
                span { "{status}" }
                if let Some(err) = &r.error {
                    span { class: "error", "Fehler: {err}" }
                }
                if let Some(Edit { error: Some(err), .. }) = &edit {
                    span { class: "error", "Speichern fehlgeschlagen: {err}" }
                }
                span { class: "hint",
                    "Pfeiltasten, Bild↑/↓, Pos1/Ende · Enter/F2 oder Tippen bearbeitet den Namen · Enter speichert, Esc verwirft"
                }
            }
        }
    }
}

#[component]
fn EditCell(
    edit: Edit,
    oninput: EventHandler<String>,
    oncommit: EventHandler<bool>,
    oncancel: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "cell editing", role: "gridcell", "aria-selected": true,
            input {
                class: if edit.error.is_some() { "cell-input invalid" } else { "cell-input" },
                value: "{edit.draft}",
                // readonly, not disabled: a disabled input loses focus, and after a
                // failed save the user must be able to fix the value right away.
                readonly: edit.saving,
                "aria-label": "Name",
                "aria-invalid": edit.error.is_some(),
                onmounted: move |e| async move {
                    let _ = e.data().set_focus(true).await;
                },
                oninput: move |e| oninput.call(e.value()),
                onkeydown: move |e| {
                    e.stop_propagation();
                    match e.key() {
                        Key::Enter => {
                            e.prevent_default();
                            oncommit.call(true);
                        }
                        Key::Escape => {
                            e.prevent_default();
                            oncancel.call(());
                        }
                        _ => {}
                    }
                },
                onblur: move |_| oncommit.call(false),
            }
        }
    }
}
