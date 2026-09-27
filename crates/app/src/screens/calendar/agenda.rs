//! Agenda view: upcoming events in chronological order, grouped by day.
//! Planned dinners (meal-plan projection, 5D) appear as a read-only row at
//! the end of their day — a dinner is an evening thing.

use crate::api;
use crate::screens::Route;
use crate::screens::dates::{shift_day, today, weekday_fi};
use dioxus::prelude::*;
use perkele_shared::auth::UserView;
use perkele_shared::calendar::{Event, Exdate, expand_events, starts_within};
use perkele_shared::recipe::MealPlanEntry;
use std::collections::BTreeMap;

/// Render 'YYYY-MM-DD…' → 'D.M.' for a day heading (Finnish short date).
pub(crate) fn day_label(date: &str) -> String {
    let ymd = &date[..10.min(date.len())];
    let parts: Vec<&str> = ymd.split('-').collect();
    if parts.len() == 3 {
        let d = parts[2].trim_start_matches('0');
        let m = parts[1].trim_start_matches('0');
        format!("{d}.{m}.")
    } else {
        ymd.to_owned()
    }
}

/// Render an event's time for display: "HH:MM" (UTC clock) or "koko päivä".
pub(crate) fn time_label(ev: &Event) -> String {
    if ev.all_day {
        return "koko päivä".to_owned();
    }
    let t = &ev.starts_at;
    if t.len() >= 16 {
        t[11..16].to_owned()
    } else {
        t.clone()
    }
}

#[component]
pub fn AgendaView(events: Vec<Event>, exdates: Vec<Exdate>) -> Element {
    // The agenda has no navigation window, so recurring events expand from
    // today to an 8-week horizon.
    let day0 = today();
    let horizon = shift_day(&day0, 56);
    let mut events = expand_events(
        &events,
        &exdates,
        &format!("{day0}T00:00:00Z"),
        &format!("{horizon}T23:59:59Z"),
    );
    // expand_events only bounds the RECURRING expansion — one-off events pass
    // through untouched, past ones included — so clamp to the window ourselves
    // or last week's events keep their own day heading at the top of the list.
    events.retain(|e| starts_within(e, &day0, &horizon));

    // Planned dinners over the same horizon. No reactive inputs here (today
    // and the horizon are recomputed only on mount); a failed fetch (offline)
    // just means no dinner rows.
    let meals = use_resource(move || async move {
        let from = today();
        let to = shift_day(&from, 56);
        api::mealplan_range(&from, &to).await.unwrap_or_default()
    });
    let meals: Vec<MealPlanEntry> = meals().unwrap_or_default();

    // Family members, for turning attendee_ids into display names. Until the
    // fetch resolves (or if it fails offline), rows just show no names.
    let members = use_resource(api::members);
    let members: Vec<UserView> = match &*members.read_unchecked() {
        Some(Ok(m)) => m.clone(),
        _ => vec![],
    };

    // Sort by start, then group per day into an ordered map. A BTreeMap keyed
    // by 'YYYY-MM-DD' iterates chronologically for free, and lets a dinner on
    // an event-less day still get a heading (its entry holds an empty Vec).
    let mut sorted = events;
    sorted.sort_by(|a, b| a.starts_at.cmp(&b.starts_at));

    let mut by_day: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for ev in sorted {
        let day = ev.starts_at[..10.min(ev.starts_at.len())].to_owned();
        by_day.entry(day).or_default().push(ev);
    }
    for m in &meals {
        if super::meal_label(m).is_some() {
            by_day.entry(m.date.clone()).or_default();
        }
    }

    if by_day.is_empty() {
        return rsx! {
            p { class: "muted", "Ei tapahtumia." }
        };
    }

    rsx! {
        ul { class: "members",
            for (day , evs) in by_day {
                {
                    // At most one dinner per day (UNIQUE(family_id, date)).
                    let meal: Option<String> = meals
                        .iter()
                        .filter(|m| m.date == day)
                        .find_map(super::meal_label)
                        .map(str::to_owned);
                    rsx! {
                        li { style: "border:0;padding-bottom:0;",
                            strong { "{day_label(&day)} {weekday_fi(&day)}" }
                        }
                        for ev in evs {
                            {
                                // Tagged family members, as "Mikko, Mari". Ids whose
                                // member is unknown (fetch pending / user removed)
                                // are silently skipped.
                                let names: String = ev
                                    .attendee_ids
                                    .iter()
                                    .filter_map(|id| members.iter().find(|m| m.id == *id))
                                    .map(|m| m.display_name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                rsx! {
                                    li {
                                        Link {
                                            to: Route::EventEditScreen { uid: ev.uid.clone() },
                                            style: "flex:1;text-decoration:none;color:inherit;display:flex;gap:10px;align-items:baseline;",
                                            // margin-left:0 overrides `.members li .muted { margin-left:auto }`
                                            // (meant for right-aligned badges), which would otherwise push
                                            // the whole row to the right edge.
                                            span { class: "muted", style: "width:5em;margin-left:0;", "{time_label(&ev)}" }
                                            span { "{ev.title}" }
                                            if !names.is_empty() {
                                                // Here that same margin-left:auto rule is used ON
                                                // PURPOSE: it pushes the names to the row's right edge.
                                                span { class: "muted", style: "font-size:0.85rem;", "{names}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Read-only: dinners are edited on the Ruoka screen.
                        if let Some(m) = meal {
                            li {
                                div { style: "flex:1;display:flex;gap:10px;",
                                    span { class: "muted", style: "width:5em;margin-left:0;", "🍽" }
                                    span { "{m}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
