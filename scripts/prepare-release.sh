#!/usr/bin/env bash
set -euo pipefail

version="${1:?release version is required}"

perl -0pi -e 's/^version = "[^"]+"/version = "'"${version}"'"/m' Cargo.toml
perl -0pi -e 's/^version: .*/version: '"${version}"'/m; s/^appVersion: .*/appVersion: "'"${version}"'"/m' charts/debug-operator/Chart.yaml

cargo check --quiet --all-targets
