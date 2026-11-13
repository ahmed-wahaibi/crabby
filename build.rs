fn main() {
    // include_dir! does not tell cargo about the files it embeds.
    println!("cargo:rerun-if-changed=agents/categories");
}
