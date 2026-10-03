#!/bin/bash
# Runs the SDK's cargo on the host for the bench crate: bench/run.sh build --release
# From outside the repo, so the app's vendored-sources config doesn't apply; network for fetching.
set -o pipefail
B="$HOME/Projects/ferret/bench"
host-spawn flatpak run --share=network --filesystem="$HOME/Projects/ferret" --command=sh org.gnome.Sdk//50 -c \
    'cd ~ && CARGO_HOME="$0/cargo-home" PATH=/usr/lib/sdk/rust-stable/bin:$PATH exec cargo "$@" --manifest-path "$0/Cargo.toml"' \
    "$B" "$@" </dev/null | tr -d '\r'
