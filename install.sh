#!/usr/bin/env bash
# Installs the ripple binary from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/saugatkhadka/stud-finder/main/install.sh | bash
#
# Environment:
#   RIPPLE_VERSION      release tag to install (default: latest), e.g. ripple-v0.1.0
#   RIPPLE_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   RIPPLE_REPO         owner/name to download from (default: saugatkhadka/stud-finder)
#   RIPPLE_NO_SKILL=1   do not print the agent-skill hint
#
# It downloads one tarball and its SHA-256 file, verifies the checksum, and
# copies one binary. It does not use sudo and changes nothing else.
set -euo pipefail

repo="${RIPPLE_REPO:-saugatkhadka/stud-finder}"
dir="${RIPPLE_INSTALL_DIR:-$HOME/.local/bin}"
version="${RIPPLE_VERSION:-latest}"

say() { printf 'ripple-install: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) die "unsupported OS $(uname -s); build from source: cargo install --git https://github.com/$repo --branch main ripple-cli" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch="aarch64" ;;
  x86_64 | amd64) arch="x86_64" ;;
  *) die "unsupported CPU $(uname -m); build from source: cargo install --git https://github.com/$repo --branch main ripple-cli" ;;
esac
target="$arch-$os"
asset="ripple-$target.tar.gz"

if [ "$version" = "latest" ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset ($version)"
curl -fsSL "$base/$asset" -o "$tmp/$asset" ||
  die "no release asset for $target at $base. Build from source: cargo install --git https://github.com/$repo --branch main ripple-cli"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || die "checksum file missing for $asset"

want="$(awk '{print $1}' "$tmp/$asset.sha256")"
if command -v shasum >/dev/null 2>&1; then
  got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else
  die "need shasum or sha256sum to verify the download"
fi
[ "$want" = "$got" ] || die "checksum mismatch for $asset (expected $want, got $got)"

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/ripple" ] || die "archive did not contain a ripple binary"

mkdir -p "$dir"
install -m 0755 "$tmp/ripple" "$dir/ripple"
say "installed $("$dir/ripple" --version 2>/dev/null || echo ripple) to $dir/ripple"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "$dir is not on your PATH. Add this to your shell profile: export PATH=\"$dir:\$PATH\"" ;;
esac

say "next: cd into a Rails or React repo and run: ripple impact"
if [ -z "${RIPPLE_NO_SKILL:-}" ]; then
  say "for coding agents, run in the repo: ripple skill install"
fi
