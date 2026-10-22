#!/usr/bin/env bash
# Prove that a libghostty-vt build runs on any CPU of its architecture.
#
# A library built for the build machine's CPU ("-Dcpu=native") has newer instructions in
# ordinary code. On an older CPU that is SIGILL. A baseline build has such instructions only in
# functions that choose their CPU at run time (simdutf's `haswell` implementation, ghostty's
# `N_AVX2` / `N_AVX3` multiversioned code), and those names are the allowlist.
#
# x86-64 counts, outside the allowlist: VEX/EVEX instructions (AVX, AVX2, AVX-512, FMA, any `v*`
#   mnemonic except the VMX ones, any ymm/zmm/k-mask operand), BMI1/BMI2, lzcnt/popcnt,
#   movbe, SSSE3, SSE4.1/4.2, AES-NI, PCLMUL, SHA, adx. `tzcnt` is only reported: it is the same
#   bytes as `rep bsf`, which baseline code uses.
# aarch64 counts, outside the allowlist: SVE (z registers, p predicates), LSE atomics, crypto
#   (aes, sha, pmull), crc32, dot product, RDM, bf16, i8mm, fcma, jscvt, half-precision float,
#   LRCPC and MTE.
#
# Usage: check-no-native-simd.sh LIB.so [--expect-native]
#        check-no-native-simd.sh --self-test        (x86-64 only: assembles sample code)
# Exit 1 if ordinary code holds such instructions (or, with --expect-native, if it holds none:
# the check must be able to see a native build). Exit 2 if a needed tool is missing: a check
# that cannot run is a failure, never a pass.
# OBJDUMP=<path> overrides the disassembler.
set -euo pipefail

# Names of functions that pick their CPU at run time. `libunwind::` is Zig's bundled unwinder:
# on aarch64 it executes `stg` (MTE) only for frames whose CIE marks them as MTE tagged.
DISPATCH='haswell|icelake|westmere|skylake|N_AVX|N_SSE|N_SSSE3|N_SVE|N_NEON|avx2|avx512|AVX|libunwind::'

# ---- x86-64 --------------------------------------------------------------------------------
X86_AWK='
BEGIN {
  n = split("vmcall vmlaunch vmresume vmread vmwrite vmxon vmxoff vmptrld vmptrst vmclear vmfunc verr verw", ex, " ")
  for (i = 1; i <= n; i++) notvex[ex[i]] = 1
  n = split("lock rep repz repnz repe repne bnd notrack data16 cs ds es ss fs gs addr32 xacquire xrelease", pf, " ")
  for (i = 1; i <= n; i++) prefix[pf[i]] = 1
  ext = "^(shlx|sarx|shrx|rorx|andn|pdep|pext|bzhi|blsi|blsr|blsmsk|bextr|mulx|lzcnt|popcnt|movbe|adcx|adox"
  ext = ext "|ptest|pcmpistri|pcmpestri|pcmpistrm|pcmpestrm|pblendvb|pblendw|pminsd|pminud|pminsb|pminuw|pmaxsd|pmaxud|pmaxsb|pmaxuw"
  ext = ext "|roundps|roundpd|roundss|roundsd|blendvps|blendvpd|blendps|blendpd|dpps|dppd|pmulld|pmuldq"
  ext = ext "|pmovsxbw|pmovsxbd|pmovsxbq|pmovsxwd|pmovsxwq|pmovsxdq|pmovzxbw|pmovzxbd|pmovzxbq|pmovzxwd|pmovzxwq|pmovzxdq"
  ext = ext "|pextrb|pextrd|pextrq|pinsrb|pinsrd|pinsrq|insertps|extractps|pcmpeqq|pcmpgtq|packusdw|phminposuw|mpsadbw|movntdqa"
  ext = ext "|pshufb|pabsb|pabsw|pabsd|palignr|phaddw|phaddd|phaddsw|phsubw|phsubd|phsubsw|pmaddubsw|pmulhrsw|psignb|psignw|psignd"
  ext = ext "|aesenc|aesenclast|aesdec|aesdeclast|aesimc|aeskeygenassist|pclmulqdq|sha1rnds4|sha1nexte|sha1msg1|sha1msg2"
  ext = ext "|sha256rnds2|sha256msg1|sha256msg2|rdrand|rdseed|clflushopt|clwb|prefetchw|xsavec|xsaves|cmpxchg16b)$"
}
/^[0-9a-f]+ <.*>:$/ { fn = $0; allowed = (fn ~ DISPATCH); next }
{
  line = $0
  sub(/^[ \t]*[0-9a-f]+:[ \t]*/, "", line)
  if (line == "" || line ~ /^\(bad\)/ || line ~ /^\./) next
  k = split(line, p, /[ \t]+/)
  i = 1
  while (i < k && (p[i] in prefix)) i++
  m = p[i]
  kind = ""
  # tzcnt is byte-for-byte `rep bsf`, which baseline code generation emits on purpose (old CPUs
  # run it as bsf). The disassembler cannot tell them apart, so it is counted but not a failure.
  if (m == "tzcnt") { if (!(fn ~ DISPATCH)) tzcnt++; next }
  if ((m ~ /^v/ && !(m in notvex)) || line ~ /%[yz]mm/ || line ~ /%k[0-7]/) kind = "vex"
  else if (m ~ ext || m ~ /^crc32/) kind = "ext"
  if (kind == "") next
  total++
  if (allowed) next
  ordinary++
  if (kind == "vex") vex++; else ext_n++
  if (line ~ /%zmm/) zmm++
  hitfn[fn] = 1
  if (shown < 6 && !(fn in seen)) { seen[fn] = 1; shown++; example[shown] = m " in " fn }
}
END {
  if (REPORT) { for (f in hitfn) print "HIT " f; exit }
  printf "%d %d %d %d %d %d\n", total, ordinary, vex, ext_n, zmm, tzcnt
  for (i = 1; i <= shown; i++) print "  e.g. " example[i] > "/dev/stderr"
}'

