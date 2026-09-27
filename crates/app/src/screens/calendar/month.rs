//! Month grid: 6 weeks × 7 days from the Monday on/before the 1st. Each cell
//! shows the day number and up to a few event titles; tapping an event opens
//! the editor.

use crate::api;
use crate::screens::Route;
use crate::screens::dates::{format_utc, shift_day, today};
use dioxus::prelude::*;
use perkele_shared::calendar::{Event, Exdate, event_overlaps, expand_events};
use perkele_shared::recipe::MealPlanEntry;
use wasm_bindgen::JsValue;

/// First day (UTC 'YYYY-MM-DD') to render: the Monday on/before the month's 1st.
fn grid_start(month_first: &str) -> String {
    let d = js_sys::Date::new(&JsValue::from_str(&format!("{month_first}T00:00:00Z")));
    // JS getUTCDay: 0=Sun..6=Sat. Convert to Monday=0.
    let weekday = ((d.get_utc_day() as i64) + 6) % 7;
    shift_day(month_first, -weekday)
}

/// 'YYYY-MM-01' for the month containing `date`.
fn month_first(date: &str) -> String {
    format!("{}-01", &date[..7.min(date.len())])
}

#[component]
pub fn MonthView(events: Vec<Event>, exdates: Vec<Exdate>) -> Element {
    let nav = use_navigator();
    // Anchor on the current month; navigation shifts by whole months.
    let mut anchor = use_signal(|| month_first(&format_utc(&js_sys::Date::new_0())));

    let first = month_first(&anchor());
    let start = grid_start(&first);
    let month_num = first[5..7].to_owned();
    // `today` is the fn; `today_str` the value we compare each cell against.
    let today_str = today();

    // 42 day cells (6 weeks).
    let days: Vec<String> = (0..42).map(|i| shift_day(&start, i)).collect();

    // Planned dinners over the visible grid (read-only projection, 5D).
    // Reading `anchor()` inside the closure subscribes the resource to it,
    // so month navigation refetches. Fetch errors (e.g. offline) just mean
    // no meal chips — the calendar itself still works from the event cache.
    let meals = use_resource(move || {
        let from = grid_start(&month_first(&anchor()));
        let to = shift_day(&from, 41);
        async move { api::mealplan_range(&from, &to).await.unwrap_or_default() }
    });
    // `meals()` is None until the first fetch resolves; render no chips then.
    let meals: Vec<MealPlanEntry> = meals().unwrap_or_default();

    // Recurring events: replace each series master with its occurrences
    // across the whole visible grid, then filter per-cell as before.
    let events = expand_events(
        &events,
        &exdates,
        &format!("{start}T00:00:00Z"),
        &format!("{}T23:59:59Z", days[41]),
    );

    let prev = {
        let first = first.clone();
        move |_| {
            let prev_last = shift_day(&first, -1);
            anchor.set(month_first(&prev_last));
        }
    };
    let next = {
        let first = first.clone();
        move |_| {
            // Day +32 always lands in the next month; normalize to its 1st.
            let into_next = shift_day(&first, 32);
            anchor.set(month_first(&into_next));
        }
    };

    let go_today = move |_| anchor.set(month_first(&today()));

    rsx! {
        div { class: "row spread",
            button { class: "ghost", onclick: prev, "‹" }
            div { class: "row", style: "gap:8px;",
                span { class: "muted", "{first}" }
                button {
                    class: "ghost",
                    style: "margin:0;padding:4px 10px;font-size:0.8rem;",
                    onclick: go_today,
                    "Tänään"
                }
            }
            button { class: "ghost", onclick: next, "›" }
        }
        div { class: "month-grid",
            for wd in ["Ma", "Ti", "Ke", "To", "Pe", "La", "Su"] {
                div { class: "month-head", "{wd}" }
            }
            for day in days {
                {
                    let win_start = format!("{day}T00:00:00Z");
                    let win_end = format!("{day}T23:59:59Z");
                    let day_num = day[8..10].trim_start_matches('0').to_owned();
                    let in_month = day[5..7] == month_num;
                    let is_today = day == today_str;
                    let cell_class = match (in_month, is_today) {
                        (_, true) => "month-cell today",
                        (true, false) => "month-cell",
                        (false, false) => "month-cell dim",
                    };
                    let todays: Vec<Event> = events
                        .iter()
                        .filter(|e| event_overlaps(e, &win_start, &win_end))
                        .cloned()
                        .collect();
                    // At most one dinner per day (UNIQUE(family_id, date)).
                    let meal: Option<String> = meals
                        .iter()
                        .filter(|m| m.date == day)
                        .find_map(super::meal_label)
                        .map(str::to_owned);
                    rsx! {
                        div {
                            class: "{cell_class}",
                            style: "cursor:pointer;",
                            // Tapping the cell itself starts a new event on that
                            // day; the form opens with start = {day}T12:00.
                            onclick: move |_| {
                                nav.push(Route::EventNewScreen { date: day.clone() });
                            },
                            div {
                                class: if is_today { "month-daynum today" } else { "month-daynum" },
                                "{day_num}"
                            }
                            if let Some(m) = meal {
                                div { class: "month-chip meal", "🍽 {m}" }
                            }
                            for ev in todays.into_iter().take(3) {
                                Link {
                                    to: Route::EventEditScreen { uid: ev.uid.clone() },
                                    class: "month-chip",
                                    // Without this, the tap would bubble up to the
                                    // cell's onclick and open a NEW event instead.
                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                    "{ev.title}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
