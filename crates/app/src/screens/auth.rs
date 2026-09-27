//! Auth screens: first-run setup, login/redeem.
//!
//! Each form keeps its inputs in local signals, calls the API in a `spawn`ed
//! task, and shows the server's error message on failure. On success it flips
//! the shared `AuthState` (from context), which re-renders the whole app.

use crate::AuthState;
use crate::api;
use dioxus::prelude::*;
use perkele_shared::auth::{
    LoginRequest, RedeemRequest, SetupRequest, normalize_username, validate_name,
    validate_password, validate_username,
};

/// Which tab the logged-out screen is showing.
#[derive(Clone, Copy, PartialEq)]
enum AuthTab {
    Login,
    Redeem,
}

/// First-run setup: create the family and its first (admin) account.
#[component]
pub fn SetupScreen() -> Element {
    let mut auth = use_context::<Signal<AuthState>>();
    let mut setup_token = use_signal(String::new);
    let mut family = use_signal(String::new);
    let mut display = use_signal(String::new);
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    let submit = move |_| {
        if busy() {
            return;
        }
        if setup_token().trim().is_empty() {
            error.set(Some("Syötä asennustunnus palvelimen lokista.".to_owned()));
            return;
        }
        // Validate client-side for instant feedback (the server re-checks).
        if let Err(e) = validate_name(&family())
            .and_then(|_| validate_name(&display()))
            .and_then(|_| validate_username(&username()))
            .and_then(|_| validate_password(&password()))
        {
            error.set(Some(e.to_owned()));
            return;
        }
        error.set(None);
        busy.set(true);
        let req = SetupRequest {
            setup_token: setup_token(),
            family_name: family(),
            username: username(),
            display_name: display(),
            password: password(),
        };
        spawn(async move {
            match api::setup(req).await {
                Ok(user) => auth.set(AuthState::LoggedIn(user)),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "card",
            h1 { "Perhe-elämää" }
            p { class: "muted", "Perustetaan perheesi." }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            label { "Asennustunnus" }
            input {
                value: "{setup_token}",
                placeholder: "XXXX-XXXX-XXXX-XXXX",
                autocomplete: "off",
                autocapitalize: "characters",
                spellcheck: "false",
                oninput: move |e| setup_token.set(e.value()),
            }
            p { class: "muted",
                "Tunnus tulostuu palvelimen lokiin käynnistyksessä: "
                code { "docker compose logs perkele" }
            }
            label { "Perheen nimi" }
            input {
                value: "{family}",
                placeholder: "Virtanen",
                oninput: move |e| family.set(e.value()),
            }
            label { "Nimesi" }
            input {
                value: "{display}",
                placeholder: "Mikko",
                oninput: move |e| display.set(e.value()),
            }
            label { "Käyttäjänimi" }
            input {
                value: "{username}",
                placeholder: "mikko",
                autocomplete: "username",
                oninput: move |e| username.set(normalize_username(&e.value())),
            }
            label { "Salasana" }
            input {
                r#type: "password",
                value: "{password}",
                autocomplete: "new-password",
                oninput: move |e| password.set(e.value()),
            }
            button { class: "primary", disabled: busy(), onclick: submit,
                if busy() { "Luodaan…" } else { "Luo perhe" }
            }
        }
    }
}

/// The logged-out screen: a Kirjaudu / Onko sinulla kutsukoodi toggle.
#[component]
pub fn LoginOrRedeem() -> Element {
    let tab = use_signal(|| AuthTab::Login);
    rsx! {
        match tab() {
            AuthTab::Login => rsx! { LoginForm { tab } },
            AuthTab::Redeem => rsx! { RedeemForm { tab } },
        }
    }
}

#[component]
fn LoginForm(tab: Signal<AuthTab>) -> Element {
    let mut tab = tab;
    let mut auth = use_context::<Signal<AuthState>>();
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    let submit = move |_| {
        if busy() {
            return;
        }
        error.set(None);
        busy.set(true);
        let req = LoginRequest {
            username: username(),
            password: password(),
        };
        spawn(async move {
            match api::login(req).await {
                Ok(user) => auth.set(AuthState::LoggedIn(user)),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "card",
            h1 { "Perhe-elämää" }
            p { class: "muted", "Tervetuloa takaisin." }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            label { "Käyttäjänimi" }
            input {
                value: "{username}",
                autocomplete: "username",
                oninput: move |e| username.set(normalize_username(&e.value())),
            }
            label { "Salasana" }
            input {
                r#type: "password",
                value: "{password}",
                autocomplete: "current-password",
                oninput: move |e| password.set(e.value()),
            }
            button { class: "primary", disabled: busy(), onclick: submit,
                if busy() { "Kirjaudutaan…" } else { "Kirjaudu sisään" }
            }
            p { class: "switch",
                "Onko sinulla kutsukoodi? "
                a { onclick: move |_| tab.set(AuthTab::Redeem), "Liity perheeseesi" }
            }
        }
    }
}

#[component]
fn RedeemForm(tab: Signal<AuthTab>) -> Element {
    let mut tab = tab;
    let mut auth = use_context::<Signal<AuthState>>();
    let mut code = use_signal(String::new);
    let mut display = use_signal(String::new);
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    let submit = move |_| {
        if busy() {
            return;
        }
        // Validate client-side for instant feedback (the server re-checks).
        if let Err(e) = validate_name(&display())
            .and_then(|_| validate_username(&username()))
            .and_then(|_| validate_password(&password()))
        {
            error.set(Some(e.to_owned()));
            return;
        }
        error.set(None);
        busy.set(true);
        let req = RedeemRequest {
            code: code(),
            username: username(),
            display_name: display(),
            password: password(),
        };
        spawn(async move {
            match api::redeem(req).await {
                Ok(user) => auth.set(AuthState::LoggedIn(user)),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "card",
            h1 { "Liity perheeseesi" }
            p { class: "muted", "Syötä ylläpitäjältä saamasi kutsukoodi." }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            label { "Kutsukoodi" }
            input {
                value: "{code}",
                placeholder: "ABCD-EFGH",
                autocapitalize: "characters",
                oninput: move |e| code.set(e.value().to_uppercase()),
            }
            label { "Nimesi" }
            input {
                value: "{display}",
                placeholder: "Matti",
                oninput: move |e| display.set(e.value()),
            }
            label { "Valitse käyttäjänimi" }
            input {
                value: "{username}",
                placeholder: "matti",
                autocomplete: "username",
                oninput: move |e| username.set(normalize_username(&e.value())),
            }
            label { "Valitse salasana" }
            input {
                r#type: "password",
                value: "{password}",
                autocomplete: "new-password",
                oninput: move |e| password.set(e.value()),
            }
            button { class: "primary", disabled: busy(), onclick: submit,
                if busy() { "Liitytään…" } else { "Liity" }
            }
            p { class: "switch",
                a { onclick: move |_| tab.set(AuthTab::Login), "Takaisin kirjautumiseen" }
            }
        }
    }
}
