# Spike: Dioxus data grid (#34)

Throwaway code for issue #34. It is not part of the workspace build (own
`[workspace]` in `Cargo.toml`) and gets deleted once #20 is decided.

The same partner list is built three ways, sharing one toolbar (text filter,
kind filter, token):

| Tab | What it is |
| --- | --- |
| Eigenes Grid | Own Dioxus grid (`src/grid.rs`): virtual scrolling, sort and filter in the API, cursor paging, keyboard navigation, inline editing of `name` via `PATCH /v1/partners/{id}` |
| dioxus-tabular | `dioxus-tabular` 0.3 (`src/tabular.rs`): typed columns, multi-column sort in memory |
| AG Grid (JS) | AG Grid Community 33 through `document::eval` (`src/js_grid.rs`); Rust serves the rows and saves edits |

## Run it

Needs the docker-compose Postgres, `.env`, the `wasm32-unknown-unknown` target and
`dioxus-cli` 0.7 (`cargo install dioxus-cli --version '^0.7' --locked`).

```bash
spikes/ui-dioxus/scripts/api.sh            # api-server on :18080, own database rostwerk_ui_spike
python3 spikes/ui-dioxus/scripts/seed.py   # tenant + 10,000 partners, prints the dev token
cd spikes/ui-dioxus
SPIKE_TOKEN=dev:<tenant>:spike dx serve --web --features web --port 18081   # http://127.0.0.1:18081
dx serve --desktop --features desktop      # desktop; ROSTWERK_API overrides http://127.0.0.1:18080
```

Without `SPIKE_TOKEN`, paste a token (dev token or Keycloak access token) into the
toolbar. `scripts/walkthrough.js` is the Playwright run the numbers below come from.

The API part (`sort`, `q` and `kind` on `GET /v1/partners`, keyset over sort key and id)
is in `crates/module-masterdata/src/partners.rs` on this branch only.

## Findings

Measured in headless Chromium (web build, dev profile), 10,000 partners, 1280×800.

**Own grid (Dioxus, ~480 lines).**
- Virtual scrolling: about 30–36 rows in the DOM at any time. Scrolling through all
  10,000 rows, every frame was 16.7–16.8 ms (median, p95 and max), so no dropped frames.
- Pages of 200 from the API in about 25–45 ms each, loaded ahead while scrolling.
  Sorting by name, e-mail, kind or creation, plus the text and kind filters, runs in the
  API; the client never holds the whole list.
- Keyboard: arrows, PageUp/PageDown, Home/End, Ctrl+Home/End. Enter, F2 or just typing
  starts editing the name; Enter saves and moves down; Esc cancels. API validation
  errors (422) show inline and the input keeps focus.
- Real HTML with `role=grid`/`row`/`gridcell`, `aria-rowindex`, `aria-sort` and
  `aria-selected`: Ctrl+F finds rendered rows, text is selectable, autofill works on inputs.
- Bugs found and fixed during the spike, each a requirement for a real grid component:
  - Programmatic scrolls must wait until the render after the spacer grows, or the
    browser clamps them.
  - A disabled input loses focus, so after a failed save the user couldn't keep typing.
  - Keyset paging over an editable sort key returns a renamed row again at its new
    position, so the client must dedupe by id.

**dioxus-tabular 0.3.** A good column abstraction (typed columns, multi-sort, column
order and visibility, export), but it works on a `Vec` held in memory. It sorts and
filters on the client, so all 10,000 rows had to be loaded (20 requests, 0.8 s). Without
windowing, toggling a sort re-rendered all rows in about 1.8 s. With my own window over
its sorted rows, the same sort took about 85 ms. It has no virtual scrolling, keyboard
navigation or editing. Usable at most as the column-definition layer.

**table-rs 0.1.** Not built. Its Dioxus columns use `&'static str` ids and render string
cells, with client-side search and pagination. It is weaker than dioxus-tabular for typed
ERP data.

**AG Grid Community 33 via interop.** Works. Rust answers the grid's block requests over
the eval channel and saves edits, so auth, API access and validation stay in Rust. It
offers the most out of the box: column resizing, keyboard navigation and editing. The
costs:
- The infinite row model asks by offset, but our API pages by cursor. Rust has to load
  the whole prefix up to the requested block, and jumping to the end of an unknown-length
  list doesn't work.
- State lives in two places: grid state in JS, everything else in Dioxus.
- A JS bundle (about 1 MB) has to be vendored for desktop and offline use.
- Enterprise features (row grouping, range selection, Excel export) are licensed per
  developer.
- Styling has to be done twice.

**Desktop.** Not verified in this environment. Dioxus desktop on Linux needs the
WebKitGTK and GTK3 development packages (about 630 packages here, no root access).
`cargo check --features desktop` stops at `glib-sys`. Still to do on a dev machine:
`dx serve --desktop --features desktop`. Nothing in the code is web-only except the
`web_sys` token storage and origin lookup (`cfg`-gated), and AG Grid loading from a CDN.

## Recommendation (draft)

**Dioxus with our own grid component.** The hard parts (virtual scrolling at 60 fps,
keyboard navigation, inline editing with server validation) took a few hundred lines
and stay in one Rust state model. They also fit the cursor-paged, server-sorted API
that large ERP lists need anyway.

Rough effort for a reusable grid component: about 2–3 weeks. That covers:
- column definitions generated from OpenAPI and the custom-field schema
- an editor per type (text, enum, date, money, reference, custom field)
- row and range selection with copy and paste
- column resizing and pinning
- server-side multi-sort and filters
- validation display per cell
- screen reader testing

Keep AG Grid as the fallback if that estimate slips. egui wasn't tried; given the
requirements above (real DOM, autofill, Ctrl+F, screen readers), it would need a strong
reason.

Follow-ups outside this spike: sort and filter parameters on list endpoints as a real
API change (with indexes per sort key), and the desktop check.
