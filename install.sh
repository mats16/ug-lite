#!/bin/sh
# Install ug-lite from the latest GitHub release.
#   curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | UG_LITE_INSTALL_DIR=/usr/local/bin sh
set -eu

# Everything runs from main, called on the last line, so a truncated download executes nothing.
main() {
  dir=${UG_LITE_INSTALL_DIR:-$HOME/.local/bin}

  platform="$(uname -s)-$(uname -m)"
  # A Rosetta shell on Apple Silicon reports x86_64; the arm64 binary still runs there.
  if [ "$platform" = Darwin-x86_64 ] && [ "$(/usr/sbin/sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    platform=Darwin-arm64
  fi
  case "$platform" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Linux-x86_64) target=x86_64-unknown-linux-musl ;;
    Linux-aarch64) target=aarch64-unknown-linux-musl ;;
    *) echo "ug-lite: unsupported platform: $platform" >&2; exit 1 ;;
  esac

  name=ug-lite-$target.tar.gz
  url=https://github.com/mats16/ug-lite/releases/latest/download/$name
  tmp=
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 1' INT TERM HUP # dash skips the EXIT trap on signals unless the handler exits
  tmp=$(mktemp -d)

  curl -fsSL -o "$tmp/$name" "$url"
  curl -fsSL -o "$tmp/$name.sha256" "$url.sha256"
  if command -v sha256sum >/dev/null 2>&1; then sum='sha256sum'; else sum='shasum -a 256'; fi
  (cd "$tmp" && $sum -c "$name.sha256" >/dev/null) || { echo "ug-lite: checksum mismatch for $name" >&2; exit 1; }
  tar -xzf "$tmp/$name" -C "$tmp"

  # Use sudo only when the target directory is not writable (e.g. /usr/local/bin).
  sudo=
  if ! { mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; }; then sudo=sudo; fi
  $sudo mkdir -p "$dir"
  $sudo install -m 755 "$tmp/ug-lite" "$dir/ug-lite"
  bin="$(cd "$dir" && pwd)/ug-lite" # absolute, so the printed link works from anywhere
  echo "Installed ug-lite to $bin"
  echo "To use it as ug, run this and put ~/.ug-lite/bin ahead of the original ug on PATH:"
  echo "  mkdir -p ~/.ug-lite/bin && ln -sf '$bin' ~/.ug-lite/bin/ug"
}

main
