#!/usr/bin/env bash
# Download the free FreePats sample banks used for realistic guitar, bass and drum sounds.
# Usage: scripts/fetch-sounds.sh [target-dir]   (default: ./assets)
set -euo pipefail

dest="${1:-$(dirname "$0")/../assets}"
mkdir -p "$dest"
cd "$dest"

banks=(
  "https://github.com/freepats/electric-guitar-FSBS-dist2/releases/download/2022-09-11/EGuitarFSBS-dist2-SF2-20220911.7z"
  "https://github.com/freepats/electric-guitar-FSBS-dist1/releases/download/2022-09-11/EGuitarFSBS-dist1-SF2-20220911.7z"
  "https://github.com/freepats/electric-guitar-FSBS-clean/releases/download/2026-08-07/EGuitarFSBS-clean-SF2-20260807.7z"
  "https://github.com/freepats/electric-guitar-FSBS-direct/releases/download/2022-09-11/EGuitarFSBS-direct-SF2-20220911.7z"
  "https://github.com/freepats/electric-bass-YR/releases/download/2019-09-30/FingerBassYR-SF2-20190930.7z"
  "https://github.com/freepats/electric-bass-YR/releases/download/2019-09-30/PickedBassYR-SF2-20190930.7z"
  "https://github.com/freepats/muldjordkit/releases/download/2020-10-18/MuldjordKit-SF2-20201018.7z"
)

extract() {
  if command -v 7z >/dev/null; then
    7z x -y "$1" >/dev/null
  elif command -v bsdtar >/dev/null; then
    bsdtar -xf "$1"
  else
    echo "Need 7z (p7zip) or bsdtar (libarchive) to extract $1" >&2
    exit 1
  fi
}

for url in "${banks[@]}"; do
  file="${url##*/}"
  name="${file%.7z}"
  # Archives extract to "<Name> SF2-<date>/" or "<Name>-SF2-<date>/".
  if [ -d "$name" ] || [ -d "${name/-SF2-/ SF2-}" ]; then
    echo "✓ $name already present"
    continue
  fi
  echo "↓ $file"
  curl -fL --progress-bar -o "$file" "$url"
  extract "$file"
  rm -f "$file"
done

# Dry humbucker DI recordings with real palm-muted takes (lotkey/free-sample-libraries-sfz),
# used as the input for the Neural Amp Modeler tone.
hb_base="https://raw.githubusercontent.com/lotkey/free-sample-libraries-sfz/main/guitar/humbucker"
mkdir -p humbucker-di
n_new=0
for note in "G#1" "D#2" "G#2" "C#3" "F3" "A#3"; do
  for rr in 1 2 3 4; do
    for vel in 1 2; do
      for mute in "" "_muted"; do
        f="${note}_${rr}_${vel}${mute}.wav"
        [ -s "humbucker-di/$f" ] && continue
        curl -fsSL -o "humbucker-di/$f" "$hb_base/${f//#/%23}"
        n_new=$((n_new + 1))
      done
    done
  done
done
echo "✓ humbucker DI samples ($n_new downloaded)"

echo "Sound banks are in $(pwd)"
