fn main() {
    println!("cargo:rerun-if-changed=logd.rc");
    println!("cargo:rerun-if-changed=../../public/logd.ico");

    #[cfg(windows)]
    embed_resource::compile("logd.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed the logd icon");
}
