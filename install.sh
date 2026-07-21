#!/bin/sh
# bcp installer — downloads the latest release binary and installs it.
#
#   curl -sSfL https://raw.githubusercontent.com/Miro-sh/better-cp/main/install.sh | sh
#
# Env vars: BCP_INSTALL_DIR (default: /usr/local/bin, or ~/.local/bin without sudo)

set -eu

REPO="Miro-sh/better-cp"
BASE_URL="https://github.com/$REPO/releases/latest/download"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *)      echo "bcp: unsupported OS: $os (download manually from https://github.com/$REPO/releases)" >&2; exit 1 ;;
esac

case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *)             echo "bcp: unsupported architecture: $arch" >&2; exit 1 ;;
esac

asset="bcp-${arch_part}-${os_part}.tar.gz"
url="$BASE_URL/$asset"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "bcp: downloading $url"
if command -v curl >/dev/null 2>&1; then
    curl -sSfL "$url" -o "$tmp/$asset"
elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$tmp/$asset"
else
    echo "bcp: curl or wget is required" >&2
    exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
pkg_dir="$tmp/bcp-${arch_part}-${os_part}"
bin="$pkg_dir/bcp"

# Installe la page man si l'archive la contient (Linux seulement).
install_man() {
    [ -f "$pkg_dir/bcp.1.gz" ] || return 0
    if [ -w /usr/local/share/man/man1 ] 2>/dev/null || { [ -d /usr/local/share/man ] && [ -w /usr/local/share/man ]; }; then
        mkdir -p /usr/local/share/man/man1
        install -m 644 "$pkg_dir/bcp.1.gz" /usr/local/share/man/man1/
    elif command -v sudo >/dev/null 2>&1; then
        sudo mkdir -p /usr/local/share/man/man1
        sudo install -m 644 "$pkg_dir/bcp.1.gz" /usr/local/share/man/man1/
    else
        mkdir -p "$HOME/.local/share/man/man1"
        install -m 644 "$pkg_dir/bcp.1.gz" "$HOME/.local/share/man/man1/"
        echo "bcp: page man installée dans ~/.local/share/man (ajoutez-le à MANPATH si 'man bcp' échoue)"
    fi
}

if [ -n "${BCP_INSTALL_DIR:-}" ]; then
    dest="$BCP_INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
    dest="/usr/local/bin"
elif command -v sudo >/dev/null 2>&1; then
    echo "bcp: installing to /usr/local/bin (sudo)"
    sudo install -m 755 "$bin" /usr/local/bin/bcp
    install_man
    echo "bcp: installed, run 'bcp --help' or 'man bcp' to get started"
    exit 0
else
    dest="$HOME/.local/bin"
fi

mkdir -p "$dest"
install -m 755 "$bin" "$dest/bcp"
install_man

case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "bcp: note: $dest is not in your PATH" ;;
esac

echo "bcp: installed to $dest/bcp, run 'bcp --help' to get started"
