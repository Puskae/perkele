//! Startup seeding of default recipes.
//!
//! A brand-new family (or the very first prod family) shouldn't stare at an
//! empty recipe screen. On every server start we look for families that have
//! *never* had a recipe row and copy 20 easy Finnish/European/Asian defaults
//! into them. Deletes in this app are soft (`deleted_at`), so "zero rows in
//! recipes" really means "never seeded, never hand-added" — a family that
//! deletes all the defaults keeps its soft-deleted rows and is never re-seeded.

use crate::db::Db;
use crate::error::ApiError;
use perkele_shared::recipe::{RecipeIngredient, SaveRecipeRequest};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Marker written to `recipes.source` so seeded rows are always
/// distinguishable from hand-entered ones.
pub(crate) const SEED_SOURCE: &str = "PERKELE-oletus";

/// Shorthand ingredient constructor. Empty strings mean "no value" (NULL in
/// the DB) — keeps the big data table below readable.
fn ing(name: &str, qty: &str, unit: &str, category: &str) -> RecipeIngredient {
    let opt = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    };
    RecipeIngredient {
        name: name.to_owned(),
        qty: opt(qty),
        unit: opt(unit),
        category: opt(category),
    }
}

/// Shorthand recipe constructor: every default serves 4 and carries the
/// seed-source marker.
fn recipe(
    title: &str,
    prep_min: i64,
    cook_min: i64,
    instructions: &str,
    ingredients: Vec<RecipeIngredient>,
) -> SaveRecipeRequest {
    SaveRecipeRequest {
        title: title.to_owned(),
        instructions: Some(instructions.to_owned()),
        servings: Some(4),
        prep_min: Some(prep_min),
        cook_min: Some(cook_min),
        source: Some(SEED_SOURCE.to_owned()),
        ingredients,
    }
}

