//! dioxus-tabular: typed columns, multi-column sort and filtering in memory.
//!
//! The crate works on a `Vec` it holds completely, so this page loads all
//! partners matching the shared filter (paged requests of 500) and lets the
//! crate sort them. It has no virtual scrolling; the checkbox switches between
//! rendering every row and rendering only a window of its sorted rows.

use dioxus::prelude::*;
use dioxus_tabular::*;
use web_time::Instant;

use crate::Shared;
use crate::api::{self, Partner};

const ROW_HEIGHT: f64 = 30.0;

#[derive(Clone, PartialEq)]
pub struct PartnerRow(Partner);

impl Row for PartnerRow {
    fn key(&self) -> impl Into<String> {
        self.0.id.clone()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Name,
    Kind,
    Email,
    Created,
}

#[derive(Clone, PartialEq)]
pub struct Col(Field);

impl Col {
    fn text(&self, p: &Partner) -> String {
        match self.0 {
            Field::Name => p.name.clone(),
            Field::Kind => p.kind.clone(),
            Field::Email => p.email.clone().unwrap_or_default(),
            Field::Created => p.created_at.get(..10).unwrap_or_default().to_string(),
        }
    }
}

impl TableColumn<PartnerRow> for Col {
    fn column_name(&self) -> String {
        match self.0 {
            Field::Name => "Name",
            Field::Kind => "Art",
            Field::Email => "E-Mail",
            Field::Created => "Angelegt",
        }
        .into()
    }

    fn render_header(&self, context: ColumnContext, attributes: Vec<Attribute>) -> Element {
        let label = self.column_name();
        rsx! {
            th { ..attributes,
                SortHeader { context, label }
            }
        }
    }

    fn render_cell(
        &self,
        _context: ColumnContext,
        row: &PartnerRow,
        attributes: Vec<Attribute>,
    ) -> Element {
        let text = self.text(&row.0);
        rsx! {
            td { ..attributes, "{text}" }
        }
    }

    fn compare(&self, a: &PartnerRow, b: &PartnerRow) -> std::cmp::Ordering {
        match self.0 {
            Field::Created => a.0.id.cmp(&b.0.id),
            _ => self
                .text(&a.0)
                .to_lowercase()
                .cmp(&self.text(&b.0).to_lowercase()),
        }
    }
}

#[component]
fn SortHeader(context: ColumnContext, label: String) -> Element {
    let info = context.sort_info();
    let arrow = match &info {
        Some(i) if i.direction == SortDirection::Ascending => format!(" ▲{}", i.priority + 1),
        Some(i) => format!(" ▼{}", i.priority + 1),
        None => String::new(),
    };
    rsx! {
        button {
            class: "sort-button",
            title: "Klick: sortieren / umkehren · Shift+Klick: Sortierung entfernen",
            onclick: move |e| {
                if e.modifiers().shift() {
                    context.request_sort(SortGesture::Cancel);
                } else if context.sort_info().is_some() {
                    context.request_sort(SortGesture::Toggle);
                } else {
                    context.request_sort(SortGesture::AddFirst(Sort { direction: SortDirection::Ascending }));
                }
            },
            "{label}{arrow}"
        }
    }
}

#[component]
pub fn TabularPage() -> Element {
    let Shared { token, filter } = use_context();
    let mut rows = use_signal(Vec::<PartnerRow>::new);
    let mut status = use_signal(String::new);
    let mut virtualize = use_signal(|| true);
    let mut scroll_top = use_signal(|| 0.0_f64);

    // Load everything that matches the filter; the crate sorts in memory.
    let _loader = use_resource(move || async move {
        let query = api::Query {
            sort: "created".into(),
            ..filter()
        };
        let token = token();
        api::sleep_ms(200).await;
        let started = Instant::now();
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        status.set("lädt …".into());
        loop {
            match api::list(&token, &query, cursor.as_deref(), 500).await {
                Ok(page) => {
                    all.extend(page.items.into_iter().map(PartnerRow));
                    status.set(format!("lädt … {}", all.len()));
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                }
                Err(err) => {
                    status.set(format!("Fehler: {err}"));
                    return;
                }
            }
        }
        status.set(format!(
            "{} Partner in {} ms geladen",
            all.len(),
            started.elapsed().as_millis()
        ));
        rows.set(all);
    });

    let data = use_tabular(
        (
            Col(Field::Name),
            Col(Field::Kind),
            Col(Field::Email),
            Col(Field::Created),
        ),
        rows.into(),
    );

    let total = rows.read().len();
    let (first, last) = if virtualize() {
        let first = ((scroll_top() / ROW_HEIGHT) as usize)
            .saturating_sub(10)
            .min(total);
        (first, (first + 50).min(total))
    } else {
        (0, total)
    };

    rsx! {
        div { class: "grid-wrap",
            div { class: "toolbar slim",
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: virtualize(),
                        onchange: move |e| virtualize.set(e.checked()),
                    }
                    "nur sichtbare Zeilen rendern (eigenes Fenster über den sortierten Zeilen)"
                }
                span { class: "hint", "{status} · {last - first} Zeilen im DOM" }
            }
            div {
                class: "table-scroll",
                onscroll: move |e| scroll_top.set(e.data().scroll_top()),
                table { class: "tabular",
                    thead {
                        tr { TableHeaders { data } }
                    }
                    tbody {
                        if first > 0 {
                            tr { style: "height: {first as f64 * ROW_HEIGHT}px" }
                        }
                        for row in data.rows().skip(first).take(last - first) {
                            tr { key: "{row.key()}", style: "height: {ROW_HEIGHT}px",
                                TableCells { row }
                            }
                        }
                        if last < total {
                            tr { style: "height: {(total - last) as f64 * ROW_HEIGHT}px" }
                        }
                    }
                }
            }
        }
    }
}
