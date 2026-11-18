//! Spike for #34: can Dioxus carry an ERP data grid?
//!
//! Three takes on the same partner list, sharing one toolbar (token, text
//! filter, kind filter) whose filters run in the API:
//! - `grid`: own virtualized grid with keyboard navigation and inline editing
//! - `tabular`: dioxus-tabular (typed columns, sorting in memory)
//! - `js_grid`: AG Grid Community via JS interop (`document::eval`)

mod api;
mod grid;
mod js_grid;
mod tabular;

use dioxus::prelude::*;

const STYLE: Asset = asset!("/assets/style.css");

fn main() {
    dioxus::launch(App);
}

/// What all three views show.
#[derive(Clone, Copy)]
pub struct Shared {
    pub token: Signal<String>,
    /// Text and kind filter; each view keeps its own sort.
    pub filter: Signal<api::Query>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Grid,
    Tabular,
    JsGrid,
}

fn initial_token() -> String {
    if let Some(token) = option_env!("SPIKE_TOKEN") {
        return token.to_string();
    }
    #[cfg(target_arch = "wasm32")]
    if let Some(token) = web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item("spike_token").ok().flatten())
    {
        return token;
    }
    String::new()
}

#[component]
fn App() -> Element {
    let token = use_signal(initial_token);
    let filter = use_signal(api::Query::default);
    use_context_provider(|| Shared { token, filter });
    let mut tab = use_signal(|| Tab::Grid);

    rsx! {
        document::Stylesheet { href: STYLE }
        div { class: "app",
            header { class: "topbar",
                strong { "Rostwerk · Partner" }
                nav { class: "tabs", role: "tablist",
                    for (t, label) in [(Tab::Grid, "Eigenes Grid"), (Tab::Tabular, "dioxus-tabular"), (Tab::JsGrid, "AG Grid (JS)")] {
                        button {
                            role: "tab",
                            "aria-selected": tab() == t,
                            class: if tab() == t { "tab active" } else { "tab" },
                            onclick: move |_| tab.set(t),
                            "{label}"
                        }
                    }
                }
            }
            Toolbar {}
            main { class: "content",
                match tab() {
                    Tab::Grid => rsx! { grid::PartnerGrid {} },
                    Tab::Tabular => rsx! { tabular::TabularPage {} },
                    Tab::JsGrid => rsx! { js_grid::JsGridPage {} },
                }
            }
        }
    }
}

#[component]
fn Toolbar() -> Element {
    let Shared {
        mut token,
        mut filter,
    } = use_context();
    rsx! {
        div { class: "toolbar",
            label { class: "field grow",
                span { "Suche" }
                input {
                    r#type: "search",
                    placeholder: "Name oder E-Mail",
                    value: "{filter.read().q}",
                    oninput: move |e| filter.write().q = e.value(),
                }
            }
            label { class: "field",
                span { "Art" }
                select {
                    value: "{filter.read().kind}",
                    onchange: move |e| filter.write().kind = e.value(),
                    option { value: "", "Alle" }
                    option { value: "customer", "Kunden" }
                    option { value: "supplier", "Lieferanten" }
                    option { value: "both", "Kunde und Lieferant" }
                }
            }
            label { class: "field token",
                span { "Token" }
                input {
                    r#type: "password",
                    placeholder: "dev:<tenant>:<user> oder Access Token",
                    value: "{token}",
                    oninput: move |e| {
                        #[cfg(target_arch = "wasm32")]
                        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                            let _ = s.set_item("spike_token", &e.value());
                        }
                        token.set(e.value());
                    },
                }
            }
        }
    }
}
