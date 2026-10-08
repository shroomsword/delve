//! `sqlx::migrate!` reads the migrations directory at compile time; without
//! this, adding a migration file doesn't rebuild the crate.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
