fn main() {
    embuild::espidf::sysenv::output();
    ensure_stdcxx_after_component_libs();
}

/// esp-dl (pulled in by esp-sr >= 2.6) registers its prebuilt C++ helper
/// library (`libfbs_model.a`) so late in the ESP-IDF link line that it ends up
/// *after* the `-lstdc++` that IDF's `cxx` component emits. The archive then
/// still has unresolved `std::_Hash_bytes` / `_Prime_rehash_policy` references
/// and the link fails with "undefined reference to std::_Hash_bytes".
///
/// Appending `-lstdc++` once more at the very end of `linker_args.txt` (which
/// this crate's build script runs after `esp-idf-sys` produced it, and before
/// rustc performs the link) restores the archive's symbol resolution.
fn ensure_stdcxx_after_component_libs() {
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let out_dir = std::path::PathBuf::from(out_dir);
    // .../build/esp32s3-voice-idf-<hash>/out -> .../build
    let Some(build_dir) = out_dir.parent().and_then(|p| p.parent()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(build_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("esp-idf-sys-") {
            continue;
        }
        let args_file = entry.path().join("out").join("linker_args.txt");
        let Ok(current) = std::fs::read_to_string(&args_file) else {
            continue;
        };
        let trimmed = current.trim_end();
        if trimmed.ends_with("-lstdc++") {
            continue;
        }
        if std::fs::write(&args_file, format!("{trimmed} -lstdc++\n")).is_ok() {
            println!(
                "cargo:warning=esp-sr 2.6 workaround: appended -lstdc++ to {}",
                args_file.display()
            );
        }
    }
}
