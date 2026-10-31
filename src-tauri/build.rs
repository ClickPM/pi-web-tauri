fn main() {
    // Declaring the app's commands here turns them into ACL permissions named
    // `allow-<command>` (underscores become dashes), which capabilities then
    // grant explicitly.
    //
    // This is REQUIRED, not cosmetic. Without an app manifest, Tauri allows app
    // commands on local pages and refuses them on remote ones — the embedded
    // pi-web origin got `select_directory not allowed. Plugin not found`, so the
    // directory picker, theme sync and update CTA were all dead there while
    // looking fine on the shell's own pages. With the manifest, every origin has
    // to be granted each command by name, and capabilities/remote.json spells
    // out exactly these three.
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            // Main window / pi-web surface (capabilities/remote.json):
            "select_directory",
            "theme_changed",
            "apply_update",
            // Local-only Host Manager window surface (capabilities/hosts-manager.json):
            "hosts_get_state",
            "hosts_suggest_port",
            "hosts_check_env",
            "hosts_test_connection",
            "hosts_save",
            "hosts_open_dir",
            "hosts_close_window",
        ]),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
