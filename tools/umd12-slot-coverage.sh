#!/usr/bin/env bash
# tools/umd12-slot-coverage.sh — which of `umd12`'s 206 DDI slots are still counting
# noops, and does any slot have TWO owners.
#
# ⭐ THE STATIC HALF OF THE `CONFORMANCE.md` CHARTER. That charter — *drive the noop-DDI
# hit counters to zero* — and `PARALLEL.md` §9.2's per-lane definition of done are both
# written against the RUNTIME instrument, `forward12::noop12::log_noop_hits()`. That
# instrument is exact, and it is also the most expensive readout this project has: it
# needs a build, a signed deploy, a `-RestartDevice` and a gate run on the single `win11`
# VM. This script answers the cheap 90% of the same question from the source tree, in
# under a second, with no VM at all — so a lane can see its own progress and an integrator
# can see the fan-out's, between VM leases rather than only across them.
#
# ⚠ **It measures INSTALLATION, not correctness.** A slot counted `filled` here has a
# typed handler assigned into the table; whether that handler does the right thing is what
# the review pass and the gate are for. The runtime counters remain the authority for
# "was it CALLED", which is a different question again — `DDI_REFERENCE.md` §14.0
# estimates which slots a real workload reaches, and only a run can settle it.
#
# ⛔ THE OTHER HALF, AND THE REASON THIS EXISTS AT ALL: **a duplicate owner is silent.**
# The install chain in `forward12/tables12.rs` runs each lane's `install*` in turn over one
# table, so if two lanes both assign `pfnCreateCommandSignature`, the LATER lane in the
# chain wins and the earlier one's handler is never reachable. Nothing warns: it compiles,
# both files look complete, both lanes report the slot done, and the defect surfaces as a
# behaviour nobody can attribute. `PARALLEL.md` §4 says "each lane owns its files
# exclusively" but the thing lanes actually contend for is SLOTS, and the slot partition
# lives in prose (`DDI_REFERENCE.md` §3.2), not in any type. This makes the collision an
# exit code.
#
# Usage:
#     tools/umd12-slot-coverage.sh            # summary + the unfilled list
#     tools/umd12-slot-coverage.sh --quiet    # summary + duplicates only
#
# Exit 0 when no slot has two owners; 1 (with the collisions named) otherwise.
#
# ⚠ How it decides. The slot NAMES come from `forward12/noop12.rs`'s `ddi_noop_table!`
# invocations, which are themselves compiler-checked against the bindgen structs on every
# build (the `OFFSETS[i] == i * size_of::<usize>()` proof) — so the name list is derived
# from the ABI and cannot silently drift. The ASSIGNMENTS come from a grep for
# `.pfn<Name> = Some(` across the DDI surface. That is textual, and it is deliberately
# narrow: it matches the one form `PARALLEL.md` §7 asks a lane to write, so a lane using a
# different form reads as *unfilled* here — which is the safe direction for an instrument
# whose failure mode would otherwise be over-reporting progress.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${script_dir}/.." && pwd)"
readonly SRC="${REPO_ROOT}/umd12/src"
readonly NOOP="${SRC}/forward12/noop12.rs"

quiet=0
if [[ "${1:-}" == "--quiet" ]]; then
    quiet=1
fi

if [[ ! -f "${NOOP}" ]]; then
    echo "umd12-slot-coverage: no ${NOOP}" >&2
    exit 1
fi

# ── the slot names, per table, straight out of the macro invocations ────────
#
# Each `ddi_noop_table! { ..., <mod>, <type>, <TABLE_ID>, [ names ] }` block ends at the
# closing `]`. `awk` walks the bracket list and emits `table<TAB>slot`.
slots="$(awk '
    /^ddi_noop_table! \{/ { in_macro = 1; table = ""; next }
    in_macro && table == "" && /^ *[a-z_]+, ddi12::/ {
        split($0, f, ",");
        gsub(/^ +| +$/, "", f[1]);
        table = f[1];
        next
    }
    in_macro && table != "" {
        if ($0 ~ /^ *\]/) { in_macro = 0; table = ""; next }
        line = $0
        gsub(/[^A-Za-z0-9_]/, " ", line)
        n = split(line, w, " ")
        for (i = 1; i <= n; i++) {
            if (w[i] ~ /^pfn[A-Za-z0-9_]+$/) print table "\t" w[i]
        }
    }
' "${NOOP}")"

if [[ -z "${slots}" ]]; then
    echo "umd12-slot-coverage: parsed ZERO slot names out of ${NOOP##*/}." >&2
    echo "umd12-slot-coverage: the macro shape moved; fix this script rather than" >&2
    echo "umd12-slot-coverage: trusting a zero. An instrument that reads empty is not" >&2
    echo "umd12-slot-coverage: an instrument that reads clean." >&2
    exit 1
fi

