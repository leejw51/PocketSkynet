//! Make the compiled-in deployment settings actually recompile.
//!
//! `routes::misc` reads these with `option_env!`, which is resolved when the
//! crate is compiled. Cargo does not know that, so without the directives below
//! a rebuild after changing one of them is a no-op: the release would silently
//! ship whatever value happened to be present the first time the crate was
//! built, and the failure mode — a desktop app that reports "no payment wallet
//! configured" while CI insists the secret is set — points nowhere near here.
//!
//! Listing a variable here is also the definition of which settings a build may
//! carry. Anything absent is runtime-only.

fn main() {
    for key in [
        "VITE_FRUITNATION_WALLET",
        "VITE_FRUITNATION_HASH_CRO",
        "VITE_CHAIN_ID",
        "VITE_PRIVY_APPID",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
