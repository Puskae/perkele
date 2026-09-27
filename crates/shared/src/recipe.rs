use serde::{Deserialize, Serialize};

/// One structured ingredient row. Mirrors the grocery item shape so the meal
/// planner can copy ingredients straight into the grocery list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeIngredient {
    pub name: String,
    pub qty: Option<String>,
    pub unit: Option<String>,
    pub category: Option<String>,
}

/// A full recipe with its ingredients (returned by GET /api/recipes/:id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    pub id: i64,
    pub family_id: i64,
    pub title: String,
    pub instructions: Option<String>,
    pub servings: Option<i64>,
    pub prep_min: Option<i64>,
    pub cook_min: Option<i64>,
    pub source: Option<String>,
    pub created_by: i64,
    pub created_at: String,
    pub updated_at: String,
    pub ingredients: Vec<RecipeIngredient>,
}

/// Lightweight list-view row (no ingredients) for GET /api/recipes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeSummary {
    pub id: i64,
    pub title: String,
    pub servings: Option<i64>,
    pub prep_min: Option<i64>,
    pub cook_min: Option<i64>,
}

/// Body for both POST (create) and PUT (update). Ingredients are replace-all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveRecipeRequest {
    pub title: String,
    pub instructions: Option<String>,
    pub servings: Option<i64>,
    pub prep_min: Option<i64>,
    pub cook_min: Option<i64>,
    pub source: Option<String>,
    pub ingredients: Vec<RecipeIngredient>,
}

pub fn validate_recipe_title(title: &str) -> Result<(), &'static str> {
    let len = title.trim().chars().count();
    if len == 0 {
        return Err("Reseptin nimi ei voi olla tyhjä.");
    }
    if len > 100 {
        return Err("Reseptin nimi saa olla enintään 100 merkkiä.");
    }
    Ok(())
}

pub const INSTRUCTIONS_MAX: usize = 20_000;
pub const SOURCE_MAX: usize = 500;
pub const INGREDIENTS_MAX: usize = 200;
pub const CATEGORY_MAX: usize = 64;
pub const MEAL_FREE_TEXT_MAX: usize = 200;

fn too_long(s: Option<&str>, max: usize) -> bool {
    s.is_some_and(|s| s.chars().count() > max)
}

/// Full recipe rule set (create + update + the startup seeder): title, and
/// per ingredient the grocery item rules — the import copies ingredients
/// straight into the grocery list, so they must be valid grocery items.
pub fn validate_recipe(req: &SaveRecipeRequest) -> Result<(), &'static str> {
    validate_recipe_title(&req.title)?;
    if too_long(req.instructions.as_deref(), INSTRUCTIONS_MAX) {
        return Err("Ohje saa olla enintään 20000 merkkiä.");
    }
    if too_long(req.source.as_deref(), SOURCE_MAX) {
        return Err("Lähde saa olla enintään 500 merkkiä.");
    }
    if req.ingredients.len() > INGREDIENTS_MAX {
        return Err("Reseptissä saa olla enintään 200 ainesosaa.");
    }
    for ing in &req.ingredients {
        crate::grocery::validate_grocery_item_name(&ing.name)?;
        crate::grocery::validate_qty_unit(ing.qty.as_deref(), ing.unit.as_deref())?;
        if too_long(ing.category.as_deref(), CATEGORY_MAX) {
            return Err("Ainesosan ryhmä saa olla enintään 64 merkkiä.");
        }
    }
    Ok(())
}

/// Body rules for PUT /api/mealplan/{date} (the date itself is checked with
/// [`crate::dates::validate_date`]).
pub fn validate_meal(req: &SetMealRequest) -> Result<(), &'static str> {
    if too_long(req.free_text.as_deref(), MEAL_FREE_TEXT_MAX) {
        return Err("Ateria saa olla enintään 200 merkkiä.");
    }
    Ok(())
}

/// One day's planned dinner. `recipe_id`+`recipe_title` set when a recipe is
/// chosen; `free_text` set when the user typed a meal name instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MealPlanEntry {
    pub date: String, // 'YYYY-MM-DD'
    pub recipe_id: Option<i64>,
    pub recipe_title: Option<String>,
    pub free_text: Option<String>,
}

/// A week of dinners: 7 entries, Monday-first, keyed by date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MealPlanWeek {
    pub week_start: String, // Monday 'YYYY-MM-DD'
    pub entries: Vec<MealPlanEntry>,
}

