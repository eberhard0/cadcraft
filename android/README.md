# CADCraft for Android

The Gradle project that packages `apps/cadcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/cadcraft-android/src/lib.rs`: `android_main`, the file pickers, the Downloads mirror and
  the JNI bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker, saving into
  `Downloads/CADCraft/`, and the full-screen window.
- `cargo ndk` drops `libcadcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

The engine opens and saves file paths, so a picked file is first copied into the app's private
`files/drawings/` directory and opened from there; every Save / Save As / export the engine
writes to such a path is mirrored to `Downloads/CADCraft/<name>` (MediaStore).

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p cadcraft-android
cd android && ./gradlew assembleDebug
```

## Keyboard shortcuts

A Bluetooth or USB keyboard works like on the desktop (the command line, Cmd+O/S/N… where Cmd
means Ctrl on Android).

## Known limits (first version)

- Save As and exports (DXF, SVG, PNG, PDF plot) write to `Downloads/CADCraft/<name>` without a
  dialog; saving the same name again in one session overwrites it.
- No TCP control server (`--control`), no drag-and-drop, no Open Recent.
- The desktop layout (toolsets, palettes, command line) needs a tablet-sized screen; on a
  phone's cover screen it is cramped. Pinch zoom and pan use egui's touch gestures.
