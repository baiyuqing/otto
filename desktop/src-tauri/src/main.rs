// Prevents an additional console window on Windows in release; irrelevant on
// macOS, kept for parity with the Tauri template.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    otto_desktop_lib::run();
}