# ---- aarch64 -------------------------------------------------------------------------------
A64_AWK='
/^[0-9a-f]+ <.*>:$/ { fn = $0; allowed = (fn ~ DISPATCH); next }
{
  line = $0
  sub(/^[ \t]*[0-9a-f]+:[ \t]*/, "", line)
  if (line == "" || line ~ /^\./ || line ~ /^\(bad\)/) next
  split(line, p, /[ \t]+/)
  m = p[1]
  ops = substr(line, length(m) + 1)
  kind = ""
  if (ops ~ /(^|[ ,\[{])z[0-9]+(\.[bhsdq]|[ ,\]}\/]|$)/ || ops ~ /(^|[ ,\[{])p[0-9]+(\/[zm]|\.[bhsdq])/) kind = "sve"
  else if (m ~ /^(ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|swp)(a|l|al)?(b|h)?$/ || m ~ /^(stadd|stclr|steor|stset|stsmax|stsmin|stumax|stumin)l?(b|h)?$/ || m ~ /^casp?(a|l|al)?(b|h)?$/) kind = "lse"
  else if (m ~ /^(aese|aesd|aesmc|aesimc|sha1c|sha1p|sha1m|sha1h|sha1su0|sha1su1|sha256h|sha256h2|sha256su0|sha256su1|sha512h|sha512h2|sha512su0|sha512su1|sm3[a-z0-9]*|sm4e|sm4ekey|eor3|rax1|xar|bcax|pmull|pmull2)$/) kind = "crypto"
  else if (m ~ /^crc32/) kind = "crc"
  else if (m ~ /^(sdot|udot|usdot|sudot|sqrdmlah|sqrdmlsh|bfdot|bfmmla|bfmlalb|bfmlalt|bfcvt|bfcvtn|bfcvtn2|smmla|ummla|usmmla|fcmla|fcadd|fjcvtzs|ldapr|ldaprb|ldaprh|ldapur|ldlar|stllr|ldg|stg|st2g|stzg|stz2g|irg|gmi|addg|subg)$/) kind = "ext"
  else if (m ~ /^f/ && m != "fcvt" && m !~ /^fcvt[nl]2?$/ && (ops ~ /(^|[ ,\[])h[0-9]+/ || ops ~ /\.(4|8)h/)) kind = "fp16"
  if (kind == "") next
  total++
  if (allowed) next
  ordinary++
  if (kind == "sve") sve++
  if (shown < 6 && !(fn in seen)) { seen[fn] = 1; shown++; example[shown] = m " (" kind ") in " fn }
}
END {
  printf "%d %d %d 0 0 0\n", total, ordinary, sve
  for (i = 1; i <= shown; i++) print "  e.g. " example[i] > "/dev/stderr"
}'

fail_tool() { echo "check-no-native-simd: $1" >&2; exit 2; }

disassembler() {
  local arch="$1"
  if [ -n "${OBJDUMP:-}" ]; then echo "$OBJDUMP"; return; fi
  local c
  if [ "$arch" = aarch64 ]; then
    for c in aarch64-linux-gnu-objdump llvm-objdump objdump; do
      command -v "$c" >/dev/null && "$c" -i 2>/dev/null | grep -qi aarch64 && { echo "$c"; return; }
      # llvm-objdump has no -i: accept it by name.
      [ "$c" = llvm-objdump ] && command -v "$c" >/dev/null && { echo "$c"; return; }
    done
    fail_tool "no aarch64 disassembler (install binutils-aarch64-linux-gnu or llvm), cannot check"
  fi
  for c in objdump llvm-objdump; do command -v "$c" >/dev/null && { echo "$c"; return; }; done
  fail_tool "objdump not found, cannot check"
}

run_check() {
  local so="$1" expect_native="${2:-}"
  command -v readelf >/dev/null || fail_tool "readelf not found, cannot check"
  [ -f "$so" ] || fail_tool "no such file: $so"
  local machine arch
  machine="$(readelf -h "$so" | awk -F: '/Machine/ {print $2}')"
  case "$machine" in
    *X86-64*) arch=x86_64 ;;
    *AArch64*) arch=aarch64 ;;
    *) fail_tool "unsupported ELF machine: $machine" ;;
  esac
  local od prog
  od="$(disassembler "$arch")"
  if [ "$arch" = x86_64 ]; then prog="$X86_AWK"; else prog="$A64_AWK"; fi

  local out
  out="$("$od" -d --no-show-raw-insn -C "$so" | awk -v DISPATCH="$DISPATCH" -v REPORT=0 "$prog")" || fail_tool "disassembly failed"
  local total ordinary vex ext zmm tz
  read -r total ordinary vex ext zmm tz <<<"$out"
  tz="${tz:-0}"
  [ -n "${total:-}" ] || fail_tool "no output from the disassembler for $so"
  # (the "e.g." lines went to stderr already)

  echo "libghostty-vt ($arch): $so"
  echo "  newer-than-baseline instructions, all functions:        $total"
  echo "  newer-than-baseline instructions, outside run-time dispatch: $ordinary  (vex/sve $vex, other $ext, avx512 $zmm)"
  [ "$arch" = x86_64 ] && echo "  tzcnt outside run-time dispatch (same bytes as rep bsf, not counted): $tz"

  if [ "$expect_native" = "--expect-native" ]; then
    [ "$ordinary" -gt 0 ] || { echo "FAIL: expected a native build, found none"; return 1; }
    echo "OK: native build detected (as expected)"
  else
    [ "$ordinary" -eq 0 ] || { echo "FAIL: ordinary code needs a newer CPU than the baseline: this library can raise SIGILL"; return 1; }
    echo "OK: baseline"
  fi
}

