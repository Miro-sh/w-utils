#!/bin/sh
# Met à jour la formule Homebrew dans Miro-sh/homebrew-tap pour un tag donné.
#
#   TAG=v0.1.7 GH_TOKEN=<pat> script/update-homebrew-tap.sh
#
# En CI, GH_TOKEN = secret HOMEBREW_TAP_TOKEN (PAT avec scope repo).
set -eu

REPO="Miro-sh/w-utils"
TAP="Miro-sh/homebrew-tap"
TAG="${TAG:?variable TAG requise (ex: v0.1.7)}"
VERSION="${TAG#v}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -sSfL "https://github.com/$REPO/releases/download/$TAG/SHA256SUMS.txt" -o "$tmp/SHA256SUMS.txt"

sha() { grep " $1\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1; }

SHA_MAC_ARM=$(sha "w-utils-aarch64-apple-darwin.tar.gz")
SHA_MAC_INTEL=$(sha "w-utils-x86_64-apple-darwin.tar.gz")
SHA_LINUX_ARM=$(sha "w-utils-aarch64-unknown-linux-musl.tar.gz")
SHA_LINUX_X64=$(sha "w-utils-x86_64-unknown-linux-musl.tar.gz")

mkdir -p "$tmp/f"
cat > "$tmp/f/w-utils.rb" <<EOF
class WUtils < Formula
  desc "Unix command-line tools rewritten in Rust (wcp: cp with a progress bar)"
  homepage "https://github.com/$REPO"
  version "$VERSION"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/$REPO/releases/download/$TAG/w-utils-aarch64-apple-darwin.tar.gz"
      sha256 "$SHA_MAC_ARM"
    else
      url "https://github.com/$REPO/releases/download/$TAG/w-utils-x86_64-apple-darwin.tar.gz"
      sha256 "$SHA_MAC_INTEL"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/$REPO/releases/download/$TAG/w-utils-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$SHA_LINUX_ARM"
    else
      url "https://github.com/$REPO/releases/download/$TAG/w-utils-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$SHA_LINUX_X64"
    end
  end

  def install
    bin.install "wcp"
    man1.install "wcp.1.gz" if File.exist? "wcp.1.gz"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/wcp --version")
  end
end
EOF

if [ -n "${GH_TOKEN:-}" ]; then
    clone_url="https://x-access-token:${GH_TOKEN}@github.com/$TAP"
else
    clone_url="https://github.com/$TAP"
fi
git clone "$clone_url" "$tmp/tap"
mkdir -p "$tmp/tap/Formula"
cp "$tmp/f/w-utils.rb" "$tmp/tap/Formula/w-utils.rb"

cd "$tmp/tap"
git add Formula/w-utils.rb
if git diff --cached --quiet; then
    echo "homebrew-tap: formule déjà à jour"
    exit 0
fi
git -c user.name="github-actions[bot]" \
    -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    commit -m "w-utils $VERSION"
git push
echo "homebrew-tap: w-utils $VERSION publié (brew install Miro-sh/tap/w-utils)"
