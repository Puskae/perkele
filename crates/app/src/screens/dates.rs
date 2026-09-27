//! Shared UTC date helpers for the meal planner and calendar. We work entirely
//! in UTC so client-computed dates match the strings the server stores.

use wasm_bindgen::JsValue;

/// Format a JS `Date` as a UTC 'YYYY-MM-DD' string.
pub fn format_utc(d: &js_sys::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        d.get_utc_full_year(),
        d.get_utc_month() + 1,
        d.get_utc_date(),
    )
}

/// Today's date as a UTC 'YYYY-MM-DD' string. Uses UTC like the rest of the
/// app (events are stored as floating wall-clock with a `Z` suffix), so this
/// matches the day keys the month/week grids compare against.
pub fn today() -> String {
    format_utc(&js_sys::Date::new_0())
}

/// Monday (UTC 'YYYY-MM-DD') of the current week.
pub fn current_monday() -> String {
    let days = (js_sys::Date::now() / 86_400_000.0).floor() as i64;
    // Epoch day 0 (1970-01-01) was a Thursday; with Monday = 0, Thursday = 3.
    let monday_days = days - ((days + 3) % 7);
    let d = js_sys::Date::new(&JsValue::from_f64(monday_days as f64 * 86_400_000.0));
    format_utc(&d)
}

/// Finnish weekday abbreviation ("Ma".."Su") for a 'YYYY-MM-DD' date.
pub fn weekday_fi(date: &str) -> &'static str {
    const DAYS: [&str; 7] = ["Ma", "Ti", "Ke", "To", "Pe", "La", "Su"];
    let d = js_sys::Date::new(&JsValue::from_str(&format!("{date}T00:00:00Z")));
    // JS getUTCDay: 0=Sun..6=Sat. Convert to Monday=0 before indexing.
    let idx = ((d.get_utc_day() as i64) + 6) % 7;
    DAYS[idx as usize]
}

/// Shift a 'YYYY-MM-DD' date by `delta_days`, staying in UTC.
pub fn shift_day(date: &str, delta_days: i64) -> String {
    let d = js_sys::Date::new(&JsValue::from_str(&format!("{date}T00:00:00Z")));
    let shifted = js_sys::Date::new(&JsValue::from_f64(
        d.get_time() + delta_days as f64 * 86_400_000.0,
    ));
    format_utc(&shifted)
}
