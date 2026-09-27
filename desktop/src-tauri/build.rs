fn main() {
    // Declaring the app's commands generates `allow-pick-directory`, which
    // `capabilities/main.json` grants to the main window's remote page.
    // Tauri rejects app commands from remote origins without such a grant.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["pick_directory"])),
    )
    .expect("failed to run the Tauri build script");
}
