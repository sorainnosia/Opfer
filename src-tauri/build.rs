fn main() {
    tauri_build::build();

    // On Android, camera_android.rs calls into the NDK Camera2 / media APIs
    // (ACameraManager_*, AImageReader_*, AImage_*). Those symbols live in
    // libcamera2ndk.so and libmediandk.so — link them, or the app's shared
    // library has unresolved symbols and Android aborts the app on load.
    // build.rs runs on the host, so check the *target* via CARGO_CFG_TARGET_OS.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=camera2ndk");
        println!("cargo:rustc-link-lib=mediandk");
    }
}