/// Body for PUT /api/mealplan/:date.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetMealRequest {
    pub recipe_id: Option<i64>,
    pub free_text: Option<String>,
}

/// Combine ingredients for the grocery import, deduping by (lowercased name,
/// unit). For each group: if every `qty` parses as f64, sum them and format
/// without trailing zeros; otherwise join the distinct quantity strings with
/// " + " so nothing is silently dropped. `category` is taken from the first row
/// in the group that has one. Output order follows first appearance.
pub fn merge_ingredients(ingredients: Vec<RecipeIngredient>) -> Vec<RecipeIngredient> {
    use std::collections::HashMap;

    // Preserve insertion order while grouping: keep a list of keys + a map.
    let mut order: Vec<(String, Option<String>)> = Vec::new();
    let mut groups: HashMap<(String, Option<String>), Vec<RecipeIngredient>> = HashMap::new();

    for ing in ingredients {
        let key = (ing.name.trim().to_lowercase(), ing.unit.clone());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(ing);
    }

    order
        .into_iter()
        .map(|key| {
            let rows = groups.remove(&key).unwrap();
            let name = rows[0].name.trim().to_owned();
            let unit = rows[0].unit.clone();
            let category = rows.iter().find_map(|r| r.category.clone());

            // Collect each row's qty (None / blank treated as absent, not "0").
            let qtys: Vec<String> = rows
                .iter()
                .filter_map(|r| r.qty.clone())
                .filter(|q| !q.trim().is_empty())
                .collect();

            let qty = if qtys.is_empty() {
                None
            } else if let Some(sum) = sum_numeric(&qtys) {
                Some(sum)
            } else {
                // Unmergeable: join distinct values preserving order.
                let mut seen = Vec::new();
                for q in &qtys {
                    if !seen.contains(q) {
                        seen.push(q.clone());
                    }
                }
                Some(seen.join(" + "))
            };

            RecipeIngredient {
                name,
                qty,
                unit,
                category,
            }
        })
        .collect()
}

/// Sum a list of quantity strings if and only if every one parses as f64.
/// Formats the result without a trailing ".0" (so 7.0 -> "7", 1.5 -> "1.5").
fn sum_numeric(qtys: &[String]) -> Option<String> {
    let mut total = 0.0_f64;
    for q in qtys {
        let n: f64 = q.trim().parse().ok()?;
        total += n;
    }
    let s = if total.fract() == 0.0 {
        format!("{}", total as i64)
    } else {
        format!("{total}")
    };
    Some(s)
}

