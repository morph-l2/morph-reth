//! Morph-Reth version metadata.
//!
//! Overrides reth's default version info so `--version` reports morph-reth's
//! own version, commit SHA, and build timestamp instead of the upstream reth
//! fork's values.

use reth_node_core::version::{RethCliVersionConsts, try_init_version_metadata};
use std::{borrow::Cow, env};

/// Initialise global version metadata for Morph-Reth.
///
/// Must be called once at startup, before any CLI parsing.
pub fn init_version_metadata() {
    try_init_version_metadata(version_metadata())
        .expect("Version metadata initialised more than once");
}

/// Build the [`RethCliVersionConsts`] for morph-reth using compile-time env vars
/// emitted by `build.rs`.
pub fn version_metadata() -> RethCliVersionConsts {
    RethCliVersionConsts {
        name_client: Cow::Borrowed("Morph-Reth"),
        cargo_pkg_version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        vergen_git_sha_long: Cow::Borrowed(env!("VERGEN_GIT_SHA")),
        vergen_git_sha: Cow::Borrowed(env!("VERGEN_GIT_SHA_SHORT")),
        vergen_build_timestamp: Cow::Borrowed(env!("VERGEN_BUILD_TIMESTAMP")),
        vergen_cargo_target_triple: Cow::Borrowed(env!("VERGEN_CARGO_TARGET_TRIPLE")),
        vergen_cargo_features: Cow::Borrowed(env!("VERGEN_CARGO_FEATURES")),
        short_version: Cow::Borrowed(env!("MORPH_SHORT_VERSION")),
        long_version: Cow::Owned(format!(
            "{}\n{}\n{}\n{}\n{}",
            env!("MORPH_LONG_VERSION_0"),
            env!("MORPH_LONG_VERSION_1"),
            env!("MORPH_LONG_VERSION_2"),
            env!("MORPH_LONG_VERSION_3"),
            env!("MORPH_LONG_VERSION_4"),
        )),
        build_profile_name: Cow::Borrowed(env!("MORPH_BUILD_PROFILE")),
        p2p_client_version: Cow::Borrowed(env!("MORPH_P2P_CLIENT_VERSION")),
        extra_data: Cow::Owned(format!("morph-reth/v{}/{}", env!("CARGO_PKG_VERSION"), env::consts::OS)),
    }
}
