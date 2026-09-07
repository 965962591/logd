fn main() {
    println!("cargo:rerun-if-changed=logd.rc");
    println!("cargo:rerun-if-changed=../../public/logd.ico");

    #[cfg(windows)]
    {
        let version = [
            format!("LOGD_VERSION_MAJOR={}", env!("CARGO_PKG_VERSION_MAJOR")),
            format!("LOGD_VERSION_MINOR={}", env!("CARGO_PKG_VERSION_MINOR")),
            format!("LOGD_VERSION_PATCH={}", env!("CARGO_PKG_VERSION_PATCH")),
        ];
        embed_resource::compile("logd.rc", &version)
            .manifest_optional()
            .expect("failed to embed the logd Windows resources");
    }
}
