#!/bin/sh
# Build bman for the Pi on your PC, the same way the release workflow does:
# in a Debian 12 container (glibc 2.36, like Raspberry Pi OS bookworm), so
# the binary runs on the Pi whatever distro the PC has.
#
#   ./cross-build.sh            # 64-bit Pi (aarch64), the default
#   ./cross-build.sh armv7      # 32-bit Raspberry Pi OS
#   ./cross-build.sh x86_64
#
# Then: python3 update-pi.py bman@<pi> --file dist/bman-aarch64-linux-gnu
#
# Needs podman (or docker). The Rust toolchain, crates and build output are
# kept between runs (volumes bman-rustup / bman-cargo, target/debian), so
# only the first build is slow.
set -eu

case "${1:-aarch64}" in
    aarch64) target=aarch64-unknown-linux-gnu; arch=arm64; cross=aarch64-linux-gnu; asset=bman-aarch64-linux-gnu ;;
    armv7) target=armv7-unknown-linux-gnueabihf; arch=armhf; cross=arm-linux-gnueabihf; asset=bman-armv7-linux-gnueabihf ;;
    x86_64) target=x86_64-unknown-linux-gnu; arch=amd64; cross=; asset=bman-x86_64-linux-gnu ;;
    *) echo "usage: $0 [aarch64|armv7|x86_64]" >&2; exit 1 ;;
esac

if command -v podman >/dev/null 2>&1; then
    engine=podman
elif command -v docker >/dev/null 2>&1; then
    engine=docker
else
    echo "Needs podman or docker (Void: sudo xbps-install -S podman)" >&2
    exit 1
fi

cd "$(dirname "$0")"
mkdir -p dist target/debian

# Same steps as .github/workflows/release.yml.
script=$(cat <<EOF
set -eu
export DEBIAN_FRONTEND=noninteractive
if [ -n "$cross" ]; then
    dpkg --add-architecture $arch
    extra="gcc-$cross g++-$cross libc6-dev:$arch"
fi
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl build-essential pkg-config \
    libasound2-dev:$arch \${extra:-} >/dev/null
if ! [ -x /root/.cargo/bin/cargo ]; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
export PATH=/root/.cargo/bin:\$PATH
rustup target add $target >/dev/null 2>&1
if [ -n "$cross" ]; then
    t=\$(echo $target | tr 'a-z-' 'A-Z_')
    export CARGO_TARGET_\${t}_LINKER=$cross-gcc
    export CC_\$(echo $target | tr '-' '_')=$cross-gcc
    export CXX_\$(echo $target | tr '-' '_')=$cross-g++
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_LIBDIR=/usr/lib/$cross/pkgconfig:/usr/share/pkgconfig
fi
export CARGO_PROFILE_RELEASE_STRIP=true
cargo build --release --locked --target $target --target-dir target/debian
cp target/debian/$target/release/bman dist/$asset
cd dist && sha256sum $asset > $asset.sha256
# docker runs as root: hand the files back (rootless podman's root is already you).
if [ "$engine" = docker ]; then chown -R $(id -u):$(id -g) /src/dist /src/target/debian; fi
EOF
)

echo "Building $asset in a Debian 12 container ($engine)..."
$engine run --rm \
    -v "$PWD":/src -w /src \
    -v bman-rustup:/root/.rustup -v bman-cargo:/root/.cargo \
    docker.io/library/debian:bookworm sh -c "$script"
echo "Built dist/$asset"
echo "Send it to the Pi: python3 update-pi.py bman@<pi> --file dist/$asset"
