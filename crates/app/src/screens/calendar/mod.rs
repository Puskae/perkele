//! Calendar screen shell: a view switcher (Agenda / Kuukausi / Viikko) over a
//! shared, offline-seeded event cache. The editor (event create/edit/delete)
//! and the offline SSE/queue wiring are added in later tasks; this task ships
//! the online agenda path.

pub(crate) mod agenda;
mod editor;
mod month;
mod week;

pub use editor::{EventEditScreen, EventNewScreen};

use crate::api;
use crate::screens::Route;
use crate::store;
use dioxus::prelude::*;
use perkele_shared::calendar::{Event, Exdate};
use perkele_shared::recipe::MealPlanEntry;

/// Display label for a planned dinner: the recipe title when one is chosen,
/// otherwise the free-typed meal name. `None` when neither is set (e.g. the
/// planned recipe was later deleted) — the calendar projection skips those.
/// Used by all three views' read-only meal chips (5D).
pub(crate) fn meal_label(m: &MealPlanEntry) -> Option<&str> {
    m.recipe_title.as_deref().or(m.free_text.as_deref())
}

#[derive(Clone, Copy, PartialEq)]
enum CalView {
    Agenda,
    Month,
    Week,
}

impl CalView {
    /// Stable string used as the localStorage value. Kept separate from the
    /// Finnish button labels so we can retranslate the UI without breaking
    /// saved preferences.
    fn as_key(self) -> &'static str {
        match self {
            CalView::Agenda => "agenda",
            CalView::Month => "month",
            CalView::Week => "week",
        }
    }

    /// Parse a stored key back into a view; `None` for anything unrecognised
    /// (missing/corrupt value), letting the caller fall back to a default.
    fn from_key(s: &str) -> Option<Self> {
        match s {
            "agenda" => Some(CalView::Agenda),
            "month" => Some(CalView::Month),
            "week" => Some(CalView::Week),
            _ => None,
        }
    }
}

/// True when the browser reports connectivity (always true off-wasm).
pub(crate) fn is_online() -> bool {
    web_sys::window()
        .map(|w| w.navigator().on_line())
        .unwrap_or(true)
}

#[component]
pub fn CalendarScreen() -> Element {
    // Seed from cache for instant paint, then sync.
    let mut events: Signal<Vec<Event>> = use_signal(|| store::get_events().unwrap_or_default());
    let mut exdates: Signal<Vec<Exdate>> = use_signal(store::get_exdates);
    let mut error = use_signal(|| Option::<String>::None);
    // Restore the last-used view from localStorage; default to Month on first
    // visit (or if the stored value is missing/unrecognised).
    let mut view = use_signal(|| {
        store::get_cal_view()
            .as_deref()
            .and_then(CalView::from_key)
            .unwrap_or(CalView::Month)
    });
    // Becomes true when offline mutations are sitting in the queue; the fetch
    // effect below recomputes it once NavLayout has replayed the queue.
    let mut pending = use_signal(|| !store::get_cal_queue().is_empty());
    let nav = use_navigator();

    // Full sync, re-run on every app-wide tick: once on mount, then whenever
    // NavLayout's single SSE stream reports a family mutation or has replayed
    // the offline queue after a reconnect.
    let sync_tick = use_context::<Signal<u32>>();
    let fetched = use_resource(move || {
        let _ = sync_tick(); // subscribe
        async move { api::calendar_sync().await }
    });
    use_effect(move || {
        if let Some(res) = fetched() {
            match res {
                Ok(sync) => {
                    events.set(sync.events.clone());
                    store::set_events(&sync.events);
                    exdates.set(sync.exdates.clone());
                    store::set_exdates(&sync.exdates);
                    // NavLayout clears the queue before bumping the tick.
                    pending.set(!store::get_cal_queue().is_empty());
                }
                Err(e) => error.set(Some(e)),
            }
        }
    });

    rsx! {
        div { class: "card wide",
            div { class: "row spread",
                h1 { "Kalenteri" }
                button {
                    class: "primary",
                    style: "width:auto;margin:0;",
                    onclick: move |_| {
                        // Empty date = no pre-picked day (the query param is omitted).
                        nav.push(Route::EventNewScreen { date: String::new() });
                    },
                    "+ Uusi"
                }
            }

            div { class: "row", style: "gap:6px;margin:8px 0;",
                for (label , v) in [("Agenda", CalView::Agenda), ("Kuukausi", CalView::Month), ("Viikko", CalView::Week)] {
                    button {
                        class: if view() == v { "primary" } else { "ghost" },
                        style: "margin:0;padding:6px 12px;",
                        onclick: move |_| {
                            view.set(v);
                            store::set_cal_view(v.as_key());
                        },
                        "{label}"
                    }
                }
            }

            if pending() {
                p { class: "offline-banner", "Muutoksia jonossa — synkronoidaan kun yhteys palautuu." }
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            match view() {
                CalView::Agenda => rsx! { agenda::AgendaView { events: events(), exdates: exdates() } },
                CalView::Month => rsx! { month::MonthView { events: events(), exdates: exdates() } },
                CalView::Week => rsx! { week::WeekView { events: events(), exdates: exdates() } },
            }
        }
    }
}
