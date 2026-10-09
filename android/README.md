# VectorCraft for Android

The Gradle project that packages `apps/vectorcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/vectorcraft-android/src/lib.rs`: `android_main`, the platform services (open, place,
  save, print, clipboard text, preferences, URLs; the web shell's set with Android behind it)
  and the JNI bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework pickers (one file for
  Open, several for Place), saving into `Downloads/VectorCraft/`, Android's print dialog, and the
  full-screen window.
- `cargo ndk` drops `libvectorcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p vectorcraft-android
cd android && ./gradlew assembleDebug
```

Set `CRAFT_FONTS_DIR` to a craft-fonts checkout to embed the Japanese fonts, as CI does.

## Keyboard shortcuts

A Bluetooth or USB keyboard works like on the desktop: the same shortcut table (the commands in
`crates/engine/src/cmd/*`, Edit › Keyboard Shortcuts… overrides). Cmd in the table means Ctrl on
Android.

## Known limits (first version)

- Save and Export write to `Downloads/VectorCraft/<name>` without a dialog (the web build's
  "download" path); saving the same name again in one session overwrites it. There is no Save As
  folder, no Open Recent, no Show in Folder, no Edit Original and no Package.
- Paste from other apps takes text (SVG markup) only: no bitmaps, PDF or EMF from the clipboard,
  and nothing is drag-and-dropped onto the canvas.
- File › Print goes through Android's print dialog (which picks the printer or Save as PDF);
  Setup… and the printer list are not available.
- Fonts: the phone's `/system/fonts` plus the embedded ones; no user font installation.
- The desktop layout needs a tablet-sized or unfolded screen; on a phone's cover screen it is
  cramped.
