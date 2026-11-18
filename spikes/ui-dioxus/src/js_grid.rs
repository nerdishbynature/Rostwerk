//! AG Grid Community through JS interop.
//!
//! The grid runs in JS (loaded from a CDN) with its infinite row model. It asks
//! for row blocks by offset over a `document::eval` channel; Rust answers from
//! the API and also saves edits. Data access, auth and validation stay in Rust,
//! the grid only renders and handles keyboard and editing.
//!
//! The API pages by cursor, AG Grid by offset: Rust keeps the loaded prefix of
//! the current query and loads forward until the requested block is covered.

use dioxus::prelude::*;
use futures::StreamExt;
use futures::future::{Either, select};
use serde_json::{Value, json};

use crate::Shared;
use crate::api::{self, Partner};

const AG_GRID: &str =
    "https://cdn.jsdelivr.net/npm/ag-grid-community@33.3.2/dist/ag-grid-community.min.js";

const SCRIPT: &str = r#"
const src = __AG_GRID__;
if (!window.agGrid) {
  await new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = src; s.onload = resolve; s.onerror = reject;
    document.head.appendChild(s);
  });
}
let el;
while (!(el = document.getElementById('ag-grid'))) await new Promise(r => setTimeout(r, 10));

let seq = 0;
const pending = new Map();
const kinds = { customer: 'Kunde', supplier: 'Lieferant', both: 'Kunde + Lieferant' };
const datasource = {
  getRows(p) {
    const id = ++seq;
    pending.set(id, p);
    dioxus.send({ type: 'rows', id, start: p.startRow, end: p.endRow, sort: p.sortModel });
  },
};
const grid = agGrid.createGrid(el, {
  columnDefs: [
    { field: 'name', headerName: 'Name', editable: true, flex: 2 },
    { field: 'kind', headerName: 'Art', flex: 1, valueFormatter: p => kinds[p.value] ?? p.value ?? '' },
    { field: 'email', headerName: 'E-Mail', flex: 2 },
    { field: 'created_at', colId: 'created', headerName: 'Angelegt', flex: 1,
      valueFormatter: p => (p.value ?? '').slice(0, 10) },
  ],
  defaultColDef: { sortable: true },
  rowModelType: 'infinite',
  datasource,
  cacheBlockSize: 200,
  maxBlocksInCache: 100,
  rowHeight: 32,
  getRowId: p => p.data.id,
  stopEditingWhenCellsLoseFocus: true,
  onCellValueChanged: e => {
    if (e.colDef.field === 'name' && e.newValue !== e.oldValue) {
      dioxus.send({ type: 'edit', id: e.data.id, name: e.newValue, old: e.oldValue });
    }
  },
});
while (true) {
  const msg = await dioxus.recv();
  if (msg.type === 'rows') {
    const p = pending.get(msg.id);
    pending.delete(msg.id);
    if (!p) continue;
    if (msg.error) p.failCallback(); else p.successCallback(msg.rows, msg.last);
  } else if (msg.type === 'edited') {
    const node = grid.getRowNode(msg.id);
    // setData does not fire onCellValueChanged, so a revert is not sent back.
    if (node) node.setData(msg.ok ? msg.row : { ...node.data, name: msg.old });
  } else if (msg.type === 'refresh') {
    grid.purgeInfiniteCache();
  }
}
"#;

/// The loaded prefix of one query.
#[derive(Default)]
struct Prefix {
    query: Option<api::Query>,
    rows: Vec<Partner>,
    cursor: Option<String>,
    complete: bool,
}

enum Cmd {
    Refresh,
}

/// API sort key from AG Grid's sort model, e.g. `[{"colId": "name", "sort": "desc"}]`.
fn sort_key(model: &Value) -> String {
    let Some(first) = model.as_array().and_then(|m| m.first()) else {
        return "created".into();
    };
    let col = first["colId"].as_str().unwrap_or("created");
    match first["sort"].as_str() {
        Some("desc") => format!("-{col}"),
        _ => col.to_string(),
    }
}

