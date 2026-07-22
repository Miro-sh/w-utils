#!/bin/sh
# wcp installer — downloads the latest release binary and installs it.
#
#   curl -sSfL https://raw.githubusercontent.com/Miro-sh/w-utils/main/install.sh | sh
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
    curl -sSfL "$url" -o "$tmp/$asset"
elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$tmp/$asset"
else
    echo "w-utils: curl or wget is required" >&2
    exit 1
fi

tar -xzf "$tmp/$asset" -C "$tmp"
pkg_dir="$tmp/w-utils-${arch_part}-${os_part}"
bin="$pkg_dir/wcp"

# Installe la page man si l'archive la contient (Linux seulement).
install_man() {
    [ -f "$pkg_dir/wcp.1.gz" ] || return 0
    if [ -w /usr/local/share/man/man1 ] 2>/dev/null || { [ -d /usr/local/share/man ] && [ -w /usr/local/share/man ]; }; then
        mkdir -p /usr/local/share/man/man1
        install -m 644 "$pkg_dir/wcp.1.gz" /usr/local/share/man/man1/
    elif command -v sudo >/dev/null 2>&1; then
        sudo mkdir -p /usr/local/share/man/man1
        sudo install -m 644 "$pkg_dir/wcp.1.gz" /usr/local/share/man/man1/
    else
        mkdir -p "$HOME/.local/share/man/man1"
        install -m 644 "$pkg_dir/wcp.1.gz" "$HOME/.local/share/man/man1/"
        echo "w-utils: man page installed to ~/.local/share/man (add it to MANPATH if 'man wcp' fails)"
    fi
}

if [ -n "${WCP_INSTALL_DIR:-}" ]; then
    dest="$WCP_INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
    dest="/usr/local/bin"
elif command -v sudo >/dev/null 2>&1; then
    echo "w-utils: installing to /usr/local/bin (sudo)"
    sudo install -m 755 "$bin" /usr/local/bin/wcp
    install_man
    echo "w-utils: installed, run 'wcp --help' or 'man wcp' to get started"
    exit 0
else
    dest="$HOME/.local/bin"
fi

mkdir -p "$dest"
install -m 755 "$bin" "$dest/wcp"
install_man

case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "w-utils: note: $dest is not in your PATH" ;;
esac

echo "w-utils: installed to $dest/wcp, run 'wcp --help' to get started"
