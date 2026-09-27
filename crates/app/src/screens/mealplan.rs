//! Dinner-per-day week planner.
//!
//! Shows 7 days (Mon–Sun); each day has a dropdown of the family's recipes so
//! you can assign that night's dinner (or clear it). The "Lisää viikko
//! ostoslistalle" button calls the grocery-glue endpoint and reports the
//! import summary (added / already listed / revived staples).
//!
//! Free-text dinners are supported by the server but not yet surfaced here —
//! this first cut only assigns recipes, which is all the grocery import reads.

use crate::api;
use crate::screens::dates::{current_monday, shift_day};
use dioxus::prelude::*;
use perkele_shared::recipe::{SetMealRequest, import_summary_fi};

const WEEKDAYS: [&str; 7] = ["Ma", "Ti", "Ke", "To", "Pe", "La", "Su"];

#[component]
pub fn MealPlanScreen() -> Element {
    let mut monday = use_signal(current_monday);
    // Bump this to force the week resource to refetch after an edit.
    let mut refresh = use_signal(|| 0u32);
    let mut status = use_signal(|| Option::<String>::None);

    let recipes = use_resource(api::list_recipes);
    let week = use_resource(move || {
        let m = monday();
        let _ = refresh(); // subscribe so edits trigger a refetch
        async move { api::get_week(&m).await }
    });

    let import = move |_| {
        let m = monday();
        spawn(async move {
            match api::add_week_to_grocery(&m).await {
                Ok(s) => status.set(Some(import_summary_fi(&s))),
                Err(e) => status.set(Some(e)),
            }
        });
    };

    rsx! {
        div { class: "card wide",
            h1 { "Viikkosuunnitelma" }

            div { class: "row spread",
                button {
                    class: "ghost",
                    onclick: move |_| {
                        let prev = shift_day(&monday(), -7);
                        monday.set(prev);
                    },
                    "‹ Edellinen"
                }
                span { class: "muted", "{monday}" }
                button {
                    class: "ghost",
                    onclick: move |_| {
                        let next = shift_day(&monday(), 7);
                        monday.set(next);
                    },
                    "Seuraava ›"
                }
            }

            match (&*week.read_unchecked(), &*recipes.read_unchecked()) {
                (Some(Ok(w)), Some(Ok(rlist))) => {
                    let rlist = rlist.clone();
                    rsx! {
                        ul { class: "members",
                            for (i , entry) in w.entries.iter().cloned().enumerate() {
                                li { key: "{entry.date}",
                                    span { style: "width:2.5em;font-weight:600;", "{WEEKDAYS[i]}" }
                                    select {
                                        style: "flex:1;",
                                        onchange: {
                                            let date = entry.date.clone();
                                            move |e: Event<FormData>| {
                                                let date = date.clone();
                                                let val = e.value();
                                                spawn(async move {
                                                    let _ = if val.is_empty() {
                                                        api::clear_meal(&date).await
                                                    } else if let Ok(rid) = val.parse::<i64>() {
                                                        api::set_meal(
                                                                &date,
                                                                SetMealRequest {
                                                                    recipe_id: Some(rid),
                                                                    free_text: None,
                                                                },
                                                            )
                                                            .await
                                                    } else {
                                                        Ok(())
                                                    };
                                                    refresh.set(refresh() + 1);
                                                });
                                            }
                                        },
                                        option {
                                            value: "",
                                            selected: entry.recipe_id.is_none(),
                                            "—"
                                        }
                                        for r in rlist.iter().cloned() {
                                            option {
                                                value: "{r.id}",
                                                selected: entry.recipe_id == Some(r.id),
                                                "{r.title}"
                                            }
                                        }
                                    }
                                    if let Some(ft) = &entry.free_text {
                                        span { class: "muted", "{ft}" }
                                    }
                                }
                            }
                        }
                    }
                }
                (Some(Err(e)), _) => rsx! {
                    div { class: "error", "{e}" }
                },
                (_, Some(Err(e))) => rsx! {
                    div { class: "error", "{e}" }
                },
                _ => rsx! {
                    p { class: "muted", "Ladataan…" }
                },
            }

            if let Some(s) = status() {
                div { class: "code", "{s}" }
            }
            button { class: "primary", onclick: import, "Lisää viikko ostoslistalle" }
        }
    }
}
