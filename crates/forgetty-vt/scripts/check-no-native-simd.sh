#!/usr/bin/env bash
# Prove that a libghostty-vt build runs on any x86-64 CPU.
#
# A library built for the build machine's CPU ("-Dcpu=native") has AVX, AVX2 or AVX-512
# instructions in ordinary code. On another CPU that is SIGILL. A baseline build has such
# instructions only in functions that pick their CPU at run time (simdutf's `haswell`
# implementation and ghostty's `N_AVX2` / `N_AVX3` multiversioned code).
#
# Usage: check-no-native-simd.sh path/to/libghostty-vt.so [--expect-native]
# Prints the counts. Exit 1 if ordinary code holds VEX/EVEX instructions (or, with
# --expect-native, if it holds none: the check must be able to see a native build).
set -euo pipefail
so="${1:?usage: check-no-native-simd.sh libghostty-vt.so [--expect-native]}"
expect_native="${2:-}"

# Functions that choose their CPU at run time. Their names say which instruction set they use.
dispatch='haswell|icelake|westmere|skylake|N_AVX|N_SSE|avx2|avx512|AVX'

read -r total ordinary zmm_ordinary < <(
  objdump -d --no-show-raw-insn -C "$so" | awk -v dispatch="$dispatch" '
    /^[0-9a-f]+ <.*>:$/ { fn = $0; is_dispatch = (fn ~ dispatch); next }
    {
      # "  addr:\tmnemonic operands"
      line = $0
      sub(/^[ \t]*[0-9a-f]+:[ \t]*/, "", line)
      split(line, p, /[ \t]+/)
      m = p[1]
      vex = (line ~ /[yz]mm/) || (m ~ /^v/ && m !~ /^(verr|verw|vm|vzeroall)/ && m != "")
      if (!vex) next
      total++
      if (!is_dispatch) {
        ordinary++
        if (line ~ /zmm/) zmm_ordinary++
      }
    }
    END { printf "%d %d %d\n", total, ordinary, zmm_ordinary }')

echo "libghostty-vt: $so"
echo "  VEX/EVEX instructions, all functions:              ${total:-0}"
echo "  VEX/EVEX instructions, outside run-time dispatch:  ${ordinary:-0}"
echo "  AVX-512 (zmm) outside run-time dispatch:           ${zmm_ordinary:-0}"

if [ "$expect_native" = "--expect-native" ]; then
  [ "${ordinary:-0}" -gt 0 ] || { echo "FAIL: expected a native build, found none"; exit 1; }
  echo "OK: native build detected (as expected)"
else
  [ "${ordinary:-0}" -eq 0 ] || { echo "FAIL: ordinary code needs AVX or newer: this library can raise SIGILL on other CPUs"; exit 1; }
  echo "OK: baseline"
fi
