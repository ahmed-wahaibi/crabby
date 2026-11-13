fn main() {
    // include_dir! does not tell cargo about the files it embeds.
    println!("cargo:rerun-if-changed=agents/categories");
    // The commit goes into crash reports; "unknown" outside a git checkout.
    println!("cargo:rerun-if-changed=.git/HEAD");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=CRABBY_COMMIT={commit}");
}
