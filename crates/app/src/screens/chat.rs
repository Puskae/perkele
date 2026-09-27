//! Perhechatti: the family chat room. Cache-seeded latest page + SSE-driven
//! refetch; sending needs the network (no offline queue — a chat message
//! replayed hours late confuses more than it helps).

use crate::api;
use crate::screens::announcements::stamp;
use crate::screens::calendar::is_online;
use crate::store;
use dioxus::prelude::*;
use perkele_shared::auth::Role;
use perkele_shared::chat::{ChatMessage, REACTION_EMOJI, validate_body};

#[component]
pub fn ChatScreen() -> Element {
    // Seed from the cache for instant paint; the fetch below replaces it.
    let mut messages: Signal<Vec<ChatMessage>> = use_signal(store::get_chat_messages);
    let mut has_more = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    // Message id whose reaction picker is open; None = all closed.
    let mut picker = use_signal(|| Option::<i64>::None);
    let me = use_resource(api::me);

    // The app-wide chat tick from NavLayout: SSE bumps it, we refetch. We
    // bump it ourselves after our own mutations and after advancing the
    // read marker (so the nav badge clears without waiting for a poke).
    let chat_tick = use_context::<Signal<u32>>();
    let bump = move || {
        let mut chat_tick = chat_tick;
        let n = chat_tick.peek().wrapping_add(1);
        chat_tick.set(n);
    };

    let fetched = use_resource(move || {
        let _ = chat_tick(); // subscribe
        async move { api::chat_page(None, None).await }
    });
    use_effect(move || {
        if let Some(Ok(page)) = fetched() {
            store::set_chat_messages(&page.messages);
            has_more.set(page.has_more);
            // Keep any older pages the user has loaded: everything below the
            // fresh page's first id survives, the fresh page replaces the rest.
            // (Reactions/deletes on those older messages stay stale until
            // remount — acceptable for scrolled-back history.)
            let cut = page.messages.first().map(|m| m.id).unwrap_or(i64::MIN);
            let mut all: Vec<ChatMessage> = messages
                .peek()
                .iter()
                .filter(|m| m.id < cut)
                .cloned()
                .collect();
            all.extend(page.messages.clone());
            messages.set(all);
            // Being on this screen = reading: advance the marker, then bump
            // so the badge refetches. Terminates: the next fetch sees
            // latest == last_read and takes the else-branch.
            if page.latest_id > page.last_read_id {
                spawn(async move {
                    if api::mark_chat_read(page.latest_id).await.is_ok() {
                        bump();
                    }
                });
            }
        }
    });

    let online = is_online();
    // `mut` because calling an FnMut closure (it mutates captured signals)
    // is itself a mutable borrow of the closure value.
    let mut send = move |_: ()| {
        let text = draft();
        if let Err(m) = validate_body(&text) {
            error.set(Some(m.to_owned()));
            return;
        }
        spawn(async move {
            match api::send_chat_message(text).await {
                Ok(_) => {
                    draft.set(String::new());
                    error.set(None);
                    bump();
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let load_older = move |_| {
        let Some(oldest) = messages.peek().first().map(|m| m.id) else {
            return;
        };
        spawn(async move {
            match api::chat_page(Some(oldest), None).await {
                Ok(page) => {
                    has_more.set(page.has_more);
                    let mut all = page.messages;
                    all.extend(messages.peek().iter().cloned());
                    messages.set(all);
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
            h1 { "Perhechatti" }

            // column-reverse container: DOM order is newest→oldest, so the
            // list renders oldest-on-top and stays pinned to the bottom.
            div { class: "chat-log",
                for msg in messages().into_iter().rev() {
                    {
                        let mine = my_id == Some(msg.author_id);
                        let id = msg.id;
                        // (emoji, count, I reacted?) chips, in the fixed order.
                        let chips: Vec<(&'static str, usize, bool)> = REACTION_EMOJI
                            .iter()
                            .filter_map(|&e| {
                                let count = msg.reactions.iter().filter(|r| r.emoji == e).count();
                                let own = msg
                                    .reactions
                                    .iter()
                                    .any(|r| r.emoji == e && Some(r.user_id) == my_id);
                                (count > 0).then_some((e, count, own))
                            })
                            .collect();
                        rsx! {
                            div { class: if mine { "chat-msg mine" } else { "chat-msg" },
                                div { class: "chat-meta",
                                    strong { "{msg.author_name}" }
                                    span { " · {stamp(&msg.created_at)}" }
                                    if mine || is_admin {
                                        button {
                                            class: "ghost",
                                            style: "margin:0 0 0 8px;padding:0 6px;font-size:0.7rem;",
                                            onclick: move |_| {
                                                spawn(async move {
                                                    let _ = api::delete_chat_message(id).await;
                                                    bump();
                                                });
                                            },
                                            "Poista"
                                        }
                                    }
                                }
                                p {
                                    class: "chat-body",
                                    onclick: move |_| {
                                        let open = picker.peek().is_some_and(|p| p == id);
                                        picker.set(if open { None } else { Some(id) });
                                    },
                                    "{msg.body}"
                                }
                                if !chips.is_empty() || picker() == Some(id) {
                                    div { class: "chat-reactions",
                                        // Existing reactions as toggleable chips…
                                        for (emoji, count, own) in chips {
                                            button {
                                                class: if own { "chat-chip own" } else { "chat-chip" },
                                                onclick: move |_| {
                                                    spawn(async move {
                                                        let _ = api::toggle_chat_reaction(id, emoji.to_owned()).await;
                                                        bump();
                                                    });
                                                },
                                                "{emoji} {count}"
                                            }
                                        }
                                        // …plus the full picker when open.
                                        if picker() == Some(id) {
                                            for emoji in REACTION_EMOJI {
                                                button {
                                                    class: "chat-chip",
                                                    onclick: move |_| {
                                                        picker.set(None);
                                                        spawn(async move {
                                                            let _ = api::toggle_chat_reaction(id, emoji.to_owned()).await;
                                                            bump();
                                                        });
                                                    },
                                                    "{emoji}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Last DOM child of a column-reverse container = visual TOP.
                if has_more() {
                    button { class: "ghost chat-older", onclick: load_older, "Näytä vanhemmat" }
                }
            }

            if messages().is_empty() {
                p { class: "muted", "Ei viestejä vielä. Aloita perhekaaos!" }
            }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            div { class: "chat-send",
                input {
                    placeholder: if online { "Kirjoita viesti…" } else { "Ei yhteyttä — lähetys vaatii verkon" },
                    value: "{draft}",
                    disabled: !online,
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            send(());
                        }
                    },
                }
                button { class: "primary", disabled: !online, onclick: move |_| send(()), "Lähetä" }
            }
        }
    }
}
