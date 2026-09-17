// Single Rust-side source of truth for the product's display name, for the
// handful of log/dialog strings emitted before a Tauri `AppHandle` exists
// (and so is not yet able to read `productName` from `tauri.conf.json` via
// `app.package_info().name`). Everywhere an `AppHandle` is already in scope,
// prefer `handle.package_info().name` over this constant so `tauri.conf.json`
// stays the single canonical source.
pub const PRODUCT_NAME: &str = "MyroLogic POS";
