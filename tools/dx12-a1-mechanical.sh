#!/usr/bin/env bash
# A1 mechanical sweep for a D3D12 changeset (PARALLEL.md §10 A1, METHOD.md §3 criterion 3).
#
# Every check here is an EXIT CODE, not a human's reading -- METHOD.md saturation criterion 3
# exists because "a grep check has twice counted its own documentation here".
#
# Usage: tools/dx12-a1-mechanical.sh [BASE_REF]   (default 3e750c0)
#
# Exit 0 only if every check passes. Findings print as "FAIL <check>: <detail>".
set -uo pipefail
cd "$(dirname "$0")/.."

BASE="${1:-3e750c0}"
rc=0
fail() { echo "FAIL $1: $2"; rc=1; }
pass() { echo "ok   $1${2:+: $2}"; }

# The changeset's Rust + C++ sources (submodules and docs excluded -- they are reviewed by lens,
# not by grep, and a submodule path is a gitlink with no lines).
mapfile -t SRC < <(git diff --name-only "$BASE" -- '*.rs' '*.cpp' '*.h' | grep -v '^docs/')
if [ ${#SRC[@]} -eq 0 ]; then echo "FAIL setup: no source files in $BASE..HEAD"; exit 1; fi
echo "== A1 over ${#SRC[@]} changed source files, base $BASE =="

# ---------------------------------------------------------------------------
# 1. Every `unsafe` block/fn in a CHANGED HUNK carries a `// SAFETY:` (CLAUDE.md rule 4).
#    Scoped to added lines only: pre-existing unsafe is not this changeset's debt, and sweeping
#    whole files would drown the real signal.
# ---------------------------------------------------------------------------
n_unsafe=0; n_missing=0; missing_list=""
for f in "${SRC[@]}"; do
  [[ "$f" == *.rs ]] || continue
  [ -f "$f" ] || continue
  # Added lines introducing an unsafe block or unsafe fn.
  while IFS= read -r ln; do
    n_unsafe=$((n_unsafe+1))
    # Three forms are legal and all three are in use: the `// SAFETY:` CLAUDE.md rule 4 names,
    # the QUALIFIED form `// SAFETY (both arms):`, and rustdoc's `# Safety` section, which is how
    # an `unsafe fn`'s contract is written here.
    # ⛔ Tightening past this is how a lint manufactures work: demanding the colon immediately
    # after SAFETY rejected `// SAFETY (both arms):`, and knowing only the first form reported 31
    # false positives. The rule is "the block states its invariant under a SAFETY heading" -- so
    # that, and not one spelling of it, is what is matched.
    # Window is 18 lines: a `# Safety` section sits above the doc prose that follows it.
    lo=$((ln>18 ? ln-18 : 1))
    if ! sed -n "${lo},${ln}p" "$f" | grep -qE 'SAFETY|#[[:space:]]*Safety'; then
      n_missing=$((n_missing+1)); missing_list+=$'\n'"    $f:$ln"
    fi
  done < <(git diff -U0 "$BASE" -- "$f" \
            | awk '/^(\+\+\+|---|diff |index |new file|deleted file|similarity|rename |old mode|new mode)/ { next }
                   /^@@/{ if (match($0,/\+[0-9]+/)) { ln=substr($0,RSTART+1,RLENGTH-1)+0 } ; next }
                   /^\+/ { if ($0 ~ /unsafe[[:space:]]*[{]/ || $0 ~ /unsafe[[:space:]]+(fn|extern|impl)/) print ln; ln++ ; next }
                   /^-/  { next }
                   { ln++ }')
done
if [ $n_missing -gt 0 ]; then fail "unsafe/SAFETY" "$n_missing of $n_unsafe added unsafe sites lack SAFETY:$missing_list"
else pass "unsafe/SAFETY" "$n_unsafe added unsafe sites, all documented"; fi

# ---------------------------------------------------------------------------
# 2. No panic path on runtime data in added lines. A panic in any DDI is a silent graphics
#    deadlock (panic=abort in the KMD). Test modules and const-eval asserts are exempt and the
#    exemption is applied by FILE ROLE, not by guessing.
# ---------------------------------------------------------------------------
panic_hits=""
for f in "${SRC[@]}"; do
  [[ "$f" == *.rs ]] || continue
  [ -f "$f" ] || continue
  [[ "$f" == kmd_logic/* ]] && continue   # libtest harness crate: unwrap in tests is correct
  while IFS= read -r line; do
    ln="${line%%:*}"; txt="${line#*:}"
    # const _: () = assert!(...) is compile-time; `expect(` inside a #[cfg(test)] block is not
    # runtime. Everything else is a candidate the A2 agents triage.
    case "$txt" in
      *"const _"*|*"static_assert"*) continue ;;
    esac
    panic_hits+=$'\n'"    $f:$ln: $(echo "$txt" | sed 's/^[[:space:]]*//' | cut -c1-100)"
  done < <(git diff -U0 "$BASE" -- "$f" \
            | awk '/^(\+\+\+|---|diff |index |new file|deleted file|similarity|rename |old mode|new mode)/ { next }
                   /^@@/{ if (match($0,/\+[0-9]+/)) { ln=substr($0,RSTART+1,RLENGTH-1)+0 } ; next }
                   /^\+/ { s=substr($0,2);
                           if (s ~ /(panic!|todo!|unimplemented!|\.unwrap\(\)|\.expect\()/ && s !~ /^[[:space:]]*\/\//)
                             printf "%d:%s\n", ln, s;
                           ln++ ; next }
                   /^-/  { next }
                   { ln++ }')
done
if [ -n "$panic_hits" ]; then fail "no-panic-on-runtime-data" "added panic paths:$panic_hits"
else pass "no-panic-on-runtime-data" "no added panic!/todo!/unwrap/expect outside kmd_logic"; fi

# ---------------------------------------------------------------------------
# 3. No `#[allow(...)]` on a hand-written added line (R908). Generated code may be allowed.
# ---------------------------------------------------------------------------
allow_hits=""
for f in "${SRC[@]}"; do
  [[ "$f" == *.rs ]] || continue
  [ -f "$f" ] || continue
  while IFS= read -r ln; do
    allow_hits+=$'\n'"    $f:$ln: $(sed -n "${ln}p" "$f" | sed 's/^[[:space:]]*//')"
  done < <(git diff -U0 "$BASE" -- "$f" \
            | awk '/^(\+\+\+|---|diff |index |new file|deleted file|similarity|rename |old mode|new mode)/ { next }
                   /^@@/{ if (match($0,/\+[0-9]+/)) { ln=substr($0,RSTART+1,RLENGTH-1)+0 } ; next }
                   /^\+/ { s=substr($0,2);
                           # ⛔ Drop comment lines. §10 records "a grep check can count its own
                           # documentation" as a scar, and the FIRST run of this check reproduced
                           # it exactly: 3 of its 5 hits were doc comments quoting the rule.
                           if (s ~ /#\[allow\(/ && s !~ /^[[:space:]]*\/\//) print ln;
                           ln++ ; next }
                   /^-/  { next }
                   { ln++ }')
done
if [ -n "$allow_hits" ]; then fail "no-hand-written-allow" "$allow_hits"
else pass "no-hand-written-allow"; fi

# ---------------------------------------------------------------------------
# 4. static_assert anchor count == 1 (ead692e). The ANCHORED form is what works: the bare word
#    and the trailing-paren form both count the comments that quote them and report 3.
#    NEVER `git grep` -- it skips untracked files, so a new umd12/bridge/ reads 0.
# ---------------------------------------------------------------------------
sa=$(grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge umd_common/bridge 2>/dev/null | wc -l)
if [ "$sa" -ne 1 ]; then fail "static_assert-count" "expected 1, got $sa"; else pass "static_assert-count" "1"; fi

# ---------------------------------------------------------------------------
# 5. Shared files: tables12.rs must have an EMPTY diff (§5).
# ---------------------------------------------------------------------------
t12=$(git diff --name-only "$BASE" -- 'umd12/src/tables12.rs' | wc -l)
if [ "$t12" -ne 0 ]; then fail "tables12-untouched" "tables12.rs changed in the changeset"; else pass "tables12-untouched"; fi

# ---------------------------------------------------------------------------
# 6. Slot coverage -- a slot with TWO owners is silent.
# ---------------------------------------------------------------------------
sc=$(bash tools/umd12-slot-coverage.sh 2>&1); sc_rc=$?
if [ $sc_rc -ne 0 ]; then fail "slot-coverage" "$(echo "$sc" | tail -5)"
else pass "slot-coverage" "$(echo "$sc" | head -1)"; fi

# ---------------------------------------------------------------------------
# 7. ASCII log check -- the READER is PowerShell 5.1 at the ANSI code page.
# ---------------------------------------------------------------------------
al=$(bash tools/umd12-log-ascii-check.sh 2>&1); al_rc=$?
if [ $al_rc -ne 0 ]; then fail "log-ascii" "$(echo "$al" | tail -8)"; else pass "log-ascii"; fi

# ---------------------------------------------------------------------------
# 8. clippy -D warnings THROUGH the script (a bare cargo clippy dies in link-cplusplus).
# ---------------------------------------------------------------------------
cl=$(bash tools/umd12-host-check.sh --clippy -- -D warnings 2>&1); cl_rc=$?
if [ $cl_rc -ne 0 ]; then fail "umd12-clippy" "$(echo "$cl" | grep -E '^(error|warning)' | head -10)"
else pass "umd12-clippy"; fi

# ---------------------------------------------------------------------------
# 9. kmd_logic test suite -- the only KMD code whose tests actually run.
# ---------------------------------------------------------------------------
kl=$(cd kmd_logic && CARGO_TARGET_DIR=../target/linux cargo test --quiet 2>&1); kl_rc=$?
if [ $kl_rc -ne 0 ]; then fail "kmd_logic-tests" "$(echo "$kl" | tail -15)"
else pass "kmd_logic-tests" "$(echo "$kl" | grep -oE '[0-9]+ passed' | head -1)"; fi

# ---------------------------------------------------------------------------
# 10. protocol clippy -- DIFFED against the base ref, not against a number in a doc.
#     ⛔ The handoff said "5 pre-existing too_many_arguments". It is FOUR errors, three of which
#     are `doc list item without indentation` in escape.rs (a file this changeset never touches)
#     and one `too_many_arguments` in wddm.rs. A hard-coded count is a claim and goes stale; the
#     base worktree cannot.
# ---------------------------------------------------------------------------
proto_sites() { (cd "$1/protocol" && CARGO_TARGET_DIR="$2" cargo clippy --quiet --all-targets -- -D warnings 2>&1 \
                 | grep -E '^\s+-->' | sed 's/^[[:space:]]*//' | sort); }
BASE_WT=/tmp/claude-1000/dx12-a1-base
if [ ! -d "$BASE_WT" ]; then git worktree add -q --detach "$BASE_WT" "$BASE" >/dev/null 2>&1; fi
now=$(proto_sites . target/linux)
was=$(proto_sites "$BASE_WT" "$BASE_WT/tgt")
newly=$(comm -13 <(echo "$was") <(echo "$now"))
if [ -n "$newly" ]; then fail "protocol-clippy" "sites NOT present at $BASE: $newly"
else pass "protocol-clippy" "$(echo "$now" | grep -c . ) sites, identical to $BASE"; fi

echo
[ $rc -eq 0 ] && echo "== A1 CLEAN ==" || echo "== A1 HAS FINDINGS =="
exit $rc
