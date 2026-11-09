#!/usr/bin/env bash
# Build a release tarball: dist/wang-<arch>-linux.tar.gz (+ .sha256).
# Contents: wang (CLI + daemon + sandbox helper), wang-proxy, jail, wang-vm-agent (static, for VM images).
set -euo pipefail
cd "$(dirname "$0")/.."
arch="$(uname -m)"
version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
name="wang-${arch}-linux"
out="dist/${name}"

cargo build --release -p wang-cli -p wang-proxy --bin wang --bin wang-proxy
cargo build --release -p wang-jail --bin jail
# The guest agent runs inside VM images built from any Docker image: link it fully static.
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target "${arch}-unknown-linux-gnu" -p wang-vm-agent

rm -rf "${out}" && mkdir -p "${out}"
cp target/release/wang target/release/wang-proxy target/release/jail "${out}/"
cp "target/${arch}-unknown-linux-gnu/release/wang-vm-agent" "${out}/"
cp README.md "${out}/"
echo "${version}" > "${out}/VERSION"
tar -C dist -czf "dist/${name}.tar.gz" "${name}"
(cd dist && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256")
echo "dist/${name}.tar.gz ($(du -h "dist/${name}.tar.gz" | cut -f1)), version ${version}"
