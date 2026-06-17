#!/usr/bin/env bash
#
# Generate the tray PNG icons from their editable SVG sources.
#
# For every resources/icons/*.svg it renders a same-named .png at SIZE
# pixels (square). Edit the SVGs, then run this to refresh the PNGs that
# src/tray_icons.rs embeds.
#
# Usage:
#   scripts/generate_icons.sh           # 128x128 (matches the embedded assets)
#   SIZE=256 scripts/generate_icons.sh  # override output size
#
# Renderer: uses whichever is installed (in order):
#   rsvg-convert  (brew install librsvg)
#   inkscape      (brew install --cask inkscape)
#   cairosvg      (pip install cairosvg)

set -euo pipefail

SIZE="${SIZE:-128}"
ICON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../resources/icons" && pwd)"

render() {
  local svg="$1" png="$2"
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w "$SIZE" -h "$SIZE" "$svg" -o "$png"
  elif command -v inkscape >/dev/null 2>&1; then
    inkscape "$svg" --export-type=png --export-filename="$png" \
      -w "$SIZE" -h "$SIZE" >/dev/null 2>&1
  elif command -v cairosvg >/dev/null 2>&1; then
    cairosvg "$svg" -W "$SIZE" -H "$SIZE" -o "$png"
  else
    echo "error: no SVG renderer found." >&2
    echo "install one of: rsvg-convert (brew install librsvg)," >&2
    echo "                inkscape, or cairosvg (pip install cairosvg)" >&2
    exit 1
  fi
}

shopt -s nullglob
svgs=("$ICON_DIR"/*.svg)
if [ ${#svgs[@]} -eq 0 ]; then
  echo "no SVGs found in $ICON_DIR" >&2
  exit 1
fi

for svg in "${svgs[@]}"; do
  png="${svg%.svg}.png"
  render "$svg" "$png"
  echo "rendered ${png#"$ICON_DIR"/} (${SIZE}x${SIZE})"
done
