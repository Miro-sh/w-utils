#!/bin/sh
# Publie/met à jour le paquet AUR w-utils-bin pour un tag donné.
#
#   TAG=v0.1.7 AUR_SSH_PRIVATE_KEY=<clé> script/publish-aur.sh
#
# Prérequis : compte AUR avec la clé publique correspondante enregistrée.
# Le premier push crée le paquet sur l'AUR.
set -eu

REPO="Miro-sh/w-utils"
PKG="w-utils-bin"
TAG="${TAG:?variable TAG requise (ex: v0.1.7)}"
VERSION="${TAG#v}"
AUR_SSH_PRIVATE_KEY="${AUR_SSH_PRIVATE_KEY:?clé privée SSH AUR requise}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL "https://github.com/$REPO/releases/download/$TAG/SHA256SUMS.txt" -o "$tmp/sums"
sha() { grep " $1\$" "$tmp/sums" | cut -d' ' -f1; }
SHA_X64=$(sha "w-utils-x86_64-unknown-linux-musl.tar.gz")
SHA_ARM=$(sha "w-utils-aarch64-unknown-linux-musl.tar.gz")

cat > "$tmp/PKGBUILD" <<EOF
# Maintainer: Miro-sh <https://github.com/Miro-sh>
pkgname=$PKG
pkgver=$VERSION
pkgrel=1
pkgdesc="Unix command-line tools rewritten in Rust (wcp: cp with a progress bar)"
arch=('x86_64' 'aarch64')
url="https://github.com/$REPO"
license=('MIT')
provides=('w-utils' 'wcp')
conflicts=('w-utils')
source_x86_64=("\$url/releases/download/$TAG/w-utils-x86_64-unknown-linux-musl.tar.gz")
source_aarch64=("\$url/releases/download/$TAG/w-utils-aarch64-unknown-linux-musl.tar.gz")
sha256sums_x86_64=('$SHA_X64')
sha256sums_aarch64=('$SHA_ARM')

package() {
    install -Dm755 wcp "\$pkgdir/usr/bin/wcp"
    install -Dm644 README.md "\$pkgdir/usr/share/doc/w-utils/README"
    install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/\$pkgname/LICENSE"
    if [ -f wcp.1.gz ]; then
        install -Dm644 wcp.1.gz "\$pkgdir/usr/share/man/man1/wcp.1.gz"
    fi
}
EOF

cat > "$tmp/.SRCINFO" <<EOF
pkgbase = $PKG
	pkgdesc = Unix command-line tools rewritten in Rust (wcp: cp with a progress bar)
	pkgver = $VERSION
	pkgrel = 1
	url = https://github.com/$REPO
	arch = x86_64
	arch = aarch64
	license = MIT
	provides = w-utils
	provides = wcp
	conflicts = w-utils
	source_x86_64 = https://github.com/$REPO/releases/download/$TAG/w-utils-x86_64-unknown-linux-musl.tar.gz
	sha256sums_x86_64 = $SHA_X64
	source_aarch64 = https://github.com/$REPO/releases/download/$TAG/w-utils-aarch64-unknown-linux-musl.tar.gz
	sha256sums_aarch64 = $SHA_ARM

pkgname = $PKG
EOF

mkdir -p ~/.ssh
printf '%s\n' "$AUR_SSH_PRIVATE_KEY" > ~/.ssh/aur
chmod 600 ~/.ssh/aur
ssh-keyscan aur.archlinux.org >> ~/.ssh/known_hosts 2>/dev/null
export GIT_SSH_COMMAND="ssh -i ~/.ssh/aur -o IdentitiesOnly=yes"

if git clone "ssh://aur@aur.archlinux.org/$PKG.git" "$tmp/aur" 2>/dev/null; then
    echo "AUR: paquet existant cloné"
else
    echo "AUR: premier push, initialisation du dépôt"
    mkdir -p "$tmp/aur"
    git -C "$tmp/aur" init
    git -C "$tmp/aur" remote add origin "ssh://aur@aur.archlinux.org/$PKG.git"
fi

cp "$tmp/PKGBUILD" "$tmp/.SRCINFO" "$tmp/aur/"
cd "$tmp/aur"
git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
    echo "AUR: déjà à jour"
    exit 0
fi
git -c user.name="Miro-sh" -c user.email="Miro-sh@users.noreply.github.com" \
    commit -m "w-utils $VERSION"
git push -u origin master
echo "AUR: $PKG $VERSION publié"
