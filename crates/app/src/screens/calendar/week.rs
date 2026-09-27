//! Week view: seven day columns (Mon–Sun). All-day events sort first per day;
//! timed events list chronologically with their start time. Prev/next shifts
//! by a week. A columns+chips layout (not a pixel-positioned hour grid) reads
//! better on a phone and keeps the code simple.

use crate::api;
use crate::screens::Route;
use crate::screens::dates::{current_monday, shift_day, today};
use dioxus::prelude::*;
use perkele_shared::calendar::{Event, Exdate, event_overlaps, expand_events};
use perkele_shared::recipe::MealPlanEntry;

const WEEKDAYS: [&str; 7] = ["Ma", "Ti", "Ke", "To", "Pe", "La", "Su"];

fn hhmm(utc: &str) -> String {
    if utc.len() >= 16 {
        utc[11..16].to_owned()
    } else {
        utc.to_owned()
    }
}

#[component]
pub fn WeekView(events: Vec<Event>, exdates: Vec<Exdate>) -> Element {
    let mut monday = use_signal(current_monday);

    let days: Vec<String> = (0..7).map(|i| shift_day(&monday(), i)).collect();
    let today_str = today();

    // Planned dinners for the visible week (read-only projection, 5D).
    // Reading `monday()` subscribes the resource, so week navigation refetches;
    // a failed fetch (offline) just means no meal chips.
    let meals = use_resource(move || {
        let from = monday();
        let to = shift_day(&from, 6);
        async move { api::mealplan_range(&from, &to).await.unwrap_or_default() }
    });
    let meals: Vec<MealPlanEntry> = meals().unwrap_or_default();

    // Expand series masters into occurrences over the visible week.
    let events = expand_events(
        &events,
        &exdates,
        &format!("{}T00:00:00Z", days[0]),
        &format!("{}T23:59:59Z", days[6]),
    );

    rsx! {
        div { class: "row spread",
            button { class: "ghost", onclick: move |_| monday.set(shift_day(&monday(), -7)), "‹" }
            div { class: "row", style: "gap:8px;",
                span { class: "muted", "{monday}" }
                button {
                    class: "ghost",
                    style: "margin:0;padding:4px 10px;font-size:0.8rem;",
                    onclick: move |_| monday.set(current_monday()),
                    "Tänään"
                }
            }
            button { class: "ghost", onclick: move |_| monday.set(shift_day(&monday(), 7)), "›" }
        }
        div { class: "week-grid",
            for (i , day) in days.into_iter().enumerate() {
                {
                    let win_start = format!("{day}T00:00:00Z");
                    let win_end = format!("{day}T23:59:59Z");
                    let mut todays: Vec<Event> = events
                        .iter()
                        .filter(|e| event_overlaps(e, &win_start, &win_end))
                        .cloned()
                        .collect();
                    todays.sort_by(|a, b| {
                        a.all_day
                            .cmp(&b.all_day)
                            .reverse()
                            .then(a.starts_at.cmp(&b.starts_at))
                    });
                    let dnum = day[8..10].trim_start_matches('0').to_owned();
                    let is_today = day == today_str;
                    // At most one dinner per day (UNIQUE(family_id, date)).
                    let meal: Option<String> = meals
                        .iter()
                        .filter(|m| m.date == day)
                        .find_map(super::meal_label)
                        .map(str::to_owned);
                    rsx! {
                        div { class: if is_today { "week-col today" } else { "week-col" },
                            div { class: "week-head",
                                "{WEEKDAYS[i]} "
                                if is_today {
                                    span { class: "today", "{dnum}" }
                                } else {
                                    "{dnum}"
                                }
                            }
                            for ev in todays {
                                Link {
                                    to: Route::EventEditScreen { uid: ev.uid.clone() },
                                    class: if ev.all_day { "week-chip allday" } else { "week-chip" },
                                    if !ev.all_day {
                                        span { class: "muted", "{hhmm(&ev.starts_at)} " }
                                    }
                                    "{ev.title}"
                                }
                            }
                            // Dinner last: it's an evening thing, so it reads
                            // naturally below the day's timed events.
                            if let Some(m) = meal {
                                div { class: "week-chip meal", "🍽 {m}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
