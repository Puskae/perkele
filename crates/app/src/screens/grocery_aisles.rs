//! Aisle editor ("Hyllyt"): add, rename, delete (admin only), and reorder
//! the family's store aisles. Reordering = walking order through the store. Online-only:
//! aisles need server-assigned ids, same restriction as adding a grocery
//! item offline.

use crate::api;
use dioxus::prelude::*;
use perkele_shared::aisle::{AislesResponse, validate_aisle_name};

#[component]
pub fn AisleEditor(
    aisles_data: Signal<Option<AislesResponse>>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    // id of the aisle being renamed + the draft text.
    let mut renaming: Signal<Option<(i64, String)>> = use_signal(|| None);

    let aisles = aisles_data().map(|d| d.aisles).unwrap_or_default();
    // Deleting an aisle is admin-only on the server (aisles have no author,
    // and a delete drops every learned item→aisle mapping). The editor is
    // online-only, so `me` is normally known; until it loads, hide the ✕.
    let me = use_resource(api::me);
    let is_admin = me().and_then(|r| r.ok()).is_some_and(|u| u.role.is_admin());

    let mut add = move || {
        let name = new_name().trim().to_owned();
        if let Err(e) = validate_aisle_name(&name) {
            error.set(Some(e.to_owned()));
            return;
        }
        error.set(None);
        spawn(async move {
            match api::create_aisle(&name).await {
                Ok(_) => {
                    error.set(None);
                    new_name.set(String::new());
                    on_changed.call(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    // Swap the aisle at `idx` with its neighbor and send the FULL new order.
    // Reads the id list from the signal at call time so the closure captures
    // only Copy signals — every row's buttons can then carry their own copy
    // (a captured Vec would make the closure non-Copy and it wouldn't compile
    // across the loop). `.peek()` avoids subscribing this closure to the
    // signal (same trick as apply_sync in grocery.rs).
    let move_by = move |idx: usize, delta: i64| {
        let mut order: Vec<i64> = aisles_data
            .peek()
            .as_ref()
            .map(|d| d.aisles.iter().map(|a| a.id).collect())
            .unwrap_or_default();
        let target = idx as i64 + delta;
        if target < 0 || target as usize >= order.len() {
            return;
        }
        order.swap(idx, target as usize);
        spawn(async move {
            match api::reorder_aisles(order).await {
                Ok(_) => {
                    error.set(None);
                    on_changed.call(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    rsx! {
        div { class: "aisle-editor",
            h2 { "Hyllyt" }
            p { class: "muted", "Järjestä hyllyt siihen järjestykseen kuin kuljet kaupassa." }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            ul { class: "aisle-editor-list",
                for (idx, a) in aisles.iter().cloned().enumerate() {
                    li { key: "{a.id}", class: "aisle-editor-row",
                        if renaming().is_some_and(|(id, _)| id == a.id) {
                            input {
                                class: "item-input",
                                value: "{renaming().map(|(_, d)| d).unwrap_or_default()}",
                                autofocus: true,
                                oninput: move |e| {
                                    if let Some((id, _)) = renaming() {
                                        renaming.set(Some((id, e.value())));
                                    }
                                },
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter
                                        && let Some((id, draft)) = renaming()
                                    {
                                        let name = draft.trim().to_owned();
                                        if let Err(err) = validate_aisle_name(&name) {
                                            error.set(Some(err.to_owned()));
                                            return;
                                        }
                                        spawn(async move {
                                            match api::rename_aisle(id, &name).await {
                                                Ok(_) => {
                                                    renaming.set(None);
                                                    error.set(None);
                                                    on_changed.call(());
                                                }
                                                Err(e) => error.set(Some(e)),
                                            }
                                        });
                                    }
                                },
                            }
                        } else {
                            span {
                                class: "aisle-name",
                                onclick: {
                                    let name = a.name.clone();
                                    move |_| renaming.set(Some((a.id, name.clone())))
                                },
                                "{a.name}"
                            }
                        }
                        button { class: "aisle-move", disabled: idx == 0,
                            onclick: move |_| move_by(idx, -1), "↑" }
                        button { class: "aisle-move", disabled: idx == aisles.len() - 1,
                            onclick: move |_| move_by(idx, 1), "↓" }
                        if is_admin {
                            button { class: "del", title: "Poista hylly",
                                onclick: move |_| {
                                    let confirmed = web_sys::window()
                                        .map(|w| w.confirm_with_message(
                                            "Poistetaanko hylly? Sen tuotteet siirtyvät kohtaan Lajittelematon.")
                                            .unwrap_or(false))
                                        .unwrap_or(false);
                                    if !confirmed { return; }
                                    spawn(async move {
                                        match api::delete_aisle(a.id).await {
                                            Ok(_) => {
                                                error.set(None);
                                                on_changed.call(());
                                            }
                                            Err(e) => error.set(Some(e)),
                                        }
                                    });
                                },
                                "✕" }
                        }
                    }
                }
            }
            div { class: "add-form",
                input {
                    class: "item-input", placeholder: "Uusi hylly…", value: "{new_name}",
                    oninput: move |e| new_name.set(e.value()),
                    onkeydown: move |e| { if e.key() == Key::Enter { add(); } },
                }
                button { class: "primary", onclick: move |_| add(), "Lisää" }
            }
        }
    }
}
