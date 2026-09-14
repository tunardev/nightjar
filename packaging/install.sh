#!/bin/sh
set -eu

REPO=tunardev/nightjar
RELEASES="${NIGHTJAR_BASE_URL:-https://github.com/$REPO/releases}"
VERSION="${NIGHTJAR_VERSION:-latest}"
FROM_SOURCE="build from source instead: cargo install --git https://github.com/$REPO nightjar-cli"

say() {
    printf 'install: %s\n' "$*"
}

die() {
    printf 'install: %s\n' "$*" >&2
    exit 1
}

require() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but is not on your PATH"
}

download() {
    curl --proto '=https' --tlsv1.2 --retry 3 --retry-connrefused -fsSL "$1" -o "$2" 2>/dev/null
}

architecture() {
    case "$(uname -m)" in
        arm64 | aarch64) printf aarch64 ;;
        x86_64 | amd64) printf x86_64 ;;
        *) die "no build exists for $(uname -m); $FROM_SOURCE" ;;
    esac
}

libc() {
    if command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
        printf gnu
    elif ldd --version 2>&1 | grep -qi glibc; then
        printf gnu
    else
        printf musl
    fi
}

target() {
    arch="$(architecture)"
    case "$(uname -s)" in
        Darwin)
            printf '%s-apple-darwin' "$arch"
            ;;
        Linux)
            flavour="$(libc)"
            [ "$flavour" = gnu ] || [ "$arch" = x86_64 ] \
                || die "no $arch build exists for a system without glibc; $FROM_SOURCE"
            printf '%s-unknown-linux-%s' "$arch" "$flavour"
            ;;
        *)
            die "no build exists for $(uname -s); $FROM_SOURCE"
            ;;
    esac
}

destination() {
    if [ -n "${NIGHTJAR_INSTALL_DIR:-}" ]; then
        printf '%s' "$NIGHTJAR_INSTALL_DIR"
    elif [ -w /usr/local/bin ]; then
        printf /usr/local/bin
    else
        printf '%s/.local/bin' "$HOME"
    fi
}

release_url() {
    case "$VERSION" in
        latest | LATEST) printf '%s/latest/download/%s' "$RELEASES" "$1" ;;
        *) printf '%s/download/v%s/%s' "$RELEASES" "${VERSION#v}" "$1" ;;
    esac
}

checksum_matches() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c --strict "$1.sha256" >/dev/null 2>&1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c --strict "$1.sha256" >/dev/null 2>&1
    else
        die "neither sha256sum nor shasum is here, so the download cannot be verified;" \
            "install one, or set NIGHTJAR_SKIP_CHECKSUM=1 to accept the risk"
    fi
}

verify() {
    url="$1"
    scratch="$2"
    asset="$3"

    if [ "${NIGHTJAR_SKIP_CHECKSUM:-0}" = 1 ]; then
        say "NIGHTJAR_SKIP_CHECKSUM is set, not verifying the download"
        return 0
    fi

    download "$url.sha256" "$scratch/$asset.sha256" \
        || die "nothing is published at $url.sha256, so the download cannot be verified;" \
               "set NIGHTJAR_SKIP_CHECKSUM=1 to accept the risk"
    (cd "$scratch" && checksum_matches "$asset") \
        || die "checksum mismatch: $asset is not the file that was published"
}

report() {
    dir="$1"
    say "installed $("$dir/nightjar" --version) to $dir/nightjar"
    case ":$PATH:" in
        *":$dir:"*) say 'run `nightjar doctor` to check this machine over' ;;
        *) say "$dir is not on your PATH; add it, then run \`nightjar doctor\`" ;;
    esac
}

main() {
    require curl
    require tar

    asset="nightjar-$(target).tar.gz"
    url="$(release_url "$asset")"

    dir="$(destination)"
    mkdir -p "$dir" 2>/dev/null || die "cannot create $dir"
    [ -w "$dir" ] \
        || die "$dir is not writable; set NIGHTJAR_INSTALL_DIR to a directory you own," \
               "or re-run under sudo"

    scratch="$(mktemp -d)"
    staged=
    trap 'rm -rf "$scratch" ${staged:+"$staged"}' EXIT HUP INT TERM

    say "downloading $url"
    download "$url" "$scratch/$asset" || die "download failed: $url"
    verify "$url" "$scratch" "$asset"

    tar -xzf "$scratch/$asset" -C "$scratch" nightjar \
        || die "$asset does not contain a nightjar binary"
    [ -f "$scratch/nightjar" ] && [ -s "$scratch/nightjar" ] \
        || die "$asset contains an empty nightjar binary"

    staged="$(mktemp "$dir/.nightjar.XXXXXXXX")" || die "cannot write to $dir"
    cat "$scratch/nightjar" > "$staged" || die "cannot write to $dir"
    chmod 755 "$staged"
    "$staged" --version >/dev/null 2>&1 \
        || die "the downloaded binary does not run on this machine; $FROM_SOURCE"

    mv -f "$staged" "$dir/nightjar"
    staged=
    report "$dir"
}

main "$@"
