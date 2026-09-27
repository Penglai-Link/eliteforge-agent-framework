// Native resource selection is based on the compilation target, never the build host.
#[cfg(feature = "bundled-native")]
pub(crate) const FILES: &[(&str, &[u8])] = &[
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    (
        "sqlite-simple/v0.7.1/android-aarch64/libsimple.so",
        include_bytes!("../resources/sqlite-simple/v0.7.1/android-aarch64/libsimple.so"),
    ),
    #[cfg(all(target_os = "android", target_arch = "x86_64"))]
    (
        "sqlite-simple/v0.7.1/android-x86_64/libsimple.so",
        include_bytes!("../resources/sqlite-simple/v0.7.1/android-x86_64/libsimple.so"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/hmm_model.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/hmm_model.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/idf.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/idf.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/jieba.dict.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/jieba.dict.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/pos_dict/char_state_tab.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/pos_dict/char_state_tab.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/pos_dict/prob_emit.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/pos_dict/prob_emit.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/pos_dict/prob_start.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/pos_dict/prob_start.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/pos_dict/prob_trans.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/pos_dict/prob_trans.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/stop_words.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/stop_words.utf8"),
    ),
    (
        "sqlite-simple/v0.7.1/dict/user.dict.utf8",
        include_bytes!("../resources/sqlite-simple/v0.7.1/dict/user.dict.utf8"),
    ),
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    (
        "sqlite-simple/v0.7.1/linux-aarch64/libsimple.so",
        include_bytes!("../resources/sqlite-simple/v0.7.1/linux-aarch64/libsimple.so"),
    ),
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    (
        "sqlite-simple/v0.7.1/linux-x86_64/libsimple.so",
        include_bytes!("../resources/sqlite-simple/v0.7.1/linux-x86_64/libsimple.so"),
    ),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    (
        "sqlite-simple/v0.7.1/macos-aarch64/libsimple.dylib",
        include_bytes!("../resources/sqlite-simple/v0.7.1/macos-aarch64/libsimple.dylib"),
    ),
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    (
        "sqlite-simple/v0.7.1/macos-x86_64/libsimple.dylib",
        include_bytes!("../resources/sqlite-simple/v0.7.1/macos-x86_64/libsimple.dylib"),
    ),
    #[cfg(all(target_env = "ohos", target_arch = "aarch64"))]
    (
        "sqlite-simple/v0.7.1/ohos-aarch64/libsimple.so",
        include_bytes!("../resources/sqlite-simple/v0.7.1/ohos-aarch64/libsimple.so"),
    ),
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    (
        "sqlite-simple/v0.7.1/windows-aarch64/simple.dll",
        include_bytes!("../resources/sqlite-simple/v0.7.1/windows-aarch64/simple.dll"),
    ),
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    (
        "sqlite-simple/v0.7.1/windows-i686/simple.dll",
        include_bytes!("../resources/sqlite-simple/v0.7.1/windows-i686/simple.dll"),
    ),
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    (
        "sqlite-simple/v0.7.1/windows-x86_64/simple.dll",
        include_bytes!("../resources/sqlite-simple/v0.7.1/windows-x86_64/simple.dll"),
    ),
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    (
        "sqlite-vec/v0.1.9/android-aarch64/vec0.so",
        include_bytes!("../resources/sqlite-vec/v0.1.9/android-aarch64/vec0.so"),
    ),
    #[cfg(all(target_os = "android", target_arch = "x86"))]
    (
        "sqlite-vec/v0.1.9/android-i686/vec0.so",
        include_bytes!("../resources/sqlite-vec/v0.1.9/android-i686/vec0.so"),
    ),
    #[cfg(all(target_os = "android", target_arch = "x86_64"))]
    (
        "sqlite-vec/v0.1.9/android-x86_64/vec0.so",
        include_bytes!("../resources/sqlite-vec/v0.1.9/android-x86_64/vec0.so"),
    ),
    #[cfg(all(target_os = "ios", target_arch = "aarch64", not(target_abi = "sim")))]
    (
        "sqlite-vec/v0.1.9/ios-aarch64/vec0.dylib",
        include_bytes!("../resources/sqlite-vec/v0.1.9/ios-aarch64/vec0.dylib"),
    ),
    #[cfg(all(target_os = "ios", target_arch = "aarch64", target_abi = "sim"))]
    (
        "sqlite-vec/v0.1.9/iossimulator-aarch64/vec0.dylib",
        include_bytes!("../resources/sqlite-vec/v0.1.9/iossimulator-aarch64/vec0.dylib"),
    ),
    #[cfg(all(target_os = "ios", target_arch = "x86_64"))]
    (
        "sqlite-vec/v0.1.9/iossimulator-x86_64/vec0.dylib",
        include_bytes!("../resources/sqlite-vec/v0.1.9/iossimulator-x86_64/vec0.dylib"),
    ),
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    (
        "sqlite-vec/v0.1.9/linux-aarch64/vec0.so",
        include_bytes!("../resources/sqlite-vec/v0.1.9/linux-aarch64/vec0.so"),
    ),
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    (
        "sqlite-vec/v0.1.9/linux-x86_64/vec0.so",
        include_bytes!("../resources/sqlite-vec/v0.1.9/linux-x86_64/vec0.so"),
    ),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    (
        "sqlite-vec/v0.1.9/macos-aarch64/vec0.dylib",
        include_bytes!("../resources/sqlite-vec/v0.1.9/macos-aarch64/vec0.dylib"),
    ),
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    (
        "sqlite-vec/v0.1.9/macos-x86_64/vec0.dylib",
        include_bytes!("../resources/sqlite-vec/v0.1.9/macos-x86_64/vec0.dylib"),
    ),
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    (
        "sqlite-vec/v0.1.9/windows-x86_64/vec0.dll",
        include_bytes!("../resources/sqlite-vec/v0.1.9/windows-x86_64/vec0.dll"),
    ),
];
#[cfg(not(feature = "bundled-native"))]
pub(crate) const FILES: &[(&str, &[u8])] = &[];
