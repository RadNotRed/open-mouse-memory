#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$project_root"

case "$(uname -m)" in
    x86_64)
        appimage_arch="x86_64"
        ;;
    *)
        echo "unsupported AppImage architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

appimagetool_revision="8c8c91f762b412a19f4e8d2c4b35afb98f2d7c81"
appimagetool_sha256="a6d71e2b6cd66f8e8d16c37ad164658985e0cf5fcaa950c90a482890cb9d13e0"
appimagetool_url="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${appimage_arch}.AppImage"
tool_cache="${XDG_CACHE_HOME:-${HOME}/.cache}/open-mouse-memory-tools"
appimagetool="${tool_cache}/appimagetool-${appimagetool_revision}-${appimage_arch}.AppImage"
dist_dir="${DIST_DIR:-${project_root}/dist}"
version="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)}"
output_name="Open-Mouse-Memory-${version}-${appimage_arch}.AppImage"
output="${dist_dir}/${output_name}"
staging="$(mktemp -d "${TMPDIR:-/tmp}/open-mouse-memory-appimage.XXXXXX")"
appdir="${staging}/OpenMouseMemory.AppDir"

cleanup() {
    rm -rf "$staging"
}
trap cleanup EXIT

mkdir -p "$tool_cache" "$dist_dir"
if [[ ! -f "$appimagetool" ]] || ! echo "${appimagetool_sha256}  ${appimagetool}" | sha256sum --check --status; then
    temporary_tool="${appimagetool}.download"
    curl --fail --location --retry 3 --output "$temporary_tool" "$appimagetool_url"
    echo "${appimagetool_sha256}  ${temporary_tool}" | sha256sum --check
    chmod +x "$temporary_tool"
    mv "$temporary_tool" "$appimagetool"
fi

cargo build --release --all-features --locked
install -Dm755 target/release/open-mouse-memory-gui "${appdir}/usr/bin/open-mouse-memory-gui"
install -Dm644 packaging/open-mouse-memory.desktop "${appdir}/usr/share/applications/open-mouse-memory.desktop"
install -Dm644 packaging/icons/hicolor/scalable/apps/open-mouse-memory.svg \
    "${appdir}/usr/share/icons/hicolor/scalable/apps/open-mouse-memory.svg"
install -Dm644 packaging/icons/hicolor/symbolic/apps/open-mouse-memory-symbolic.svg \
    "${appdir}/usr/share/icons/hicolor/symbolic/apps/open-mouse-memory-symbolic.svg"
ln -s usr/bin/open-mouse-memory-gui "${appdir}/AppRun"
ln -s usr/share/applications/open-mouse-memory.desktop "${appdir}/open-mouse-memory.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/open-mouse-memory.svg "${appdir}/open-mouse-memory.svg"
ln -s open-mouse-memory.svg "${appdir}/.DirIcon"
rm -f "$output" "${output}.sha256"

ARCH="$appimage_arch" "$appimagetool" --appimage-extract-and-run "$appdir" "$output"

test -s "$output"
chmod +x "$output"
(
    cd "$dist_dir"
    sha256sum "$output_name" > "${output_name}.sha256"
)
printf '%s\n' "$output"
