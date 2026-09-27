//! Kotityöt: today's chores as big tap-to-complete cards (kid-friendly),
//! everyone's turns visible, admin-managed definitions.

use crate::api;
use crate::screens::Route;
use crate::store;
use dioxus::prelude::*;
use perkele_shared::auth::{Role, UserView};
use perkele_shared::chore::{Chore, DueChore, MemberTotal, SaveChoreRequest};
use perkele_shared::recur::{End, Freq, RecurrenceSpec, Weekday};

/// Resolve a member id to a display name; falls back to "?" mid-load.
pub(crate) fn name_of(members: &[UserView], id: i64) -> String {
    members
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| "?".to_owned())
}

/// Leaderboard strip rows: server totals (already sorted by points DESC)
/// followed by zero-point members — kids notice who's missing from a list,
/// so nobody is silently dropped.
fn strip_entries(members: &[UserView], totals: &[MemberTotal]) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = totals
        .iter()
        .map(|t| (name_of(members, t.user_id), t.points))
        .collect();
    for m in members {
        if !totals.iter().any(|t| t.user_id == m.id) {
            out.push((m.display_name.clone(), 0));
        }
    }
    out
}

#[component]
pub fn ChoresScreen() -> Element {
    let mut refresh = use_signal(|| 0u32);
    let mut error = use_signal(|| Option::<String>::None);
    let me = use_resource(api::me);
    let members = use_resource(api::members);
    let nav = use_navigator();

    // Cache-seeded today list; refetch on mount and after every mutation.
    let mut today: Signal<Vec<DueChore>> = use_signal(store::get_chores_today);
    let fetched = use_resource(move || {
        let _ = refresh(); // subscribe: mutations bump this
        async move { api::chores_today().await }
    });
    use_effect(move || {
        if let Some(Ok(list)) = fetched() {
            store::set_chores_today(&list);
            today.set(list);
        }
    });

    // Weekly leaderboard for the strip; refetches after every toggle so the
    // score reacts immediately to a tick.
    let week_stats = use_resource(move || {
        let _ = refresh();
        async move { api::chore_stats("week").await }
    });

    let my = me().and_then(|r| r.ok());
    let my_id = my.as_ref().map(|u| u.id);
    let is_admin = matches!(my.as_ref().map(|u| u.role), Some(Role::Admin));
    let members_list = members().and_then(|r| r.ok()).unwrap_or_default();
    // Undoing a tick is completer-or-admin on the server (it would erase that
    // member's points), so a done chore is only tappable for them. Ticking an
    // open chore stays open to everyone. While `/api/me` is still unknown we
    // don't block the tap — the server decides and the error line shows a 403.
    // `is_none_or` = "true if None, else test the value" (Option combinator).
    let may_toggle = |c: &DueChore| {
        c.done_by
            .is_none_or(|d| my.is_none() || super::can_modify(my.as_ref(), d))
    };

    // A card tap toggles done/undone; the list refetches for truth.
    let toggle = move |c: DueChore| {
        spawn(async move {
            let res = if c.done_by.is_some() {
                api::uncomplete_chore(c.chore_id).await
            } else {
                api::complete_chore(c.chore_id).await
            };
            if let Err(e) = res {
                error.set(Some(e));
            }
            let n = refresh.peek().wrapping_add(1);
            refresh.set(n);
        });
    };

    // Mine first (kid view: my chores are the big cards), then the rest.
    // Whole-family chores count as "mine" — they're anyone's to grab.
    let mut mine: Vec<DueChore> = Vec::new();
    let mut others: Vec<DueChore> = Vec::new();
    for c in today() {
        if c.assignee_id.is_none() || c.assignee_id == my_id {
            mine.push(c);
        } else {
            others.push(c);
        }
    }

    rsx! {
        div { class: "card wide",
            div { class: "row spread",
                h1 { "Kotityöt" }
                if is_admin {
                    button {
                        class: "primary",
                        style: "width:auto;margin:0;",
                        onclick: move |_| {
                            nav.push(Route::ChoreNewScreen {});
                        },
                        "+ Uusi"
                    }
                }
            }
            if let Some(Ok(s)) = week_stats() {
                div { class: "points-strip",
                    span { class: "muted", "Tällä viikolla:" }
                    for (i , (name , pts)) in strip_entries(&members_list, &s.totals).into_iter().enumerate() {
                        {
                            // rsx doesn't like an inline array-index literal inside
                            // an attribute/text position, so the medal lookup is
                            // hoisted out here (top 3 non-zero scores only).
                            let medal = ["🥇", "🥈", "🥉"].get(i).copied().unwrap_or("");
                            rsx! {
                                span { class: "points-entry",
                                    if i < 3 && pts > 0 {
                                        "{medal} "
                                    }
                                    "{name} {pts}"
                                }
                            }
                        }
                    }
                    Link { to: Route::ChoreStatsScreen {}, class: "points-link", "Tilastot →" }
                }
            }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            h2 { "Minun tänään" }
            if mine.is_empty() {
                p { class: "muted", "Ei kotitöitä tänään. 🎉" }
            }
            div { class: "chore-cards",
                for c in mine {
                    {
                        let done = c.done_by.is_some();
                        let family = c.assignee_id.is_none();
                        let can_toggle = may_toggle(&c);
                        let c2 = c.clone();
                        rsx! {
                            button {
                                class: if done { "chore-card done" } else { "chore-card" },
                                // Someone else's tick: shown, but not undoable here.
                                disabled: !can_toggle,
                                onclick: move |_| toggle(c2.clone()),
                                span { class: "chore-check", if done { "✅" } else { "⬜" } }
                                span { class: "chore-title", "{c.title}" }
                                if let Some(d) = c.done_by {
                                    // Who actually ticked it — the whole point
                                    // of a family chore ("kuka vain") card.
                                    span { class: "muted", {name_of(&members_list, d)} }
                                } else if family {
                                    span { class: "muted", "kuka vain" }
                                }
                                span { class: "chore-points", "+{c.points}" }
                            }
                        }
                    }
                }
            }

            if !others.is_empty() {
                h2 { "Muut tänään" }
                ul { class: "members",
                    for c in others {
                        {
                            let done = c.done_by.is_some();
                            // Before completion: whose turn. After: who did it.
                            let who = match c.done_by {
                                Some(d) => name_of(&members_list, d),
                                None => c
                                    .assignee_id
                                    .map(|id| name_of(&members_list, id))
                                    .unwrap_or_default(),
                            };
                            let can_toggle = may_toggle(&c);
                            let c2 = c.clone();
                            rsx! {
                                li {
                                    span {
                                        if done { "✅ " } else { "⬜ " }
                                        "{c.title}"
                                    }
                                    span { class: "muted", "{who}" }
                                    span { class: "chore-points", "+{c.points}" }
                                    // "Peru" only for the completer or an admin.
                                    if can_toggle {
                                        button {
                                            class: "ghost",
                                            style: "margin:0;padding:2px 8px;",
                                            onclick: move |_| toggle(c2.clone()),
                                            if done { "Peru" } else { "Tee" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if is_admin {
                ManagePanel {}
            }
        }
    }
}

/// Admin list of definitions with edit links (the editor is its own route).
#[component]
fn ManagePanel() -> Element {
    let chores = use_resource(api::list_chores);
    rsx! {
        div { class: "panel",
            h2 { "Hallinta" }
            match chores() {
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "muted", "Ei kotitöitä vielä." }
                },
                Some(Ok(list)) => rsx! {
                    ul { class: "members",
                        for c in list {
                            li {
                                Link {
                                    to: Route::ChoreEditScreen { id: c.id },
                                    style: "flex:1;text-decoration:none;color:inherit;",
                                    span { "{c.title}" }
                                }
                                span { class: "muted", "{c.rrule}" }
                            }
                        }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "error", "{e}" }
                },
                None => rsx! {
                    p { class: "muted", "Ladataan…" }
                },
            }
        }
    }
}

#[component]
pub fn ChoreNewScreen() -> Element {
    rsx! {
        ChoreForm { chore: None }
    }
}

#[component]
pub fn ChoreEditScreen(id: i64) -> Element {
    let chores = use_resource(api::list_chores);
    match chores() {
        Some(Ok(list)) => match list.into_iter().find(|c| c.id == id) {
            Some(c) => rsx! {
                ChoreForm { chore: Some(c) }
            },
            None => rsx! {
                div { class: "card",
                    p { "Kotityötä ei löytynyt." }
                }
            },
        },
        Some(Err(e)) => rsx! {
            div { class: "card",
                div { class: "error", "{e}" }
            }
        },
        None => rsx! {
            div { class: "card",
                p { class: "muted", "Ladataan…" }
            }
        },
    }
}

/// Weekday picker order + labels (Finnish week starts Monday).
const WEEKDAYS: [(Weekday, &str); 7] = [
    (Weekday::Mon, "Ma"),
    (Weekday::Tue, "Ti"),
    (Weekday::Wed, "Ke"),
    (Weekday::Thu, "To"),
    (Weekday::Fri, "Pe"),
    (Weekday::Sat, "La"),
    (Weekday::Sun, "Su"),
];

/// Create/edit form. Assignment model: "kuka vain" (neither), one fixed
/// member, or an ordered rotation (tap order = turn order is simplified to
/// member-list order — drag-ordering is YAGNI for a family).
#[component]
fn ChoreForm(chore: Option<Chore>) -> Element {
    let editing = chore.as_ref().map(|c| c.id);
    let spec = chore
        .as_ref()
        .and_then(|c| RecurrenceSpec::from_rrule(&c.rrule));

    let mut title = use_signal(|| chore.as_ref().map(|c| c.title.clone()).unwrap_or_default());
    let mut start_date = use_signal(|| {
        chore
            .as_ref()
            .map(|c| c.start_date.clone())
            .unwrap_or_else(crate::screens::dates::today)
    });
    let mut freq = use_signal(|| spec.as_ref().map(|s| s.freq).unwrap_or(Freq::Daily));
    let mut byday = use_signal(|| spec.as_ref().map(|s| s.byday.clone()).unwrap_or_default());
    let mut assigned = use_signal(|| chore.as_ref().and_then(|c| c.assigned_user_id));
    let mut rotation = use_signal(|| chore.as_ref().and_then(|c| c.rotation.clone()));
    let mut remind_at = use_signal(|| chore.as_ref().and_then(|c| c.remind_at.clone()));
    let mut points = use_signal(|| chore.as_ref().map(|c| c.points).unwrap_or(1));
    let mut error = use_signal(|| Option::<String>::None);

    let members = use_resource(api::members);
    let members_list = members().and_then(|r| r.ok()).unwrap_or_default();
    let nav = use_navigator();

    let save = move |_| {
        let rrule = RecurrenceSpec {
            freq: freq(),
            interval: 1,
            byday: if freq() == Freq::Weekly {
                byday()
            } else {
                Vec::new()
            },
            end: End::Never,
        }
        .to_rrule();
        let req = SaveChoreRequest {
            title: title(),
            rrule,
            start_date: start_date(),
            assigned_user_id: assigned(),
            rotation: rotation(),
            remind_at: remind_at().filter(|t| !t.is_empty()),
            points: Some(points()), // this client always sends an explicit value
        };
        if let Err(m) = perkele_shared::chore::validate_chore(&req) {
            error.set(Some(m.to_owned()));
            return;
        }
        spawn(async move {
            let res = match editing {
                Some(id) => api::update_chore(id, req).await,
                None => api::create_chore(req).await.map(|_| ()),
            };
            match res {
                Ok(()) => {
                    nav.push(Route::ChoresScreen {});
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let delete = move |_| {
        if let Some(id) = editing {
            spawn(async move {
                match api::delete_chore(id).await {
                    Ok(()) => {
                        nav.push(Route::ChoresScreen {});
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };

    rsx! {
        div { class: "card",
            h1 {
                if editing.is_some() { "Muokkaa kotityötä" } else { "Uusi kotityö" }
            }
            label { "Otsikko"
                input {
                    value: "{title}",
                    oninput: move |e| title.set(e.value()),
                }
            }
            label { "Alkaa"
                input {
                    r#type: "date",
                    value: "{start_date}",
                    oninput: move |e| start_date.set(e.value()),
                }
            }
            label { "Toistuu"
                select {
                    onchange: move |e| {
                        freq.set(match e.value().as_str() {
                            "w" => Freq::Weekly,
                            _ => Freq::Daily,
                        });
                    },
                    option { value: "d", selected: freq() == Freq::Daily, "Päivittäin" }
                    option { value: "w", selected: freq() == Freq::Weekly, "Viikoittain" }
                }
            }
            if freq() == Freq::Weekly {
                div { class: "row", style: "gap:4px;flex-wrap:wrap;",
                    for (wd , label) in WEEKDAYS {
                        {
                            let on = byday().contains(&wd);
                            rsx! {
                                button {
                                    class: if on { "primary" } else { "ghost" },
                                    style: "margin:0;padding:4px 8px;width:auto;",
                                    onclick: move |_| {
                                        let mut days = byday();
                                        if on {
                                            days.retain(|d| *d != wd);
                                        } else {
                                            days.push(wd);
                                        }
                                        byday.set(days);
                                    },
                                    "{label}"
                                }
                            }
                        }
                    }
                }
            }

            label { "Vastuu"
                select {
                    onchange: move |e| {
                        match e.value().as_str() {
                            "family" => {
                                assigned.set(None);
                                rotation.set(None);
                            }
                            "rotation" => {
                                assigned.set(None);
                                rotation.set(Some(Vec::new()));
                            }
                            id => {
                                assigned.set(id.parse::<i64>().ok());
                                rotation.set(None);
                            }
                        }
                    },
                    option {
                        value: "family",
                        selected: assigned().is_none() && rotation().is_none(),
                        "Kuka vain"
                    }
                    for m in members_list.iter() {
                        option {
                            value: "{m.id}",
                            selected: assigned() == Some(m.id),
                            "{m.display_name}"
                        }
                    }
                    option { value: "rotation", selected: rotation().is_some(), "Vuorottelu" }
                }
            }
            if rotation().is_some() {
                p { class: "muted", style: "margin:4px 0;",
                    "Valitse vuorottelijat (vuorojärjestys = valintajärjestys):"
                }
                div { class: "row", style: "gap:4px;flex-wrap:wrap;",
                    for m in members_list.iter() {
                        {
                            let id = m.id;
                            let on = rotation().is_some_and(|r| r.contains(&id));
                            let name = m.display_name.clone();
                            rsx! {
                                button {
                                    class: if on { "primary" } else { "ghost" },
                                    style: "margin:0;padding:4px 8px;width:auto;",
                                    onclick: move |_| {
                                        let mut r = rotation().unwrap_or_default();
                                        if on {
                                            r.retain(|x| *x != id);
                                        } else {
                                            r.push(id);
                                        }
                                        rotation.set(Some(r));
                                    },
                                    "{name}"
                                }
                            }
                        }
                    }
                }
            }

            label { "Muistutus (tyhjä = ei muistutusta)"
                input {
                    r#type: "time",
                    value: remind_at().unwrap_or_default(),
                    oninput: move |e| remind_at.set(Some(e.value())),
                }
            }

            label { "Pisteet (1–100)"
                input {
                    r#type: "number",
                    min: "1",
                    max: "100",
                    value: "{points}",
                    // parse::<i64> fails on "" mid-edit; fall back to 1 and let
                    // shared validation catch anything out of range on save.
                    oninput: move |e| points.set(e.value().parse().unwrap_or(1)),
                }
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            button { class: "primary", onclick: save, "Tallenna" }
            if editing.is_some() {
                button { class: "ghost", onclick: delete, "Poista kotityö" }
            }
        }
    }
}