/// Scale a quantity string by `factor` for the recipe view's servings
/// stepper. Numeric quantities are multiplied and formatted compactly
/// (rounded to 2 decimals, trailing zeros trimmed, so 2×1.5 -> "3" and
/// 0.5×1.5 -> "0.75"). Non-numeric quantities like "ripaus" — and empty
/// strings — are returned unchanged rather than mangled.
pub fn scale_qty(qty: &str, factor: f64) -> String {
    // Same lenient parse as sum_numeric: if it isn't a plain number, don't
    // touch it. `else` on a let (let-else) exits early when parse fails.
    let Ok(n) = qty.trim().parse::<f64>() else {
        return qty.to_owned();
    };
    // Round to 2 decimals: scale into hundredths, round, scale back.
    let rounded = (n * factor * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        // {:.2} always prints two decimals ("2.50"); trim the padding zeros
        // and a bare trailing dot so "2.50" -> "2.5".
        format!("{rounded:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

/// Counts returned by the grocery-import endpoints and shown to the user:
/// `added` = new rows inserted, `skipped` = an unchecked item with the same
/// name was already on the list, `unchecked` = a checked staple was revived
/// by unchecking it instead of duplicating it. Lives in `shared` so the
/// server serializes and the client deserializes the exact same shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImportSummary {
    pub added: i64,
    pub skipped: i64,
    pub unchecked: i64,
}

/// Round a quantity UP to a whole number, but only for "countable" units —
/// no unit at all, blank, or "kpl". You can't buy 2.5 onions, but 2.5 dl of
/// cream is a perfectly good shopping quantity, so measurable units pass
/// through untouched. Non-numeric quantities ("ripaus", "1-2") also pass
/// through, same lenient policy as `scale_qty`.
pub fn ceil_countable(qty: &str, unit: Option<&str>) -> String {
    let countable = match unit {
        None => true,
        Some(u) => {
            let u = u.trim();
            // "kpl" is ASCII, so the cheap ASCII comparison is enough here.
            u.is_empty() || u.eq_ignore_ascii_case("kpl")
        }
    };
    if !countable {
        return qty.to_owned();
    }
    let Ok(n) = qty.trim().parse::<f64>() else {
        return qty.to_owned();
    };
    format!("{}", n.ceil() as i64)
}

/// The full recipe→grocery pipeline: scale every quantity, merge duplicate
/// ingredient rows, then round countable sums up. Order matters — rounding
/// runs AFTER merging so a sum is ceiled once (0.5 + 0.5 sipulia merges to
/// 1, not to 2 via per-row ceils). The week import calls this with factor
/// 1.0: scaling is then a no-op but the round-up still applies.
pub fn prepare_for_grocery(
    ingredients: Vec<RecipeIngredient>,
    factor: f64,
) -> Vec<RecipeIngredient> {
    let scaled: Vec<RecipeIngredient> = ingredients
        .into_iter()
        .map(|mut ing| {
            // Option::map consumes the Some value and rebuilds it — a tidy
            // way to transform "qty if present" without an if-let dance.
            ing.qty = ing.qty.map(|q| scale_qty(&q, factor));
            ing
        })
        .collect();

    merge_ingredients(scaled)
        .into_iter()
        .map(|mut ing| {
            ing.qty = ing.qty.map(|q| ceil_countable(&q, ing.unit.as_deref()));
            ing
        })
        .collect()
}

/// Human-readable Finnish one-liner for an import result, shown under the
/// button. Zero segments are omitted; if nothing was added or revived, a
/// friendlier "everything's already there" message is used instead.
pub fn import_summary_fi(s: &ImportSummary) -> String {
    if s.added == 0 && s.skipped == 0 && s.unchecked == 0 {
        // Nothing was even attempted (empty week / recipe without
        // ingredients) — "everything is already listed" would be a lie here.
        return "Ei lisättävää.".to_owned();
    }
    if s.added == 0 && s.unchecked == 0 {
        return "Kaikki ainekset ovat jo listalla.".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    if s.added > 0 {
        parts.push(format!("lisätty {}", s.added));
    }
    if s.skipped > 0 {
        parts.push(format!("jo listalla {}", s.skipped));
    }
    if s.unchecked > 0 {
        parts.push(format!("palautettu listalle {}", s.unchecked));
    }
    let joined = parts.join(" · ");
    // Capitalize the first letter. char-based (not byte-based) so a leading
    // non-ASCII letter would also upper-case correctly.
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_title_passes() {
        assert!(validate_recipe_title("Lihapullat").is_ok());
        assert!(validate_recipe_title(&"x".repeat(100)).is_ok());
    }

    #[test]
    fn empty_title_is_rejected() {
        assert!(validate_recipe_title("").is_err());
        assert!(validate_recipe_title("   ").is_err());
    }

    #[test]
    fn overlong_title_is_rejected() {
        assert!(validate_recipe_title(&"x".repeat(101)).is_err());
    }

    fn ing(name: &str, qty: Option<&str>, unit: Option<&str>) -> RecipeIngredient {
        RecipeIngredient {
            name: name.into(),
            qty: qty.map(|s| s.into()),
            unit: unit.map(|s| s.into()),
            category: None,
        }
    }

    #[test]
    fn merge_sums_same_name_and_unit() {
        let out = merge_ingredients(vec![
            ing("kerma", Some("2"), Some("dl")),
            ing("Kerma", Some("5"), Some("dl")),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kerma");
        assert_eq!(out[0].qty.as_deref(), Some("7"));
        assert_eq!(out[0].unit.as_deref(), Some("dl"));
    }

    #[test]
    fn merge_keeps_different_units_separate() {
        let out = merge_ingredients(vec![
            ing("maito", Some("2"), Some("dl")),
            ing("maito", Some("1"), Some("l")),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_joins_unparseable_quantities() {
        let out = merge_ingredients(vec![
            ing("suola", Some("ripaus"), None),
            ing("suola", Some("2"), None),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty.as_deref(), Some("ripaus + 2"));
    }

    #[test]
    fn merge_handles_missing_quantities() {
        let out = merge_ingredients(vec![ing("pippuri", None, None), ing("pippuri", None, None)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty, None);
    }

    #[test]
    fn merge_fractional_sum() {
        let out = merge_ingredients(vec![
            ing("öljy", Some("0.5"), Some("dl")),
            ing("öljy", Some("1"), Some("dl")),
        ]);
        assert_eq!(out[0].qty.as_deref(), Some("1.5"));
    }

    #[test]
    fn scale_qty_scales_integers() {
        assert_eq!(scale_qty("2", 1.5), "3");
        assert_eq!(scale_qty("400", 0.25), "100");
    }

    #[test]
    fn scale_qty_scales_fractions() {
        assert_eq!(scale_qty("0.5", 1.5), "0.75");
        assert_eq!(scale_qty("1.5", 2.0), "3");
    }

    #[test]
    fn scale_qty_rounds_to_two_decimals_and_trims_zeros() {
        // 1 × ⅓ = 0.333… → rounded to 0.33
        assert_eq!(scale_qty("1", 1.0 / 3.0), "0.33");
        // 2 × 1.25 = 2.50 → trailing zero trimmed
        assert_eq!(scale_qty("2", 1.25), "2.5");
    }

    #[test]
    fn scale_qty_identity_factor_keeps_value() {
        assert_eq!(scale_qty("7", 1.0), "7");
        assert_eq!(scale_qty("0.5", 1.0), "0.5");
    }

    #[test]
    fn scale_qty_passes_non_numeric_through() {
        assert_eq!(scale_qty("ripaus", 2.0), "ripaus");
        assert_eq!(scale_qty("1-2", 3.0), "1-2");
        assert_eq!(scale_qty("", 2.0), "");
        assert_eq!(scale_qty("   ", 2.0), "   ");
    }

    #[test]
    fn recipe_serde_round_trip() {
        let r = Recipe {
            id: 1,
            family_id: 1,
            title: "Lihapullat".into(),
            instructions: Some("Sekoita".into()),
            servings: Some(4),
            prep_min: Some(15),
            cook_min: Some(30),
            source: None,
            created_by: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            ingredients: vec![RecipeIngredient {
                name: "jauheliha".into(),
                qty: Some("400".into()),
                unit: Some("g".into()),
                category: None,
            }],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Recipe = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn ceil_countable_rounds_up_unitless_and_kpl() {
        assert_eq!(ceil_countable("2.5", None), "3");
        assert_eq!(ceil_countable("2.5", Some("kpl")), "3");
        assert_eq!(ceil_countable("2.5", Some("KPL")), "3");
        assert_eq!(ceil_countable("2.5", Some("")), "3");
        assert_eq!(ceil_countable("2.5", Some("  kpl ")), "3");
    }

    #[test]
    fn ceil_countable_leaves_measurable_units_alone() {
        assert_eq!(ceil_countable("2.5", Some("dl")), "2.5");
        assert_eq!(ceil_countable("375", Some("g")), "375");
        assert_eq!(ceil_countable("0.75", Some("l")), "0.75");
    }

    #[test]
    fn ceil_countable_passes_non_numeric_and_whole_through() {
        assert_eq!(ceil_countable("ripaus", None), "ripaus");
        assert_eq!(ceil_countable("1-2", Some("kpl")), "1-2");
        assert_eq!(ceil_countable("3", None), "3");
    }

    #[test]
    fn prepare_scales_and_rounds_countables_only() {
        // Factor 1.5 (4 → 6 servings): sipuli 1 → 1.5 → ceil 2 (countable),
        // kerma 2 dl → 3 dl exact (measurable units are never ceiled).
        let out = prepare_for_grocery(
            vec![
                ing("sipuli", Some("1"), None),
                ing("kerma", Some("2"), Some("dl")),
            ],
            1.5,
        );
        assert_eq!(out[0].qty.as_deref(), Some("2"));
        assert_eq!(out[1].qty.as_deref(), Some("3"));
    }

    #[test]
    fn prepare_rounds_after_merging_not_per_row() {
        // Two half-onion rows must merge to 1, NOT become 1 + 1 = 2 via
        // per-row ceils. This is the test that pins the pipeline ORDER —
        // ceiling before merging would fail it.
        let out = prepare_for_grocery(
            vec![
                ing("sipuli", Some("0.5"), None),
                ing("sipuli", Some("0.5"), None),
            ],
            1.0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty.as_deref(), Some("1"));
    }

    #[test]
    fn prepare_factor_one_still_rounds_countables() {
        // The week import uses factor 1.0 — a recipe written with "2.5
        // sipulia" must still land on the list as 3.
        let out = prepare_for_grocery(vec![ing("sipuli", Some("2.5"), None)], 1.0);
        assert_eq!(out[0].qty.as_deref(), Some("3"));
    }

    #[test]
    fn summary_fi_orders_added_skipped_unchecked() {
        let s = ImportSummary {
            added: 5,
            skipped: 2,
            unchecked: 1,
        };
        assert_eq!(
            import_summary_fi(&s),
            "Lisätty 5 · jo listalla 2 · palautettu listalle 1"
        );
    }

    #[test]
    fn summary_fi_omits_zero_segments_and_capitalizes_first() {
        assert_eq!(
            import_summary_fi(&ImportSummary {
                added: 3,
                skipped: 0,
                unchecked: 0
            }),
            "Lisätty 3"
        );
        assert_eq!(
            import_summary_fi(&ImportSummary {
                added: 0,
                skipped: 1,
                unchecked: 2
            }),
            "Jo listalla 1 · palautettu listalle 2"
        );
    }

    #[test]
    fn summary_fi_nothing_new_message() {
        assert_eq!(
            import_summary_fi(&ImportSummary {
                added: 0,
                skipped: 4,
                unchecked: 0
            }),
            "Kaikki ainekset ovat jo listalla."
        );
    }

    #[test]
    fn summary_fi_all_zeros_means_nothing_to_add() {
        // A week with no planned meals (or a recipe with no ingredients)
        // imports nothing at all — claiming "everything is already on the
        // list" would be misleading, so this case gets its own message.
        assert_eq!(
            import_summary_fi(&ImportSummary {
                added: 0,
                skipped: 0,
                unchecked: 0
            }),
            "Ei lisättävää."
        );
    }

    fn recipe_req() -> SaveRecipeRequest {
        SaveRecipeRequest {
            title: "Lohikeitto".into(),
            instructions: Some("Keitä.".into()),
            servings: Some(4),
            prep_min: Some(15),
            cook_min: Some(20),
            source: Some("Mummo".into()),
            ingredients: vec![RecipeIngredient {
                name: "lohi".into(),
                qty: Some("400".into()),
                unit: Some("g".into()),
                category: Some("kala".into()),
            }],
        }
    }

    #[test]
    fn validate_recipe_limits() {
        assert!(validate_recipe(&recipe_req()).is_ok());

        let mut r = recipe_req();
        r.instructions = Some("x".repeat(INSTRUCTIONS_MAX));
        assert!(validate_recipe(&r).is_ok());
        r.instructions = Some("x".repeat(INSTRUCTIONS_MAX + 1));
        assert!(validate_recipe(&r).is_err());

        let mut r = recipe_req();
        r.source = Some("x".repeat(SOURCE_MAX + 1));
        assert!(validate_recipe(&r).is_err());

        let mut r = recipe_req();
        let one = r.ingredients[0].clone();
        r.ingredients = vec![one.clone(); INGREDIENTS_MAX];
        assert!(validate_recipe(&r).is_ok());
        r.ingredients.push(one);
        assert!(validate_recipe(&r).is_err());

        let mut r = recipe_req();
        r.ingredients[0].qty = Some("9".repeat(33));
        assert!(validate_recipe(&r).is_err());
        let mut r = recipe_req();
        r.ingredients[0].unit = Some("x".repeat(33));
        assert!(validate_recipe(&r).is_err());
        let mut r = recipe_req();
        r.ingredients[0].category = Some("x".repeat(CATEGORY_MAX + 1));
        assert!(validate_recipe(&r).is_err());
        let mut r = recipe_req();
        r.ingredients[0].name = " ".into();
        assert!(validate_recipe(&r).is_err());
        let mut r = recipe_req();
        r.title = String::new();
        assert!(validate_recipe(&r).is_err());
    }

    #[test]
    fn validate_meal_limits_free_text() {
        let ok = SetMealRequest {
            recipe_id: None,
            free_text: Some("x".repeat(MEAL_FREE_TEXT_MAX)),
        };
        assert!(validate_meal(&ok).is_ok());
        let bad = SetMealRequest {
            recipe_id: None,
            free_text: Some("x".repeat(MEAL_FREE_TEXT_MAX + 1)),
        };
        assert!(validate_meal(&bad).is_err());
    }
}
