#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    // First, while single-threaded: agents and tools inherit a PATH that finds `gh`, `node`…
    indocode_lib::prepare_environment();
    if indocode_lib::handle_usage_keychain_helper() {
        return;
    }
    indocode_lib::run()
}
