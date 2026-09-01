#!/bin/sh
# Downloads a nightjar release binary and puts it on your PATH.
#   curl -fsSL https://nightjar.tunar.dev | sh
set -eu

REPO="tunardev/nightjar"
BASE_URL="${NIGHTJAR_BASE_URL:-https://github.com/$REPO/releases}"
VERSION="${NIGHTJAR_VERSION:-latest}"

die() {
    echo "install: $*" >&2
    exit 1
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$arch" in
        arm64 | aarch64) arch=aarch64 ;;
        x86_64 | amd64) arch=x86_64 ;;
        *) die "unsupported architecture: $arch" ;;
    esac

    case "$os" in
        Darwin) echo "$arch-apple-darwin" ;;
        Linux)
            # Only x86_64 has a static build to fall back to, so a glibc-less
            # aarch64 box has nothing that will run — say so rather than
            # installing a binary that cannot start.
            if ldd --version 2>&1 | grep -qi glibc; then
                echo "$arch-unknown-linux-gnu"
            elif [ "$arch" = x86_64 ]; then
                echo "x86_64-unknown-linux-musl"
            else
                die "no $arch build exists for a system without glibc; build from source with \`cargo install --git https://github.com/$REPO nightjar-cli\`"
            fi
            ;;
        *) die "unsupported operating system: $os" ;;
    esac
}

choose_dir() {
    if [ -n "${NIGHTJAR_INSTALL_DIR:-}" ]; then
        echo "$NIGHTJAR_INSTALL_DIR"
    elif [ -w /usr/local/bin ]; then
        echo /usr/local/bin
    else
        echo "$HOME/.local/bin"
    fi
}

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

target="$(detect_target)"
case "$VERSION" in
    latest) url="$BASE_URL/latest/download/nightjar-$target.tar.gz" ;;
    *) url="$BASE_URL/download/v${VERSION#v}/nightjar-$target.tar.gz" ;;
esac

dir="$(choose_dir)"
mkdir -p "$dir" || die "cannot create $dir"
[ -w "$dir" ] || die "$dir is not writable; set NIGHTJAR_INSTALL_DIR or re-run with sudo"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Keep the release asset's own filename — the checksum file records this
# name, not a generic one.
asset="nightjar-$target.tar.gz"
archive="$tmp/$asset"

echo "install: downloading $url"
curl -fsSL "$url" -o "$archive" || die "download failed: $url"

# Absent on any release built before this file existed; verify when
# present, warn rather than fail when it's missing.
if curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256" 2>/dev/null; then
    # --strict: without it, a checksum file with zero properly-formatted
    # lines (e.g. corrupted or replaced in transit) exits 0 with only a
    # warning, silently skipping verification instead of failing it.
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$tmp" && sha256sum -c --strict "$asset.sha256") >/dev/null || die "checksum verification failed"
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$tmp" && shasum -a 256 -c --strict "$asset.sha256") >/dev/null || die "checksum verification failed"
    fi
else
    echo "install: no checksum published for this release, skipping verification"
fi

tar -xzf "$archive" -C "$tmp" nightjar || die "archive did not contain a nightjar binary"

# Replace by rename so an install over a running daemon's binary cannot hand
# it a half-written file. The staging copy lives in the target directory:
# a rename is only atomic within one filesystem, and $tmp is usually not on
# the same one as /usr/local/bin.
staged="$dir/.nightjar.installing.$$"
cp "$tmp/nightjar" "$staged" || die "cannot write to $dir"
chmod +x "$staged"
mv -f "$staged" "$dir/nightjar"

echo "install: installed $("$dir/nightjar" --version) to $dir/nightjar"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "install: $dir is not on your PATH" ;;
esac
