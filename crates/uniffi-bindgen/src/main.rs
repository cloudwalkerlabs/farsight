//! uniffi's binding generator, run by the Android build in library mode to
//! produce the Kotlin bindings for `farsight-android`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
