#!/usr/bin/env sh
set -eu

# Installs a locally produced Piwaku Linux archive into ~/.local without root.
# Public Piwaku releases currently target macOS; use WAKU_BUNDLE_PATH for a
# locally built Linux archive.

usage() {
    cat <<'USAGE'
Install a local Piwaku Linux archive into ~/.local.

Usage:
  WAKU_BUNDLE_PATH=path/to/waku-<version>-<target>.tar.gz sh website/public/install.sh
  sh website/public/install.sh --uninstall

Options:
  --uninstall   Remove Piwaku, leaving ~/.waku (projects and settings) alone
  --help        Show this help
USAGE
}

main() {
    app_dir="$HOME/.local/waku.app"
    bin_link="$HOME/.local/bin/waku"
    desktop_file="$HOME/.local/share/applications/sh.waku.desktop"

    case "${1:-}" in
        --uninstall) uninstall; return ;;
        --help | -h) usage; return ;;
        "") ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac

    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        echo "Piwaku for macOS ships as a signed .dmg." >&2
        echo "Use the GitHub Releases page to download it." >&2
        exit 1
    fi
    if [ "$platform" != "Linux" ]; then
        echo "Unsupported platform: $platform" >&2
        exit 1
    fi

    if [ -z "${WAKU_BUNDLE_PATH:-}" ]; then
        echo "Public Piwaku releases currently target macOS; set WAKU_BUNDLE_PATH to a local Linux archive." >&2
        exit 1
    fi

    temp="$(mktemp -d "${TMPDIR:-/tmp}/piwaku-XXXXXX")"
    staging="$app_dir.new"
    trap 'rm -rf -- "$temp" "$staging"' EXIT INT TERM

    archive="$temp/waku.tar.gz"
    cp "$WAKU_BUNDLE_PATH" "$archive"
    if ! tar -tzf "$archive" >/dev/null 2>&1; then
        echo "The local file is not a readable tarball." >&2
        exit 1
    fi

    # Unpack beside the target and swap only once the contents check out. The
    # tarball holds one versioned top-level directory; stripping it keeps every
    # install at the same path.
    echo "Installing to $app_dir"
    rm -rf "$staging"
    mkdir -p "$staging" "$(dirname "$bin_link")" "$(dirname "$desktop_file")"
    tar -xzf "$archive" --strip-components=1 -C "$staging"

    # Piwaku resolves its daemon next to its executable, so both must stay
    # together in bin/. Linking only the main binary onto PATH is safe because
    # current_exe() resolves the symlink back into waku.app.
    for binary in waku waku-daemon; do
        if [ ! -x "$staging/bin/$binary" ]; then
            echo "Archive is missing bin/$binary." >&2
            exit 1
        fi
    done
    # Replace rather than merge: a file dropped from a later layout must not
    # survive the replacement.
    rm -rf "$app_dir"
    mv "$staging" "$app_dir"
    ln -sf "$app_dir/bin/waku" "$bin_link"

    entry="$app_dir/share/applications/sh.waku.desktop"
    if [ -f "$entry" ]; then
        # The packaged entry is relocatable (bare Exec/Icon names). Pin both to
        # this install so the launcher works without PATH or icon-theme setup.
        sed -e "s|^Exec=waku$|Exec=$app_dir/bin/waku|" \
            -e "s|^Icon=sh.waku$|Icon=$app_dir/share/icons/hicolor/256x256/apps/sh.waku.png|" \
            "$entry" >"$desktop_file"
        if command -v update-desktop-database >/dev/null 2>&1; then
            update-desktop-database "$(dirname "$desktop_file")" 2>/dev/null || true
        fi
    fi

    echo "Piwaku is installed."
    if [ -f "$desktop_file" ]; then
        echo "Open it from your applications menu."
    fi
    if [ "$(command -v waku || true)" = "$bin_link" ]; then
        echo "From a terminal: waku"
    else
        echo "From a terminal: $bin_link"
    fi
}

uninstall() {
    if [ ! -d "$app_dir" ] && [ ! -L "$bin_link" ]; then
        echo "Piwaku is not installed at $app_dir." >&2
        exit 1
    fi
    # Only reclaim the symlink and desktop entry this script created; a distro
    # package's copies of both belong to the package manager.
    if [ "$(readlink "$bin_link" 2>/dev/null || true)" = "$app_dir/bin/waku" ]; then
        rm -f "$bin_link"
    fi
    if [ -f "$desktop_file" ] && grep -qF "$app_dir/bin/waku" "$desktop_file"; then
        rm -f "$desktop_file"
    fi
    rm -rf "$app_dir"
    echo "Piwaku is uninstalled. Projects and settings remain in ~/.waku."
}

main "$@"
