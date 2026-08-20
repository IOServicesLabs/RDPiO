# Swarm Workspace Log

- **2026-08-20T08:59:09.646551100-05:00** `progress`: step-7-embed-icon: added [build-dependencies] embed-resource = "2" to crates/rdp-client/Cargo.toml; created res/rdpio.rc (1 ICON "rdpio.ico"); build.rs now calls embed_resource::compile("res/rdpio.rc", embed_resource::NONE). Verified via PE resource-dir parse: RT_GROUP_ICON id 1 + RT_ICON ids 1-8 present, no RT_VERSION/RT_MANIFEST. rdpio.ico untouched. cargo build --workspace and cargo test --workspace both green.
