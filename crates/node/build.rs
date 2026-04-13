#![allow(missing_docs)]

use std::{env, error::Error};
use vergen::{BuildBuilder, CargoBuilder, Emitter};
use vergen_git2::Git2Builder;

fn main() -> Result<(), Box<dyn Error>> {
    let mut emitter = Emitter::default();

    let build_builder = BuildBuilder::default().build_timestamp(true).build()?;
    emitter.add_instructions(&build_builder)?;

    let cargo_builder = CargoBuilder::default().features(true).target_triple(true).build()?;
    emitter.add_instructions(&cargo_builder)?;

    let git_builder =
        Git2Builder::default().describe(false, true, None).dirty(true).sha(false).build()?;
    emitter.add_instructions(&git_builder)?;

    emitter.emit_and_set()?;

    let sha = env::var("VERGEN_GIT_SHA")?;
    let sha_short = &sha[0..7];

    let is_dirty = env::var("VERGEN_GIT_DIRTY")? == "true";
    // if not on a tag: v0.2.1-4-gabcdef1
    // if on a tag:     v0.2.1
    let not_on_tag =
        env::var("VERGEN_GIT_DESCRIBE")?.ends_with(&format!("-g{sha_short}"));
    let version_suffix = if is_dirty || not_on_tag { "-dev" } else { "" };

    println!("cargo:rustc-env=MORPH_VERSION_SUFFIX={version_suffix}");
    println!("cargo:rustc-env=VERGEN_GIT_SHA_SHORT={}", &sha[..8]);

    let out_dir = env::var("OUT_DIR").unwrap();
    let profile = out_dir.rsplit(std::path::MAIN_SEPARATOR).nth(3).unwrap();
    println!("cargo:rustc-env=MORPH_BUILD_PROFILE={profile}");

    let pkg_version = env!("CARGO_PKG_VERSION");

    // Short: "0.2.1 (c957148)"
    println!("cargo:rustc-env=MORPH_SHORT_VERSION={pkg_version}{version_suffix} ({sha_short})");

    // Long version lines (matches reth's RETH_LONG_VERSION_* naming so we can reuse
    // RethCliVersionConsts without patching upstream)
    println!("cargo:rustc-env=MORPH_LONG_VERSION_0=Version: {pkg_version}{version_suffix}");
    println!("cargo:rustc-env=MORPH_LONG_VERSION_1=Commit SHA: {sha}");
    println!(
        "cargo:rustc-env=MORPH_LONG_VERSION_2=Build Timestamp: {}",
        env::var("VERGEN_BUILD_TIMESTAMP")?
    );
    println!(
        "cargo:rustc-env=MORPH_LONG_VERSION_3=Build Features: {}",
        env::var("VERGEN_CARGO_FEATURES")?
    );
    println!("cargo:rustc-env=MORPH_LONG_VERSION_4=Build Profile: {profile}");

    // P2P client version: morph-reth/v0.2.1-c957148/x86_64-unknown-linux-gnu
    println!(
        "cargo:rustc-env=MORPH_P2P_CLIENT_VERSION=morph-reth/v{pkg_version}-{sha_short}/{}",
        env::var("VERGEN_CARGO_TARGET_TRIPLE")?
    );

    Ok(())
}
