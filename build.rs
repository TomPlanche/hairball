// Rebuild when a migration changes: `sqlx::migrate!()` embeds the files at compile time.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
