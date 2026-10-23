#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only

# Prints the section of CHANGELOG.md for version $1 (as "0.2.0"): the notes of that version's release.
# Fails, saying why, when Cargo.toml is at another version or the changelog has no such section, so a
# tag never publishes a release without notes or with the wrong version.
set -euo pipefail

version="$1"
cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1 | tr -d '\r')
if [ "$cargo" != "$version" ]; then
    echo "Cargo.toml is at version $cargo, the tag at $version: set the version before tagging" >&2
    exit 1
fi
# From "## <version>" (alone or followed by its date) to the next "## ".
notes=$(awk -v v="$version" '{ sub(/\r$/, "") } /^## / { if (found) exit; found = ($0 == "## " v || index($0, "## " v " ") == 1); next } found' CHANGELOG.md)
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section \"## $version\": write the notes before tagging" >&2
    exit 1
fi
printf '%s\n' "$notes"
