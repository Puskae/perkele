//! Ilmoitustaulu: the family "fridge door". Cache-seeded list + SSE refetch;
//! posting needs the network (no offline queue — the board is read-mostly).

use crate::api;
use crate::screens::calendar::is_online;
use crate::store;
use dioxus::prelude::*;
use perkele_shared::announcement::{Announcement, validate_body};
use perkele_shared::auth::Role;

/// RFC3339 UTC → local 'D.M. HH:MM' for the byline. The server stores real
/// UTC instants here (unlike the calendar's floating times), so we let the
/// browser convert to the device's timezone.
pub(crate) fn stamp(ts: &str) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(ts));
    if d.get_time().is_nan() {
        return ts.to_owned(); // unparseable — show raw rather than nothing
    }
    format!(
        "{}.{}. {:02}:{:02}",
        d.get_date(),
        d.get_month() + 1, // JS months are 0-based
        d.get_hours(),
        d.get_minutes(),
    )
}

#[component]
pub fn AnnouncementsScreen() -> Element {
    let mut posts: Signal<Vec<Announcement>> = use_signal(store::get_announcements);
    let mut draft = use_signal(String::new);
    // (id, text) of the post being edited in place; None = not editing.
    let mut editing = use_signal(|| Option::<(i64, String)>::None);
    let mut error = use_signal(|| Option::<String>::None);
    // Who am I? Drives the edit/delete/pin visibility.
    let me = use_resource(api::me);

    // refresh() bumps re-run the fetch. This is the app-wide sync tick from
    // NavLayout (bumped by its single SSE stream); own mutations bump it too.
    let mut refresh = use_context::<Signal<u32>>();
    let fetched = use_resource(move || {
        let _ = refresh(); // subscribe
        async move { api::list_announcements().await }
    });
    use_effect(move || {
        if let Some(Ok(list)) = fetched() {
            store::set_announcements(&list);
            posts.set(list);
        }
    });

    let online = is_online();
    let submit = move |_| {
        let text = draft();
        if let Err(m) = validate_body(&text) {
            error.set(Some(m.to_owned()));
            return;
        }
        spawn(async move {
            match api::post_announcement(text).await {
                Ok(_) => {
                    draft.set(String::new());
                    error.set(None);
                    {
                        let n = refresh.peek().wrapping_add(1);
                        refresh.set(n);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let my = me().and_then(|r| r.ok());
    let my_id = my.as_ref().map(|u| u.id);
    let is_admin = matches!(my.as_ref().map(|u| u.role), Some(Role::Admin));

    rsx! {
        div { class: "card wide",
            h1 { "Ilmoitustaulu" }
            textarea {
                class: "board-input",
                placeholder: "Kirjoita ilmoitus perheelle…",
                value: "{draft}",
                oninput: move |e| draft.set(e.value()),
            }
            button {
                class: "primary",
                disabled: !online,
                onclick: submit,
                if online { "Lähetä" } else { "Ei yhteyttä — lähetys vaatii verkon" }
            }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            ul { class: "members",
                for post in posts() {
                    {
                        let mine = my_id == Some(post.created_by);
                        let id = post.id;
                        let pinned = post.pinned;
                        rsx! {
                            li { style: "flex-direction:column;align-items:stretch;gap:4px;",
                                div { class: "row spread",
                                    span {
                                        if pinned { "📌 " }
                                        strong { "{post.author_name}" }
                                        span { class: "muted", " · {stamp(&post.created_at)}" }
                                    }
                                    div { class: "row", style: "gap:6px;",
                                        if is_admin {
                                            button {
                                                class: "ghost", style: "margin:0;padding:2px 8px;",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let _ = api::set_announcement_pinned(id, !pinned).await;
                                                        { let n = refresh.peek().wrapping_add(1); refresh.set(n); }
                                                    });
                                                },
                                                if pinned { "Irrota" } else { "Kiinnitä" }
                                            }
                                        }
                                        if mine || is_admin {
                                            button {
                                                class: "ghost", style: "margin:0;padding:2px 8px;",
                                                onclick: {
                                                    let body = post.body.clone();
                                                    move |_| editing.set(Some((id, body.clone())))
                                                },
                                                "Muokkaa"
                                            }
                                            button {
                                                class: "ghost", style: "margin:0;padding:2px 8px;",
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let _ = api::delete_announcement(id).await;
                                                        { let n = refresh.peek().wrapping_add(1); refresh.set(n); }
                                                    });
                                                },
                                                "Poista"
                                            }
                                        }
                                    }
                                }
                                // In-place edit: swap the body for a textarea.
                                if editing().is_some_and(|(eid, _)| eid == id) {
                                    textarea {
                                        class: "board-input",
                                        value: editing().map(|(_, t)| t).unwrap_or_default(),
                                        oninput: move |e| editing.set(Some((id, e.value()))),
                                    }
                                    div { class: "row", style: "gap:6px;",
                                        button {
                                            class: "primary", style: "width:auto;margin:0;",
                                            onclick: move |_| {
                                                let Some((eid, text)) = editing() else { return };
                                                spawn(async move {
                                                    match api::edit_announcement(eid, text).await {
                                                        Ok(()) => {
                                                            editing.set(None);
                                                            { let n = refresh.peek().wrapping_add(1); refresh.set(n); }
                                                        }
                                                        Err(e) => error.set(Some(e)),
                                                    }
                                                });
                                            },
                                            "Tallenna"
                                        }
                                        button {
                                            class: "ghost", style: "width:auto;margin:0;",
                                            onclick: move |_| editing.set(None),
                                            "Peruuta"
                                        }
                                    }
                                } else {
                                    p { class: "board-body", "{post.body}" }
                                }
                            }
                        }
                    }
                }
            }
            if posts().is_empty() {
                p { class: "muted", "Ei ilmoituksia. Kirjoita ensimmäinen!" }
            }
        }
    }
}
