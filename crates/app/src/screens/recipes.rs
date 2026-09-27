//! Recipe list + create/edit screens.
//!
//! The list view fetches `RecipeSummary` rows. The editor (`RecipeForm`) is
//! shared by the "new" and "edit" routes: it holds the title/metadata fields
//! plus a growable list of ingredient rows, and on save calls create or update.

use crate::api;
use crate::screens::Route;
use dioxus::prelude::*;
use perkele_shared::recipe::{
    RecipeIngredient, SaveRecipeRequest, import_summary_fi, scale_qty, validate_recipe,
    validate_recipe_title,
};

#[component]
pub fn RecipesScreen() -> Element {
    // use_resource re-runs when the screen mounts; fetch the recipe summaries.
    let recipes = use_resource(api::list_recipes);
    let nav = use_navigator();

    rsx! {
        div { class: "card wide",
            div { class: "row spread",
                h1 { "Reseptit" }
                button {
                    class: "primary",
                    style: "width:auto;margin:0;",
                    onclick: move |_| {
                        nav.push(Route::RecipeNewScreen {});
                    },
                    "+ Uusi"
                }
            }
            Link { to: Route::MealPlanScreen {}, class: "switch", "→ Viikkosuunnitelma" }

            match &*recipes.read_unchecked() {
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "muted", "Ei vielä reseptejä." }
                },
                Some(Ok(list)) => rsx! {
                    ul { class: "members",
                        for r in list.iter().cloned() {
                            li { key: "{r.id}",
                                Link {
                                    to: Route::RecipeViewScreen { id: r.id },
                                    style: "flex:1;text-decoration:none;color:inherit;",
                                    span { "{r.title}" }
                                }
                                if let Some(s) = r.servings {
                                    span { class: "muted", "{s} hlö" }
                                }
                            }
                        }
                    }
                },
                Some(Err(e)) => rsx! { div { class: "error", "{e}" } },
                None => rsx! { p { class: "muted", "Ladataan…" } },
            }
        }
    }
}

#[component]
pub fn RecipeNewScreen() -> Element {
    rsx! {
        RecipeForm { id: None }
    }
}

#[component]
pub fn RecipeEditScreen(id: i64) -> Element {
    rsx! {
        RecipeForm { id: Some(id) }
    }
}

/// Read-only recipe view with a servings stepper. The stepper only changes
/// local state: quantities are scaled at render time via shared::scale_qty
/// and nothing is written back to the server.
#[component]
pub fn RecipeViewScreen(id: i64) -> Element {
    let recipe = use_resource(move || api::get_recipe(id));
    let me = use_resource(api::me);
    let nav = use_navigator();
    // None until the user touches the stepper; falls back to the recipe's own
    // servings below. Keeping it Option-al means the default appears as soon
    // as the fetch lands without an effect to sync state.
    let mut chosen = use_signal(|| Option::<i64>::None);
    // Result line under the grocery button, and an in-flight flag so a slow
    // request can't be double-fired.
    let mut grocery_status = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    rsx! {
        div { class: "card wide",
            Link { to: Route::RecipesScreen {}, class: "switch", "← Reseptit" }

            match &*recipe.read_unchecked() {
                Some(Ok(r)) => {
                    // Base servings: the recipe's own, or 4 (family default)
                    // when unset. max(1) guards a hand-entered 0.
                    let base = r.servings.unwrap_or(4).max(1);
                    let sel = chosen().unwrap_or(base).clamp(1, 24);
                    let factor = sel as f64 / base as f64;
                    // Clone out of the resource guard so the rsx block owns
                    // its data instead of borrowing across the render.
                    let r = r.clone();
                    // Editing (and deleting, which lives on the edit form) is
                    // author-or-admin, so only offer "Muokkaa" when allowed.
                    let my = me().and_then(|res| res.ok());
                    let can_edit = super::can_modify(my.as_ref(), r.created_by);
                    rsx! {
                        h1 { "{r.title}" }

                        div { class: "muted",
                            if let Some(p) = r.prep_min {
                                span { "⏱ {p} min valmistelu" }
                            }
                            if r.prep_min.is_some() && r.cook_min.is_some() {
                                span { " + " }
                            }
                            if let Some(c) = r.cook_min {
                                span { "{c} min kypsennys" }
                            }
                        }
                        if let Some(src) = r.source.as_ref() {
                            p { class: "muted", "Lähde: {src}" }
                        }

                        div { class: "row",
                            button {
                                class: "ghost",
                                style: "width:auto;margin:0;",
                                onclick: move |_| {
                                    let cur = chosen().unwrap_or(base);
                                    chosen.set(Some((cur - 1).max(1)));
                                },
                                "−"
                            }
                            span { style: "min-width:4.5em;text-align:center;", "{sel} hlö" }
                            button {
                                class: "ghost",
                                style: "width:auto;margin:0;",
                                onclick: move |_| {
                                    let cur = chosen().unwrap_or(base);
                                    chosen.set(Some((cur + 1).min(24)));
                                },
                                "+"
                            }
                        }

                        h2 {
                            if sel != base {
                                "Ainekset ({base} → {sel})"
                            } else {
                                "Ainekset"
                            }
                        }
                        ul { class: "members",
                            for (idx , ing) in r.ingredients.iter().enumerate() {
                                li { key: "{idx}",
                                    // Name first: `.members li span:first-child` bolds it, and
                                    // `.members li .muted { margin-left: auto }` pushes the
                                    // qty/unit span to the right edge of the flex row.
                                    span { "{ing.name}" }
                                    span { class: "muted",
                                        if let Some(q) = ing.qty.as_ref() {
                                            "{scale_qty(q, factor)} "
                                        }
                                        if let Some(u) = ing.unit.as_ref() {
                                            "{u}"
                                        }
                                    }
                                }
                            }
                        }

                        button {
                            class: "primary",
                            disabled: busy(),
                            onclick: move |_| {
                                // `sel` is Copy (i64) so the move closure
                                // captures the value, not a borrow of the
                                // render's locals.
                                busy.set(true);
                                grocery_status.set(None);
                                spawn(async move {
                                    match api::add_recipe_to_grocery(id, sel).await {
                                        Ok(s) => grocery_status.set(Some(import_summary_fi(&s))),
                                        Err(e) => grocery_status.set(Some(e)),
                                    }
                                    busy.set(false);
                                });
                            },
                            "Lisää ainekset kauppalistaan"
                        }
                        if let Some(s) = grocery_status() {
                            div { class: "code", "{s}" }
                        }

                        if let Some(instructions) = r.instructions.as_ref() {
                            h2 { "Ohjeet" }
                            p { style: "white-space:pre-line;", "{instructions}" }
                        }

                        if can_edit {
                            button {
                                class: "primary",
                                onclick: move |_| {
                                    nav.push(Route::RecipeEditScreen { id });
                                },
                                "Muokkaa"
                            }
                        }
                    }
                }
                Some(Err(e)) => rsx! { div { class: "error", "{e}" } },
                None => rsx! { p { class: "muted", "Ladataan…" } },
            }
        }
    }
}