#[component]
pub fn JsGridPage() -> Element {
    let Shared { token, filter } = use_context();
    let mut status = use_signal(String::new);

    let bridge = use_coroutine(move |mut commands: UnboundedReceiver<Cmd>| async move {
        let mut eval = document::eval(&SCRIPT.replace("__AG_GRID__", &format!("{AG_GRID:?}")));
        let mut prefix = Prefix::default();
        loop {
            // Decide first, so that the borrow of `eval` by `recv` ends before we send.
            let next = match select(Box::pin(eval.recv::<Value>()), commands.next()).await {
                Either::Left((msg, _)) => Either::Left(msg),
                Either::Right((cmd, _)) => Either::Right(cmd),
            };
            let msg = match next {
                Either::Left(Ok(msg)) => msg,
                Either::Left(Err(err)) => {
                    status.set(format!("JS: {err}"));
                    return;
                }
                Either::Right(Some(Cmd::Refresh)) => {
                    prefix = Prefix::default();
                    let _ = eval.send(json!({ "type": "refresh" }));
                    continue;
                }
                Either::Right(None) => return,
            };
            let token = token.peek().clone();
            match msg["type"].as_str() {
                Some("rows") => {
                    let start = msg["start"].as_u64().unwrap_or(0) as usize;
                    let end = msg["end"].as_u64().unwrap_or(0) as usize;
                    let query = api::Query {
                        sort: sort_key(&msg["sort"]),
                        ..filter.peek().clone()
                    };
                    if prefix.query.as_ref() != Some(&query) {
                        prefix = Prefix {
                            query: Some(query.clone()),
                            ..Default::default()
                        };
                    }
                    let mut error = None;
                    while prefix.rows.len() < end && !prefix.complete {
                        match api::list(&token, &query, prefix.cursor.as_deref(), 500).await {
                            Ok(page) => {
                                prefix.rows.extend(page.items);
                                prefix.complete = page.next_cursor.is_none();
                                prefix.cursor = page.next_cursor;
                            }
                            Err(err) => {
                                error = Some(err);
                                break;
                            }
                        }
                    }
                    if let Some(err) = &error {
                        status.set(format!("Fehler: {err}"));
                    } else {
                        let more = if prefix.complete {
                            ""
                        } else {
                            " (weitere vorhanden)"
                        };
                        status.set(format!("{} Partner geladen{more}", prefix.rows.len()));
                    }
                    let slice = prefix
                        .rows
                        .get(start..end.min(prefix.rows.len()))
                        .unwrap_or_default();
                    let last = if prefix.complete {
                        prefix.rows.len() as i64
                    } else {
                        -1
                    };
                    let _ = eval.send(json!({
                        "type": "rows", "id": msg["id"], "rows": slice, "last": last,
                        "error": error.is_some(),
                    }));
                }
                Some("edit") => {
                    let id = msg["id"].as_str().unwrap_or_default().to_string();
                    let name = msg["name"].as_str().unwrap_or_default().trim().to_string();
                    let reply = match api::rename(&token, &id, &name).await {
                        Ok(saved) => {
                            if let Some(row) = prefix.rows.iter_mut().find(|p| p.id == saved.id) {
                                *row = saved.clone();
                            }
                            status.set(format!("gespeichert: {}", saved.name));
                            json!({ "type": "edited", "id": id, "ok": true, "row": saved })
                        }
                        Err(err) => {
                            status.set(format!("Speichern fehlgeschlagen: {err}"));
                            json!({ "type": "edited", "id": id, "ok": false, "old": msg["old"] })
                        }
                    };
                    let _ = eval.send(reply);
                }
                _ => {}
            }
        }
    });

    // The shared filter lives outside the grid: on change, drop the cache and reload.
    use_effect(move || {
        let _ = (filter(), token());
        bridge.send(Cmd::Refresh);
    });

    rsx! {
        div { class: "grid-wrap",
            div { id: "ag-grid", class: "ag-host" }
            div { class: "statusbar",
                span { "{status}" }
                span { class: "hint", "AG Grid Community 33 aus dem CDN · Enter/F2 bearbeitet, Enter speichert, Esc verwirft" }
            }
        }
    }
}
