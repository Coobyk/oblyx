#!/usr/bin/env bash
# Push all oblyx packages to the AUR for a release tag.
#
# Expects: git, curl, ssh configured with an AUR-authorized key, and makepkg
# (run inside an archlinux container). Runs in CI on pushes to main and tags.
#
# Usage: publish-aur.sh <tag>
set -euo pipefail

tag="$1"
ver="${tag#v}"
repo_root="ssh://aur@aur.archlinux.org"

src_urls() {
  case "$1" in
    oblyx-bin)
      echo "https://github.com/${GITHUB_REPOSITORY}/releases/download/${tag}/oblyx-${ver}-linux-x64.tar.gz"
      ;;
    oblyx-gui-bin)
      echo "https://github.com/${GITHUB_REPOSITORY}/releases/download/${tag}/oblyx-${ver}-linux-x64.tar.gz"
      echo "https://raw.githubusercontent.com/${GITHUB_REPOSITORY}/${tag}/assets/oblyx-gui.desktop"
      echo "https://raw.githubusercontent.com/${GITHUB_REPOSITORY}/${tag}/assets/oblyx.svg"
      ;;
    *)
      echo "https://github.com/${GITHUB_REPOSITORY}/archive/refs/tags/${tag}.tar.gz"
      ;;
  esac
}

for pkg in oblyx oblyx-bin oblyx-gui oblyx-gui-bin; do
  work="$(mktemp -d)"

  # Fetch every source[] entry and build the sha256sums list in order.
  sums=""
  i=0
  while IFS= read -r url; do
    i=$((i + 1))
    echo "==> $pkg: fetch $url"
    curl -fsSL "$url" -o "$work/source.$i"
    sums+=" '$(sha256sum "$work/source.$i" | awk '{print $1}')'"
  done < <(src_urls "$pkg")

  sed -e "s/@PKGVER@/${ver}/g" -e "s/@SHA256@/${sums# }/g" \
      "aur/${pkg}/PKGBUILD" > "$work/PKGBUILD"
  chmod -R a+rwX "$work"

  # makepkg refuses to run as root; regenerate .SRCINFO unprivileged.
  if [ "$(id -u)" -eq 0 ]; then
    id builder &>/dev/null || useradd -m builder
    su builder -s /bin/sh -c "cd '$work' && makepkg --printsrcinfo" > "$work/.SRCINFO"
  else
    (cd "$work" && makepkg --printsrcinfo > .SRCINFO)
  fi

  # First push to a package repo registers it; clone only if it exists.
  repo="${repo_root}/${pkg}.git"
  if ! git clone "$repo" "$work/aur" 2>/dev/null; then
    mkdir "$work/aur"
    git -C "$work/aur" init
    git -C "$work/aur" remote add origin "$repo"
  fi
  cp "$work/PKGBUILD" "$work/.SRCINFO" "$work/aur/"
  git -C "$work/aur" add PKGBUILD .SRCINFO
  if git -C "$work/aur" diff --cached --quiet && git -C "$work/aur" rev-parse --verify HEAD &>/dev/null; then
    echo "==> $pkg already up to date on AUR"
  else
    git -C "$work/aur" commit -m "$pkg $ver" >/dev/null
    git -C "$work/aur" push origin HEAD:master
    echo "==> $pkg $ver published"
  fi
  rm -rf "$work"
done