/// The shared create/edit form. `id == None` means create.
#[component]
fn RecipeForm(id: Option<i64>) -> Element {
    let nav = use_navigator();
    let me = use_resource(api::me);
    // The loaded recipe's author (None on create / until loaded).
    let mut author = use_signal(|| Option::<i64>::None);

    let mut title = use_signal(String::new);
    let mut instructions = use_signal(String::new);
    let mut servings = use_signal(String::new);
    let mut prep = use_signal(String::new);
    let mut cook = use_signal(String::new);
    let mut source = use_signal(String::new);
    // Ingredient rows as (name, qty, unit, category) string tuples.
    let mut rows = use_signal(Vec::<(String, String, String, String)>::new);
    let mut error = use_signal(|| Option::<String>::None);

    // On edit, load the existing recipe once and seed the fields. use_future
    // runs a single time on mount, so no re-seed guard is needed.
    if let Some(rid) = id {
        use_future(move || async move {
            match api::get_recipe(rid).await {
                Ok(r) => {
                    author.set(Some(r.created_by));
                    title.set(r.title);
                    instructions.set(r.instructions.unwrap_or_default());
                    servings.set(r.servings.map(|n| n.to_string()).unwrap_or_default());
                    prep.set(r.prep_min.map(|n| n.to_string()).unwrap_or_default());
                    cook.set(r.cook_min.map(|n| n.to_string()).unwrap_or_default());
                    source.set(r.source.unwrap_or_default());
                    rows.set(
                        r.ingredients
                            .into_iter()
                            .map(|i| {
                                (
                                    i.name,
                                    i.qty.unwrap_or_default(),
                                    i.unit.unwrap_or_default(),
                                    i.category.unwrap_or_default(),
                                )
                            })
                            .collect(),
                    );
                }
                Err(e) => error.set(Some(e)),
            }
        });
    }

    let save = move |_| {
        let req = build_request(
            title(),
            instructions(),
            servings(),
            prep(),
            cook(),
            source(),
            rows(),
        );
        match req {
            Err(msg) => error.set(Some(msg)),
            Ok(req) => {
                error.set(None);
                spawn(async move {
                    let res = match id {
                        Some(rid) => api::update_recipe(rid, req).await,
                        None => api::create_recipe(req).await,
                    };
                    match res {
                        // Land on the read view of what was just saved (the
                        // create response carries the new id).
                        Ok(saved) => {
                            nav.push(Route::RecipeViewScreen { id: saved.id });
                        }
                        Err(e) => error.set(Some(e)),
                    }
                });
            }
        }
    };

    let delete = move |_| {
        if let Some(rid) = id {
            spawn(async move {
                match api::delete_recipe(rid).await {
                    Ok(()) => {
                        nav.push(Route::RecipesScreen {});
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };

    // Only once both the viewer and the recipe's author are known.
    let my = me().and_then(|res| res.ok());
    let read_only = author().is_some_and(|a| my.is_some() && !super::can_modify(my.as_ref(), a));

    rsx! {
        div { class: "card wide",
            h1 { if id.is_some() { "Muokkaa reseptiä" } else { "Uusi resepti" } }

            label { "Nimi" }
            input {
                value: "{title}",
                oninput: move |e| title.set(e.value()),
                placeholder: "esim. Lihapullat",
            }

            div { class: "row",
                div { style: "flex:1;",
                    label { "Annoksia" }
                    input {
                        r#type: "number",
                        value: "{servings}",
                        oninput: move |e| servings.set(e.value()),
                    }
                }
                div { style: "flex:1;",
                    label { "Valmistelu (min)" }
                    input {
                        r#type: "number",
                        value: "{prep}",
                        oninput: move |e| prep.set(e.value()),
                    }
                }
                div { style: "flex:1;",
                    label { "Kypsennys (min)" }
                    input {
                        r#type: "number",
                        value: "{cook}",
                        oninput: move |e| cook.set(e.value()),
                    }
                }
            }

            label { "Ainekset" }
            ul { class: "grocery-list",
                for idx in 0..rows.read().len() {
                    li { key: "{idx}", class: "grocery-item",
                        input {
                            class: "qty-input",
                            placeholder: "määrä",
                            value: "{rows.read()[idx].1}",
                            oninput: move |e| rows.write()[idx].1 = e.value(),
                        }
                        input {
                            class: "unit-input",
                            placeholder: "yks.",
                            value: "{rows.read()[idx].2}",
                            oninput: move |e| rows.write()[idx].2 = e.value(),
                        }
                        input {
                            class: "item-input",
                            placeholder: "aines",
                            value: "{rows.read()[idx].0}",
                            oninput: move |e| rows.write()[idx].0 = e.value(),
                        }
                        input {
                            class: "unit-input",
                            placeholder: "ryhmä",
                            value: "{rows.read()[idx].3}",
                            oninput: move |e| rows.write()[idx].3 = e.value(),
                        }
                        button {
                            class: "del",
                            onclick: move |_| {
                                rows.write().remove(idx);
                            },
                            "✕"
                        }
                    }
                }
            }
            button {
                class: "ghost",
                onclick: move |_| rows.write().push(Default::default()),
                "+ lisää aines"
            }

            label { "Ohjeet" }
            textarea {
                style: "width:100%;min-height:120px;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                value: "{instructions}",
                oninput: move |e| instructions.set(e.value()),
            }

            label { "Lähde / muistiinpano" }
            input {
                value: "{source}",
                oninput: move |e| source.set(e.value()),
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            // Reached by URL on someone else's recipe: the server would 403
            // both buttons, so say why instead of offering them.
            if read_only {
                p { class: "muted", "Vain reseptin lisääjä tai ylläpitäjä voi muokata tai poistaa sen." }
            } else {
                button { class: "primary", onclick: save, "Tallenna" }
                if id.is_some() {
                    button { class: "clear-btn", onclick: delete, "Poista resepti" }
                }
            }
        }
    }
}

/// Pure builder: turn the form's strings into a validated `SaveRecipeRequest`.
/// Empty optional fields become `None`; numbers parse or fail with a message.
fn build_request(
    title: String,
    instructions: String,
    servings: String,
    prep: String,
    cook: String,
    source: String,
    rows: Vec<(String, String, String, String)>,
) -> Result<SaveRecipeRequest, String> {
    validate_recipe_title(&title).map_err(|m| m.to_owned())?;

    // Parse an optional positive integer from a possibly-empty field.
    let opt_num = |s: &str, label: &str| -> Result<Option<i64>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        s.parse::<i64>()
            .map(Some)
            .map_err(|_| format!("{label} pitää olla numero."))
    };

    let blank = |s: String| {
        let t = s.trim().to_owned();
        if t.is_empty() { None } else { Some(t) }
    };

    let ingredients: Vec<RecipeIngredient> = rows
        .into_iter()
        .filter(|(name, ..)| !name.trim().is_empty())
        .map(|(name, qty, unit, category)| RecipeIngredient {
            name: name.trim().to_owned(),
            qty: blank(qty),
            unit: blank(unit),
            category: blank(category),
        })
        .collect();

    let req = SaveRecipeRequest {
        title: title.trim().to_owned(),
        instructions: blank(instructions),
        servings: opt_num(&servings, "Annokset")?,
        prep_min: opt_num(&prep, "Valmisteluaika")?,
        cook_min: opt_num(&cook, "Kypsennysaika")?,
        source: blank(source),
        ingredients,
    };
    // Same full rule set the server enforces (lengths, ingredient count…).
    validate_recipe(&req).map_err(|m| m.to_owned())?;
    Ok(req)
}