# ---- self test (x86-64): sample instructions through the same awk -----------------------------
self_test() {
  command -v as >/dev/null || fail_tool "as not found, cannot self-test"
  command -v ld >/dev/null || fail_tool "ld not found, cannot self-test"
  local dir; dir="$(mktemp -d)"; trap 'rm -rf "$dir"' RETURN
  # One function per instruction, so a miss names the instruction.
  local bad=0 name insn
  {
    echo '.text'
    while IFS='|' read -r name insn; do
      [ -n "$name" ] || continue
      printf '.globl f_%s\n.type f_%s,@function\nf_%s:\n  %s\n  ret\n' "$name" "$name" "$name" "$insn"
    done <<'LIST'
vmovdqu|vmovdqu (%rdi),%ymm0
vmulps|vmulps %xmm1,%xmm2,%xmm3
vpaddd|vpaddd %zmm1,%zmm2,%zmm3
vzeroupper|vzeroupper
vfmadd|vfmadd231ps %xmm1,%xmm2,%xmm3
kmovw|kmovw %k1,%eax
shlx|shlx %rax,%rbx,%rcx
sarx|sarx %rax,%rbx,%rcx
shrx|shrx %rax,%rbx,%rcx
rorx|rorx $3,%rax,%rbx
andn|andn %rax,%rbx,%rcx
pdep|pdep %rax,%rbx,%rcx
pext|pext %rax,%rbx,%rcx
bzhi|bzhi %rax,%rbx,%rcx
lzcnt|lzcnt %rax,%rbx
popcnt|popcnt %rax,%rbx
movbe|movbe (%rdi),%rax
ptest|ptest %xmm1,%xmm2
pcmpistri|pcmpistri $0,%xmm1,%xmm2
pcmpestri|pcmpestri $0,%xmm1,%xmm2
pblendvb|pblendvb %xmm0,%xmm1,%xmm2
pminsd|pminsd %xmm1,%xmm2
pmaxsd|pmaxsd %xmm1,%xmm2
roundps|roundps $1,%xmm1,%xmm2
pshufb|pshufb %xmm1,%xmm2
aesenc|aesenc %xmm1,%xmm2
LIST
    # Baseline code (SSE2 and plain x86-64): must not be flagged. `vm`-prefixed VMX is not AVX.
    printf '.globl f_base\n.type f_base,@function\nf_base:\n  movdqu (%%rdi),%%xmm0\n  paddd %%xmm1,%%xmm0\n  pshufd $0,%%xmm0,%%xmm1\n  pextrw $1,%%xmm0,%%eax\n  bsr %%rax,%%rbx\n  lock cmpxchg %%rcx,(%%rdi)\n  rep movsb\n  ret\n'
    # The same AVX instruction inside a run-time dispatched function is allowed.
    printf '.globl _ZN4simdutf7haswell4impl3fooEv\n.type _ZN4simdutf7haswell4impl3fooEv,@function\n_ZN4simdutf7haswell4impl3fooEv:\n  vmovdqu (%%rdi),%%ymm0\n  ret\n'
  } >"$dir/t.s"
  as "$dir/t.s" -o "$dir/t.o"
  ld -shared "$dir/t.o" -o "$dir/t.so" 2>/dev/null || ld -shared -z notext "$dir/t.o" -o "$dir/t.so"
  local od hits; od="$(disassembler x86_64)"
  hits="$("$od" -d --no-show-raw-insn -C "$dir/t.so" | awk -v DISPATCH="$DISPATCH" -v REPORT=1 "$X86_AWK" 2>/dev/null)"
  for name in vmovdqu vmulps vpaddd vzeroupper vfmadd kmovw shlx sarx shrx rorx andn pdep pext bzhi lzcnt popcnt movbe ptest pcmpistri pcmpestri pblendvb pminsd pmaxsd roundps pshufb aesenc; do
    if ! grep -q "<f_${name}>" <<<"$hits"; then echo "self-test: NOT flagged: $name"; bad=1; fi
  done
  grep -q "<f_base>" <<<"$hits" && { echo "self-test: baseline code was flagged"; bad=1; }
  grep -q "haswell" <<<"$hits" && { echo "self-test: dispatched function was flagged"; bad=1; }
  [ "$bad" -eq 0 ] && echo "self-test OK: all sample instructions flagged, baseline and dispatched code not flagged"
  return "$bad"
}

case "${1:-}" in
  --self-test) self_test ;;
  "") echo "usage: $0 LIB.so [--expect-native] | --self-test" >&2; exit 2 ;;
  *) run_check "$1" "${2:-}" ;;
esac