/// The 20 default recipes, built fresh on each call (they're only needed at
/// startup, so no need for a static).
pub(crate) fn default_recipes() -> Vec<SaveRecipeRequest> {
    vec![
        // ---------- Suomalaiset (7) ----------
        recipe(
            "Lohikeitto",
            15,
            20,
            "1. Kuori ja kuutioi perunat ja porkkanat. Hienonna sipuli.\n\
             2. Kuullota sipuli voissa kattilan pohjalla. Lisää perunat, porkkanat, \
             kalaliemikuutio ja noin 8 dl vettä. Keitä 10 minuuttia.\n\
             3. Kuutioi lohi ja lisää kattilaan kerman kanssa. Hauduta 5–8 minuuttia, \
             kunnes kala on kypsää.\n\
             4. Mausta suolalla ja pippurilla. Lisää tilli juuri ennen tarjoilua.",
            vec![
                ing("lohifilee", "400", "g", "kala"),
                ing("peruna", "800", "g", "kasvikset"),
                ing("porkkana", "2", "kpl", "kasvikset"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("kalaliemikuutio", "1", "kpl", "kuivatuotteet"),
                ing("ruokakerma", "2", "dl", "maito"),
                ing("voi", "1", "rkl", "maito"),
                ing("tuore tilli", "1", "ruukku", "kasvikset"),
                ing("suola", "", "", "mausteet"),
                ing("mustapippuri", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Makaronilaatikko",
            20,
            40,
            "1. Keitä makaronit napakan kypsiksi ja valuta.\n\
             2. Ruskista jauheliha ja hienonnettu sipuli pannulla. Mausta suolalla \
             ja pippurilla.\n\
             3. Sekoita makaronit ja jauheliha voideltuun uunivuokaan.\n\
             4. Vatkaa munamaito (maito + kananmunat + 1 tl suolaa) ja kaada vuokaan. \
             Ripottele juustoraaste pinnalle.\n\
             5. Paista 180 °C uunissa noin 40 minuuttia, kunnes munamaito on hyytynyt.",
            vec![
                ing("makaroni", "400", "g", "kuivatuotteet"),
                ing("jauheliha", "400", "g", "liha"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("maito", "8", "dl", "maito"),
                ing("kananmuna", "3", "kpl", "maito"),
                ing("juustoraaste", "150", "g", "maito"),
                ing("suola", "1", "tl", "mausteet"),
                ing("mustapippuri", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Lihapullat ja perunamuusi",
            25,
            20,
            "1. Kuori perunat ja keitä pehmeiksi suolatussa vedessä (n. 20 min).\n\
             2. Sekoita korppujauhot ja maito; anna turvota 5 minuuttia. Lisää \
             jauheliha, kananmuna, hienonnettu sipuli, suola ja pippuri.\n\
             3. Pyörittele taikinasta pullia ja paista pannulla voissa kauniin \
             ruskeiksi, noin 10–12 minuuttia.\n\
             4. Soseuta perunat, lisää voi ja lämmin maito. Mausta suolalla.",
            vec![
                ing("jauheliha", "400", "g", "liha"),
                ing("korppujauho", "0.5", "dl", "kuivatuotteet"),
                ing("kananmuna", "1", "kpl", "maito"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("maito", "3", "dl", "maito"),
                ing("peruna", "1", "kg", "kasvikset"),
                ing("voi", "50", "g", "maito"),
                ing("suola", "", "", "mausteet"),
                ing("mustapippuri", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Uunilohi ja riisi",
            10,
            25,
            "1. Laita uuni kuumenemaan 200 °C:een. Laita riisi kiehumaan \
             pakkauksen ohjeen mukaan.\n\
             2. Nosta lohifilee leivinpaperille uunivuokaan nahkapuoli alaspäin. \
             Sivele voilla, mausta suolalla ja purista päälle sitruunamehua.\n\
             3. Paista uunissa 20–25 minuuttia, kunnes lohi on kypsää.\n\
             4. Ripottele tilli pinnalle ja tarjoa riisin kanssa.",
            vec![
                ing("lohifilee", "500", "g", "kala"),
                ing("riisi", "3", "dl", "kuivatuotteet"),
                ing("sitruuna", "1", "kpl", "hedelmät"),
                ing("voi", "2", "rkl", "maito"),
                ing("tuore tilli", "1", "ruukku", "kasvikset"),
                ing("suola", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Nakkikastike ja perunamuusi",
            10,
            20,
            "1. Kuori perunat ja keitä pehmeiksi suolatussa vedessä.\n\
             2. Viipaloi nakit ja hienonna sipuli. Ruskista molemmat pannulla.\n\
             3. Lisää tomaattimurska, kerma ja sinappi. Hauduta 10 minuuttia.\n\
             4. Soseuta perunat voin ja lämpimän maidon kanssa. Mausta suolalla.",
            vec![
                ing("nakki", "400", "g", "liha"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("tomaattimurska", "1", "tlk", "säilykkeet"),
                ing("ruokakerma", "2", "dl", "maito"),
                ing("sinappi", "1", "rkl", "mausteet"),
                ing("peruna", "1", "kg", "kasvikset"),
                ing("voi", "50", "g", "maito"),
                ing("maito", "2", "dl", "maito"),
            ],
        ),
        recipe(
            "Jauhelihakeitto",
            15,
            25,
            "1. Kuori ja kuutioi perunat ja porkkanat. Viipaloi purjo.\n\
             2. Ruskista jauheliha kattilan pohjalla.\n\
             3. Lisää kasvikset, liemikuutiot ja vesi. Keitä noin 20 minuuttia, \
             kunnes perunat ovat kypsiä.\n\
             4. Mausta mustapippurilla ja tarkista suola.",
            vec![
                ing("jauheliha", "400", "g", "liha"),
                ing("peruna", "600", "g", "kasvikset"),
                ing("porkkana", "2", "kpl", "kasvikset"),
                ing("purjo", "1", "kpl", "kasvikset"),
                ing("lihaliemikuutio", "2", "kpl", "kuivatuotteet"),
                ing("vesi", "1", "l", ""),
                ing("mustapippuri", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Kalapuikot ja perunamuusi",
            10,
            20,
            "1. Kuori perunat ja keitä pehmeiksi suolatussa vedessä.\n\
             2. Paista kalapuikot pannulla tai uunissa pakkauksen ohjeen mukaan.\n\
             3. Soseuta perunat, lisää voi ja lämmin maito. Mausta suolalla.\n\
             4. Tarjoa sitruunalohkojen ja kurkkutikkujen kanssa.",
            vec![
                ing("kalapuikko", "16", "kpl", "pakasteet"),
                ing("peruna", "1", "kg", "kasvikset"),
                ing("voi", "50", "g", "maito"),
                ing("maito", "2", "dl", "maito"),
                ing("sitruuna", "1", "kpl", "hedelmät"),
                ing("kurkku", "1", "kpl", "kasvikset"),
                ing("suola", "", "", "mausteet"),
            ],
        ),
        // ---------- Eurooppalaiset (8) ----------
        recipe(
            "Spagetti bolognese",
            15,
            30,
            "1. Hienonna sipuli ja valkosipuli. Kuullota öljyssä isossa kasarissa.\n\
             2. Lisää jauheliha ja ruskista. Lisää tomaattipyree ja paista hetki.\n\
             3. Lisää tomaattimurska ja oregano. Hauduta miedolla lämmöllä \
             vähintään 20 minuuttia. Mausta suolalla ja pippurilla.\n\
             4. Keitä spagetti pakkauksen ohjeen mukaan. Tarjoa kastikkeen ja \
             parmesaanin kanssa.",
            vec![
                ing("spagetti", "400", "g", "kuivatuotteet"),
                ing("jauheliha", "400", "g", "liha"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("valkosipulinkynsi", "2", "kpl", "kasvikset"),
                ing("tomaattimurska", "2", "tlk", "säilykkeet"),
                ing("tomaattipyree", "2", "rkl", "säilykkeet"),
                ing("kuivattu oregano", "1", "tl", "mausteet"),
                ing("parmesaani", "50", "g", "maito"),
                ing("oliiviöljy", "2", "rkl", "kuivatuotteet"),
            ],
        ),
        recipe(
            "Pasta carbonara",
            10,
            15,
            "1. Keitä spagetti suolatussa vedessä. Säästä kupillinen keitinvettä.\n\
             2. Paista pekonikuutiot rapeiksi pannulla.\n\
             3. Sekoita kulhossa kananmunat, raastettu parmesaani ja reilusti \
             mustapippuria.\n\
             4. Nosta pannu levyltä. Sekoita kuuma pasta ja pekoni munaseokseen; \
             notkista keitinvedellä. Muna kypsyy pastan lämmöstä — älä keitä.",
            vec![
                ing("spagetti", "400", "g", "kuivatuotteet"),
                ing("pekoni", "170", "g", "liha"),
                ing("kananmuna", "3", "kpl", "maito"),
                ing("parmesaani", "100", "g", "maito"),
                ing("mustapippuri", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Tomaattinen kanapasta",
            15,
            20,
            "1. Keitä pasta pakkauksen ohjeen mukaan.\n\
             2. Ruskista broilerisuikaleet öljyssä. Lisää hienonnettu sipuli, \
             valkosipuli ja paprikakuutiot; kuullota hetki.\n\
             3. Lisää tomaattimurska ja kerma. Hauduta 10 minuuttia ja mausta \
             suolalla ja pippurilla.\n\
             4. Sekoita pasta kastikkeeseen ja tarjoa.",
            vec![
                ing("pasta", "400", "g", "kuivatuotteet"),
                ing("broilerin fileesuikale", "400", "g", "liha"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("valkosipulinkynsi", "2", "kpl", "kasvikset"),
                ing("paprika", "1", "kpl", "kasvikset"),
                ing("tomaattimurska", "1", "tlk", "säilykkeet"),
                ing("ruokakerma", "2", "dl", "maito"),
                ing("oliiviöljy", "1", "rkl", "kuivatuotteet"),
            ],
        ),
        recipe(
            "Jauhelihatacot",
            15,
            15,
            "1. Ruskista jauheliha pannulla. Lisää tacomausteseos ja vettä \
             pussin ohjeen mukaan; hauduta 5 minuuttia.\n\
             2. Kuutioi tomaatit ja kurkku, suikaloi salaatti.\n\
             3. Lämmitä tortillat pakkauksen ohjeen mukaan.\n\
             4. Katetaan pöytään: jokainen täyttää omat tortillansa lihalla, \
             kasviksilla, juustolla ja salsalla.",
            vec![
                ing("jauheliha", "400", "g", "liha"),
                ing("tacomausteseos", "1", "ps", "mausteet"),
                ing("tortilla", "8", "kpl", "leipä"),
                ing("juustoraaste", "150", "g", "maito"),
                ing("tomaatti", "2", "kpl", "kasvikset"),
                ing("kurkku", "1", "kpl", "kasvikset"),
                ing("jäävuorisalaatti", "0.5", "kpl", "kasvikset"),
                ing("salsakastike", "1", "prk", "säilykkeet"),
            ],
        ),
        recipe(
            "Uunimakkara ja lohkoperunat",
            15,
            35,
            "1. Laita uuni 225 °C:een. Pese perunat ja lohko kuorineen veneiksi.\n\
             2. Kääntele lohkot öljyssä, suolassa ja paprikajauheessa pellillä. \
             Paista 20 minuuttia.\n\
             3. Viillota makkarat ja lisää pellille. Paista vielä 15 minuuttia, \
             kunnes makkarat ovat saaneet väriä.\n\
             4. Tarjoa sinapin ja ketsupin kanssa.",
            vec![
                ing("grillimakkara", "8", "kpl", "liha"),
                ing("peruna", "1", "kg", "kasvikset"),
                ing("ruokaöljy", "2", "rkl", "kuivatuotteet"),
                ing("paprikajauhe", "1", "tl", "mausteet"),
                ing("sinappi", "", "", "mausteet"),
                ing("ketsuppi", "", "", "mausteet"),
                ing("suola", "", "", "mausteet"),
            ],
        ),
        recipe(
            "Kasvissosekeitto",
            15,
            20,
            "1. Kuori ja kuutioi perunat, porkkanat, palsternakka ja sipuli.\n\
             2. Laita kasvikset kattilaan, lisää liemikuutiot ja vettä juuri sen \
             verran, että kasvikset peittyvät. Keitä 15–20 minuuttia pehmeiksi.\n\
             3. Soseuta sauvasekoittimella. Lisää kerma ja kuumenna.\n\
             4. Mausta suolalla ja pippurilla. Tarjoa leivän kanssa.",
            vec![
                ing("peruna", "400", "g", "kasvikset"),
                ing("porkkana", "400", "g", "kasvikset"),
                ing("palsternakka", "1", "kpl", "kasvikset"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("kasvisliemikuutio", "2", "kpl", "kuivatuotteet"),
                ing("ruokakerma", "2", "dl", "maito"),
                ing("leipä", "", "", "leipä"),
            ],
        ),
        recipe(
            "Broileririsotto",
            15,
            25,
            "1. Kuumenna kanaliemi (vesi + kuutiot) kattilassa.\n\
             2. Ruskista broilerisuikaleet voissa paksupohjaisessa kasarissa; \
             lisää hienonnettu sipuli ja kuullota.\n\
             3. Lisää riisi ja paista pari minuuttia. Lisää kuumaa lientä \
             kauhallinen kerrallaan koko ajan sekoittaen, noin 20 minuuttia.\n\
             4. Kun riisi on kypsää, sekoita joukkoon herneet, voi ja parmesaani. \
             Anna vetäytyä hetki kannen alla.",
            vec![
                ing("risottoriisi", "3", "dl", "kuivatuotteet"),
                ing("broilerin fileesuikale", "400", "g", "liha"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("kanaliemikuutio", "2", "kpl", "kuivatuotteet"),
                ing("pakasteherne", "2", "dl", "pakasteet"),
                ing("parmesaani", "50", "g", "maito"),
                ing("voi", "2", "rkl", "maito"),
            ],
        ),
        recipe(
            "Tortillapizzat",
            15,
            10,
            "1. Laita uuni 225 °C:een.\n\
             2. Levitä tortillojen päälle ohut kerros tomaattipyreetä.\n\
             3. Lisää täytteet: kinkkusuikaleet, ananas ja juustoraaste. \
             Ripottele pinnalle oreganoa.\n\
             4. Paista pellillä 8–10 minuuttia, kunnes juusto on sulanut ja \
             reunat rapeat.",
            vec![
                ing("tortilla", "8", "kpl", "leipä"),
                ing("tomaattipyree", "1", "prk", "säilykkeet"),
                ing("juustoraaste", "200", "g", "maito"),
                ing("kinkkusuikale", "200", "g", "liha"),
                ing("ananasmurska", "1", "tlk", "säilykkeet"),
                ing("kuivattu oregano", "1", "tl", "mausteet"),
            ],
        ),
        // ---------- Aasialaiset (5) ----------
        recipe(
            "Kanawokki nuudeleilla",
            15,
            15,
            "1. Keitä nuudelit pakkauksen ohjeen mukaan ja valuta.\n\
             2. Kuumenna öljy wokkipannussa. Paista broilerisuikaleet kypsiksi.\n\
             3. Lisää hienonnettu valkosipuli, raastettu inkivääri ja \
             wokkivihannekset. Paista kovalla lämmöllä 3–4 minuuttia.\n\
             4. Lisää nuudelit, soijakastike ja hunaja. Sekoita ja kuumenna hetki.",
            vec![
                ing("broilerin fileesuikale", "400", "g", "liha"),
                ing("nuudeli", "250", "g", "kuivatuotteet"),
                ing("wokkivihannes", "400", "g", "pakasteet"),
                ing("soijakastike", "3", "rkl", "mausteet"),
                ing("hunaja", "1", "rkl", "kuivatuotteet"),
                ing("valkosipulinkynsi", "2", "kpl", "kasvikset"),
                ing("tuore inkivääri", "3", "cm", "kasvikset"),
                ing("ruokaöljy", "2", "rkl", "kuivatuotteet"),
            ],
        ),
        recipe(
            "Teriyakikana ja riisi",
            10,
            20,
            "1. Laita riisi kiehumaan pakkauksen ohjeen mukaan.\n\
             2. Paista broilerisuikaleet öljyssä kypsiksi. Lisää parsakaalin \
             nuput ja paista pari minuuttia.\n\
             3. Kaada teriyakikastike pannulle ja hauduta 5 minuuttia, kunnes \
             kastike hieman paksuuntuu.\n\
             4. Tarjoa riisin päällä. Viimeistele kevätsipulilla ja \
             seesaminsiemenillä.",
            vec![
                ing("broilerin fileesuikale", "500", "g", "liha"),
                ing("teriyakikastike", "1", "dl", "mausteet"),
                ing("riisi", "3", "dl", "kuivatuotteet"),
                ing("parsakaali", "1", "kpl", "kasvikset"),
                ing("kevätsipuli", "2", "kpl", "kasvikset"),
                ing("seesaminsiemen", "1", "rkl", "kuivatuotteet"),
                ing("ruokaöljy", "1", "rkl", "kuivatuotteet"),
            ],
        ),
        recipe(
            "Butter chicken",
            15,
            25,
            "1. Laita riisi kiehumaan. Ruskista broilerisuikaleet voissa ja \
             nosta sivuun.\n\
             2. Kuullota hienonnettu sipuli ja valkosipuli samassa kattilassa. \
             Lisää garam masala ja paista hetki, kunnes tuoksuu.\n\
             3. Lisää tomaattimurska ja hauduta 10 minuuttia. Lisää kana ja \
             kerma; hauduta vielä 10 minuuttia.\n\
             4. Mausta suolalla. Tarjoa riisin ja halutessa naan-leivän kanssa.",
            vec![
                ing("broilerin fileesuikale", "500", "g", "liha"),
                ing("voi", "50", "g", "maito"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("valkosipulinkynsi", "2", "kpl", "kasvikset"),
                ing("garam masala", "2", "tl", "mausteet"),
                ing("tomaattimurska", "1", "tlk", "säilykkeet"),
                ing("kuohukerma", "2", "dl", "maito"),
                ing("riisi", "3", "dl", "kuivatuotteet"),
                ing("naan-leipä", "4", "kpl", "leipä"),
            ],
        ),
        recipe(
            "Mieto kookos-kanacurry",
            15,
            20,
            "1. Laita riisi kiehumaan pakkauksen ohjeen mukaan.\n\
             2. Ruskista broilerisuikaleet öljyssä kattilassa. Lisää hienonnettu \
             sipuli ja paprikasuikaleet; kuullota hetki.\n\
             3. Lisää currytahna ja paista minuutti. Kaada joukkoon kookosmaito \
             ja hauduta 10 minuuttia.\n\
             4. Lisää herneet, kuumenna ja mausta suolalla. Tarjoa riisin kanssa.",
            vec![
                ing("broilerin fileesuikale", "400", "g", "liha"),
                ing("kookosmaito", "1", "tlk", "säilykkeet"),
                ing("mieto currytahna", "2", "rkl", "säilykkeet"),
                ing("paprika", "1", "kpl", "kasvikset"),
                ing("sipuli", "1", "kpl", "kasvikset"),
                ing("pakasteherne", "2", "dl", "pakasteet"),
                ing("riisi", "3", "dl", "kuivatuotteet"),
                ing("ruokaöljy", "1", "rkl", "kuivatuotteet"),
            ],
        ),
        recipe(
            "Paistettu riisi kananmunalla",
            10,
            15,
            "1. Keitä riisi ja anna jäähtyä (edellisen päivän riisi toimii \
             parhaiten).\n\
             2. Riko kananmunat kuumalle öljytylle pannulle ja sekoita \
             munakokkeliksi; nosta sivuun.\n\
             3. Paista kinkkusuikaleet ja pakastevihannekset pannulla pari \
             minuuttia. Lisää riisi ja paista kovalla lämmöllä sekoitellen.\n\
             4. Lisää munat ja soijakastike. Sekoita ja viimeistele kevätsipulilla.",
            vec![
                ing("riisi", "4", "dl", "kuivatuotteet"),
                ing("kananmuna", "4", "kpl", "maito"),
                ing("kinkkusuikale", "200", "g", "liha"),
                ing("herne-maissi-paprika", "300", "g", "pakasteet"),
                ing("soijakastike", "3", "rkl", "mausteet"),
                ing("kevätsipuli", "2", "kpl", "kasvikset"),
                ing("ruokaöljy", "2", "rkl", "kuivatuotteet"),
            ],
        ),
    ]
}

/// Seed the 20 default recipes into every family that has never had a recipe.
///
/// Called once at startup, after migrations. Idempotent: the guard is "zero
/// rows in `recipes` for the family" — and since recipe deletes are soft, a
/// family that removes the defaults keeps its rows and is never re-seeded.
/// Each family is seeded inside one transaction, so a crash mid-seed leaves
/// either all 20 recipes or none (and none means the next startup retries).
pub(crate) async fn seed_default_recipes(db: &Db) -> Result<(), ApiError> {
    // Families that have never had a single recipe row (soft-deleted included).
    let family_ids = sqlx::query_scalar!(
        r#"SELECT f.id AS "id!: i64"
           FROM families f
           WHERE NOT EXISTS (SELECT 1 FROM recipes r WHERE r.family_id = f.id)"#,
    )
    .fetch_all(db)
    .await?;

    for family_id in family_ids {
        // Attribute seeded rows to the family's oldest admin; fall back to the
        // oldest member. `role <> 'admin'` sorts as 0 (false) for admins, so
        // they come first. A family with no users yet is skipped — the next
        // startup will pick it up.
        let seeder = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64"
               FROM users
               WHERE family_id = ?
               ORDER BY (role <> 'admin') ASC, id ASC
               LIMIT 1"#,
            family_id,
        )
        .fetch_optional(db)
        .await?;
        let Some(created_by) = seeder else {
            continue;
        };

        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| ApiError::Internal)?;

        let mut tx = db.begin().await?;
        for req in default_recipes() {
            let recipe_id = sqlx::query!(
                "INSERT INTO recipes
                    (family_id, title, instructions, servings, prep_min, cook_min,
                     source, created_by, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                family_id,
                req.title,
                req.instructions,
                req.servings,
                req.prep_min,
                req.cook_min,
                req.source,
                created_by,
                now,
                now,
            )
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();

            crate::recipe::insert_ingredients(&mut tx, family_id, recipe_id, &req.ingredients)
                .await?;
        }
        tx.commit().await?;
        tracing::info!("seeded {} default recipes into family {family_id}", 20);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, test_pool};

    /// Insert a family with no members and return its id.
    async fn fixture_family(db: &Db, name: &str) -> i64 {
        sqlx::query!(
            "INSERT INTO families (name, created_at) VALUES (?, '2026-01-01T00:00:00Z')",
            name,
        )
        .execute(db)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    /// Insert a user into `family_id` and return their id. `pw_hash` is a dummy —
    /// these tests never log in.
    async fn fixture_user(db: &Db, family_id: i64, username: &str, role: &str) -> i64 {
        sqlx::query!(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, ?, ?, 'x', ?, '2026-01-01T00:00:00Z')",
            family_id,
            username,
            username,
            role,
        )
        .execute(db)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn recipe_count(db: &Db, family_id: i64) -> i64 {
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!: i64" FROM recipes WHERE family_id = ?"#,
            family_id,
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn seeds_empty_family_with_20_recipes_and_ingredients() {
        let db = test_pool().await;
        let fam = fixture_family(&db, "Virtanen").await;
        let admin = fixture_user(&db, fam, "mikko", "admin").await;

        seed_default_recipes(&db).await.unwrap();

        assert_eq!(recipe_count(&db, fam).await, 20);

        // Every seeded recipe is marked, attributed to the admin, and has
        // ingredient rows scoped to the family.
        let rows = sqlx::query!(
            r#"SELECT id AS "id!: i64", source AS "source?: String",
                      created_by AS "created_by!: i64"
               FROM recipes WHERE family_id = ?"#,
            fam,
        )
        .fetch_all(&db)
        .await
        .unwrap();
        for r in &rows {
            assert_eq!(r.source.as_deref(), Some(SEED_SOURCE));
            assert_eq!(r.created_by, admin);
            let n_ing = sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!: i64" FROM recipe_ingredients
                   WHERE recipe_id = ? AND family_id = ?"#,
                r.id,
                fam,
            )
            .fetch_one(&db)
            .await
            .unwrap();
            assert!(n_ing > 0, "recipe {} has no ingredient rows", r.id);
        }
    }

    #[tokio::test]
    async fn second_run_inserts_nothing() {
        let db = test_pool().await;
        let fam = fixture_family(&db, "Virtanen").await;
        fixture_user(&db, fam, "mikko", "admin").await;

        seed_default_recipes(&db).await.unwrap();
        seed_default_recipes(&db).await.unwrap();

        assert_eq!(recipe_count(&db, fam).await, 20);
    }

    #[tokio::test]
    async fn family_with_existing_recipe_is_untouched() {
        let db = test_pool().await;
        let fam = fixture_family(&db, "Virtanen").await;
        let user = fixture_user(&db, fam, "mikko", "admin").await;

        // One pre-existing (even soft-deleted!) recipe means "never seed".
        sqlx::query!(
            "INSERT INTO recipes (family_id, title, created_by, created_at, updated_at, deleted_at)
             VALUES (?, 'Oma resepti', ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                     '2026-01-02T00:00:00Z')",
            fam,
            user,
        )
        .execute(&db)
        .await
        .unwrap();

        seed_default_recipes(&db).await.unwrap();

        assert_eq!(recipe_count(&db, fam).await, 1);
    }

    #[tokio::test]
    async fn seeder_prefers_admin_over_older_member() {
        let db = test_pool().await;
        let fam = fixture_family(&db, "Virtanen").await;
        // Member gets the *lower* id — the admin must still win.
        fixture_user(&db, fam, "lapsi", "member").await;
        let admin = fixture_user(&db, fam, "mikko", "admin").await;

        seed_default_recipes(&db).await.unwrap();

        let creators: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT DISTINCT created_by AS "created_by!: i64"
               FROM recipes WHERE family_id = ?"#,
            fam,
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(creators, vec![admin]);
    }

    #[tokio::test]
    async fn memberless_family_is_skipped_without_error() {
        let db = test_pool().await;
        let fam = fixture_family(&db, "Tyhjä").await;

        seed_default_recipes(&db).await.unwrap();

        assert_eq!(recipe_count(&db, fam).await, 0);
    }

    /// Every seed recipe must pass the exact validation the API applies to
    /// user input — the seeder writes to the same tables the handlers do.
    #[test]
    fn default_recipes_are_valid_and_complete() {
        let recipes = default_recipes();
        assert_eq!(recipes.len(), 20, "expected exactly 20 default recipes");
        for r in &recipes {
            crate::recipe::validate_save(r)
                .unwrap_or_else(|_| panic!("seed recipe {:?} fails API validation", r.title));
            assert_eq!(r.servings, Some(4), "{}: servings must be 4", r.title);
            assert_eq!(
                r.source.as_deref(),
                Some(SEED_SOURCE),
                "{}: missing seed marker",
                r.title
            );
            assert!(
                !r.ingredients.is_empty(),
                "{}: recipe has no ingredients",
                r.title
            );
            assert!(
                r.instructions
                    .as_deref()
                    .is_some_and(|i| !i.trim().is_empty()),
                "{}: recipe has no instructions",
                r.title
            );
        }
    }
}