# ── the assignments: `<expr>.pfnName = Some(` anywhere in the DDI surface ───
#
# ⚠ `caps12.rs` is included: L1 lives there rather than under `forward12/`, and its three
# device-core slots are as much a slot owner as any lane.
#
# ⛔ COMMENT LINES ARE DROPPED, AND THIS SCRIPT'S FIRST RUN IS WHY. `PARALLEL.md` §10 lists
# *"a grep check can count its own documentation"* as a scar — the `static_assert`
# invariant reported 3 for months because both its patterns matched the comments quoting
# them. The very first run of this script reported `pfnCreateCommandQueue` installed
# **twice**, by `noop12.rs:365` and `tables12.rs:21`: both are prose, teaching a lane to
# write `f.pfnCreateCommandQueue = Some(handler)`. A duplicate-owner check that fires on
# the doc that explains duplicate owners is worse than no check, because its first real
# finding arrives in a report nobody trusts any more.
#
# The filter is `//`-or-`*` at the start of the trimmed line, which covers `//`, `///`,
# `//!` and the continuation lines of a `/* */` block. It cannot see an assignment inside a
# string literal or a trailing comment — both would read as *installed*, i.e.
# over-reporting — so if this ever disagrees with the runtime counters, suspect those two
# shapes first.
#
# ⚠⚠ **THE MATCHER IS FORMATTING-INSENSITIVE BY CONSTRUCTION, AND IT TOOK THREE WRONG
# ANSWERS IN ONE SITTING TO GET THERE.** A naive `\.pfnX = Some\(` grep missed two whole
# classes on the first S6 merge, both silently and both under-reporting:
#
#   1. `Some(` is not the only install form. The command-queue table's `pfnUnused` /
#      `pfnUnused2` are bindgen'd as bare `*mut ::std::os::raw::c_void` — the header
#      declares them as untyped reserved words, not `Option<unsafe extern "C" fn(..)>` — so
#      they install as `table.pfnUnused = queue_unused_slot as *mut c_void;`.
#   2. **rustfmt wraps.** Three slots with long handler names install as
#      `table.pfnCalcPrivateGeometryShaderWithStreamOutput =` followed by `Some(handler);`
#      on the NEXT line, and a line-at-a-time grep cannot see them.
#
# Patching the pattern a third time is how an instrument becomes a thing nobody believes,
# so the shape changed instead: continuation lines are **joined** before matching (any line
# whose last non-space character is `=` absorbs the next), and the match is then any
# `.pfnName = <rhs>` whose rhs is not `None`. It still cannot match `==` or `!=`, both of
# which need a second `=` where this needs a space or an end of line.
#
# ⚠ What it still cannot see, both of which OVER-report: an assignment inside a string
# literal, and one in a trailing comment. If this ever disagrees with the runtime counters,
# suspect those two first.
assignments="$(
    while IFS= read -r -d '' f; do
        awk -v file="${f}" '
            {
                line = $0
                lineno = FNR
                # Join rustfmt continuation lines: a statement broken after `=`.
                while (line ~ /=[ \t]*$/ && (getline nxt) > 0) {
                    sub(/^[ \t]+/, "", nxt)
                    line = line " " nxt
                }
                trimmed = line
                gsub(/^[ \t]+/, "", trimmed)
                if (trimmed ~ /^\/\//) next
                if (trimmed ~ /^\*/) next
                if (trimmed ~ /\.pfn[A-Za-z0-9_]+ = None/) next
                if (match(trimmed, /\.pfn[A-Za-z0-9_]+ = [^=]/)) {
                    # The match is `.` + name + ` = ` + one non-`=`, so the name is
                    # RLENGTH - 5 characters starting one past the dot.
                    slot = substr(trimmed, RSTART + 1, RLENGTH - 5)
                    print slot "\t" file ":" lineno
                }
            }
        ' "${f}"
    done < <(find "${SRC}/forward12" "${SRC}/caps12.rs" -name '*.rs' -print0) || true
)"

status=0
total=0
filled=0
unfilled_list=""

# ── per-table coverage ─────────────────────────────────────────────────────
prev_table=""
table_total=0
table_filled=0
summary=""

emit_table() {
    if [[ -n "${prev_table}" ]]; then
        summary+="  ${prev_table}: ${table_filled}/${table_total}"$'\n'
    fi
}

while IFS=$'\t' read -r table slot; do
    [[ -z "${slot}" ]] && continue
    if [[ "${table}" != "${prev_table}" ]]; then
        emit_table
        prev_table="${table}"
        table_total=0
        table_filled=0
    fi
    total=$((total + 1))
    table_total=$((table_total + 1))
    hits="$(printf '%s\n' "${assignments}" | awk -F'\t' -v s="${slot}" '$1 == s {print $2}')"
    count="$(printf '%s' "${hits}" | grep -c . || true)"
    if [[ "${count}" -ge 1 ]]; then
        filled=$((filled + 1))
        table_filled=$((table_filled + 1))
    else
        unfilled_list+="    ${table}::${slot}"$'\n'
    fi
    if [[ "${count}" -ge 2 ]]; then
        echo "COLLISION: ${table}::${slot} is installed ${count} times:" >&2
        printf '%s\n' "${hits}" | sed 's|^|    |' >&2
        status=1
    fi
done <<<"${slots}"
emit_table

echo "umd12 DDI slot installation: ${filled}/${total} filled, $((total - filled)) still counting noops"
printf '%s' "${summary}"

if [[ "${quiet}" -eq 0 && -n "${unfilled_list}" ]]; then
    echo "  still noops:"
    printf '%s' "${unfilled_list}"
fi

if [[ "${status}" -ne 0 ]]; then
    echo "" >&2
    echo "umd12-slot-coverage: a slot with two owners is SILENT at runtime -- the install" >&2
    echo "chain in tables12.rs runs the lanes in order over one table, so the later lane" >&2
    echo "wins and the earlier handler is unreachable. Settle the ownership against" >&2
    echo "DDI_REFERENCE.md 3.2 before merging." >&2
fi
exit "${status}"
