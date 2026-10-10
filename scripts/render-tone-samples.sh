#!/usr/bin/env bash
# Render the same passages with every guitar-tone method for A/B listening.
# Usage: scripts/render-tone-samples.sh <song.gp5> [out-dir]   (default out-dir: tone-samples/)
set -euo pipefail
cd "$(dirname "$0")/.."
song=${1:?usage: $0 <song.gp5> [out-dir]}
out=${2:-tone-samples}
cargo build --release --quiet
bin=target/release/guitar-practice-tool
mkdir -p "$out"

# name | extra render flags
methods=(
  "A_sampled-distortion|--no-nam --tone distortion"
  "B_freepats-di_builtin-amp-sim|--no-nam --tone distortion(ampsim)"
  "C_freepats-di_nam|--di freepats --single"
  "D_humbucker-di_nam_single|--single"
  "E_humbucker-di_nam_double-tracked|"
)
# name | render flags
clips=(
  "1_rhythm-solo|--solo 2 --start-measure 40 --seconds 15"
  "2_lead-solo|--solo 0 --start-measure 100 --seconds 12"
  "3_full-mix|--start-measure 40 --seconds 20"
)

for m in "${methods[@]}"; do
  mname=${m%%|*}; mflags=${m#*|}
  for c in "${clips[@]}"; do
    cname=${c%%|*}; cflags=${c#*|}
    f="$out/${cname}__${mname}.wav"
    echo "== $f"
    # shellcheck disable=SC2086
    "$bin" --render "$song" "$f" $cflags $mflags | tail -1
  done
done
# Practice-speed example with the default (E) tone.
"$bin" --render "$song" "$out/4_rhythm-solo_70pct-speed__E_humbucker-di_nam_double-tracked.wav" \
  --solo 2 --start-measure 40 --seconds 20 --speed 0.7 | tail -1
