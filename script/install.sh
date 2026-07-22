#!/bin/sh
# wcp installer — downloads the latest release binary and installs it.
#
#   curl -sSfL https://raw.githubusercontent.com/Miro-sh/w-utils/main/script/install.sh | sh
#
# Env vars: WCP_INSTALL_DIR (default: /usr/local/bin, or ~/.local/bin without sudo)

set -eu

REPO="Miro-sh/w-utils"
BASE_URL="https://github.com/$REPO/releases/latest/download"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *)      echo "w-utils: unsupported OS: $os (download manually from https://github.com/$REPO/releases)" >&2; exit 1 ;;
esac

case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *)             echo "w-utils: unsupported architecture: $arch" >&2; exit 1 ;;
esac

asset="w-utils-${arch_part}-${os_part}.tar.gz"
url="$BASE_URL/$asset"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "w-utils: downloading $url"
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -sSfL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q "$1" -O "$2"; }
else
    echo "w-utils: curl or wget is required" >&2
    exit 1
fi
fetch "$url" "$tmp/$asset"

# Vérification SHA256 si la release publie SHA256SUMS.txt (sinon on continue).
if fetch "$BASE_URL/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$tmp" && grep " $asset\$" SHA256SUMS.txt | sha256sum -c -)
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$tmp" && grep " $asset\$" SHA256SUMS.txt | shasum -a 256 -c -)
    else
        echo "w-utils: no sha256sum/shasum found, skipping checksum verification" >&2
    fi
else
    echo "w-utils: no checksums in this release, skipping verification" >&2
fi

tar -xzf "$tmp/$asset" -C "$tmp"
pkg_dir="$tmp/w-utils-${arch_part}-${os_part}"
bin="$pkg_dir/wcp"

# Installe la page man si l'archive la contient (Linux seulement).
# $1: "system" (/usr/local/share/man) ou "user" (~/.local/share/man)
# $2: "" ou "sudo"
install_man() {
    [ -f "$pkg_dir/wcp.1.gz" ] || return 0
    if [ "$1" = system ]; then
        mandir=/usr/local/share/man/man1
    else
        mandir="$HOME/.local/share/man/man1"
    fi
    $2 mkdir -p "$mandir"
    $2 install -m 644 "$pkg_dir/wcp.1.gz" "$mandir/"
    if [ "$1" = user ]; then
        echo "w-utils: man page installed to $mandir (add it to MANPATH if 'man wcp' fails)"
    fi
}

if [ -n "${WCP_INSTALL_DIR:-}" ]; then
    dest="$WCP_INSTALL_DIR"
    mode=user
elif [ -w /usr/local/bin ]; then
    dest="/usr/local/bin"
    mode=system
elif command -v sudo >/dev/null 2>&1; then
    echo "w-utils: installing to /usr/local/bin (sudo)"
    sudo install -m 755 "$bin" /usr/local/bin/wcp
    install_man system sudo
    echo "w-utils: installed, run 'wcp --help' or 'man wcp' to get started"
    exit 0
else
    dest="$HOME/.local/bin"
    mode=user
fi

mkdir -p "$dest"
install -m 755 "$bin" "$dest/wcp"
install_man "$mode" ""

case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "w-utils: note: $dest is not in your PATH" ;;
esac

echo "w-utils: installed to $dest/wcp, run 'wcp --help' to get started"
