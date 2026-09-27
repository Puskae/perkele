use dioxus::prelude::*;
use perkele_shared::auth::UserView;

mod api;
mod screens;
pub mod store;

use screens::{CSS, LoginOrRedeem, Route, SetupScreen};

fn main() {
    // On wasm, launch() just schedules the app on the JS event loop and
    // returns immediately (the browser can't block), so the reporter installs
    // right after it. In debug builds dioxus's devtools replace the hook
    // later — panics then only reach the console, which is fine for dev.
    dioxus::launch(App);
    #[cfg(target_arch = "wasm32")]
    install_panic_reporter();
}

/// Report browser-side panics to the server, which forwards them to
/// GlitchTip. Without this a panic is only visible in the devtools console of
/// whoever's phone it happened on — i.e. never.
#[cfg(target_arch = "wasm32")]
fn install_panic_reporter() {
    // Chain rather than replace: whatever hook is already installed (e.g.
    // one that logs to the console) still runs after ours.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(window) = web_sys::window() {
            let report = perkele_shared::client_report::ClientErrorReport {
                message: info.to_string(),
                url: window.location().href().ok(),
            };
            if let Ok(body) = serde_json::to_string(&report) {
                // sendBeacon, not fetch: the browser queues the request
                // synchronously and delivers it even though the wasm instance
                // aborts right after a panic. The session cookie rides along
                // automatically (same origin).
                let _ = window
                    .navigator()
                    .send_beacon_with_opt_str("/api/client-error", Some(&body));
            }
        }
        previous(info);
    }));
}

/// Where the app is in the authentication lifecycle. The whole UI is a function
/// of this one value.
#[derive(Clone, PartialEq)]
pub enum AuthState {
    /// Still asking the server who we are.
    Loading,
    /// No family exists yet — show first-run setup.
    NeedsSetup,
    /// A family exists but we're not logged in.
    LoggedOut,
    /// Logged in as this member.
    LoggedIn(UserView),
}

#[component]
fn App() -> Element {
    // Provided via context so any screen can flip the auth state (e.g. a form
    // sets `LoggedIn` on success, logout sets `LoggedOut`).
    let mut auth = use_context_provider(|| Signal::new(AuthState::Loading));

    // Register the service worker once on first render. Failure is silently
    // ignored: the app works fine without offline support.
    use_effect(move || {
        #[cfg(target_arch = "wasm32")]
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().service_worker().register("/sw.js");
        }
    });

    // Bootstrap once on load: are we logged in? if not, does setup exist yet?
    use_future(move || async move {
        match api::me().await {
            Ok(user) => auth.set(AuthState::LoggedIn(user)),
            Err(_) => match api::setup_status().await {
                Ok(status) if status.needs_setup => auth.set(AuthState::NeedsSetup),
                _ => auth.set(AuthState::LoggedOut),
            },
        }
    });

    rsx! {
        document::Link { rel: "manifest", href: "/manifest.webmanifest" }
        style { {CSS} }
        match auth() {
            AuthState::Loading => rsx! {
                main { class: "app",
                    p { class: "muted center", "Ladataan…" }
                }
            },
            AuthState::NeedsSetup => rsx! {
                main { class: "app", SetupScreen {} }
            },
            AuthState::LoggedOut => rsx! {
                main { class: "app", LoginOrRedeem {} }
            },
            AuthState::LoggedIn(_) => rsx! {
                Router::<Route> {}
            },
        }
    }
}
