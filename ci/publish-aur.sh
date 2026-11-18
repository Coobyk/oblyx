#!/usr/bin/env bash
# Push one package (oblyx or oblyx-bin) to the AUR.
#
# Expects: git, curl, ssh configured with an AUR-authorized key, and makepkg
# (run inside an archlinux container). Rendered in CI on tag pushes.
#
# Usage: publish-aur.sh <pkgbase> <tag>
set -euo pipefail

pkg="$1"
tag="$2"
ver="${tag#v}"
repo="ssh://aur@aur.archlinux.org/${pkg}.git"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

case "$pkg" in
  oblyx-bin)
    url="https://github.com/${GITHUB_REPOSITORY}/releases/download/${tag}/oblyx-linux-x64.tar.gz"
    ;;
  oblyx)
    url="https://github.com/${GITHUB_REPOSITORY}/archive/refs/tags/${tag}.tar.gz"
    ;;
  *)
    echo "unknown package: $pkg" >&2
    exit 1
    ;;
esac

echo "==> $pkg: fetch $url"
curl -fsSL "$url" -o "$work/source.tar.gz"
sha256="$(sha256sum "$work/source.tar.gz" | awk '{print $1}')"

sed -e "s/@PKGVER@/${ver}/g" -e "s/@SHA256@/${sha256}/g" \
    "aur/${pkg}/PKGBUILD" > "$work/PKGBUILD"
chmod -R a+rwX "$work"

# makepkg refuses to run as root; regenerate .SRCINFO as an unprivileged user.
if [ "$(id -u)" -eq 0 ]; then
  id builder &>/dev/null || useradd -m builder
  su builder -s /bin/sh -c "cd '$work' && makepkg --printsrcinfo" > "$work/.SRCINFO"
else
  (cd "$work" && makepkg --printsrcinfo > .SRCINFO)
fi

# First push to a package repo registers it; clone only if it already exists.
if ! git clone "$repo" "$work/aur" 2>/dev/null; then
  mkdir "$work/aur"
  git -C "$work/aur" init
  git -C "$work/aur" remote add origin "$repo"
fi
cp "$work/PKGBUILD" "$work/.SRCINFO" "$work/aur/"
git -C "$work/aur" add PKGBUILD .SRCINFO
if git -C "$work/aur" diff --cached --quiet && git -C "$work/aur" rev-parse --verify HEAD &>/dev/null; then
  echo "==> $pkg already up to date on AUR"
  exit 0
fi
git -C "$work/aur" commit -m "$pkg $ver"
git -C "$work/aur" push origin HEAD:master
echo "==> $pkg $ver published"
