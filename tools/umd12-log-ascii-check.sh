#!/usr/bin/env bash
# tools/umd12-log-ascii-check.sh — no non-ASCII outside comments in `umd12`.
#
# ⭐ THE POINT IS THE *READER*, NOT THE WRITER. The UMD writes its log as UTF-8
# and does so correctly: an em dash lands in `umd12-<pid>.log` as U+2014, which
# `[System.Text.Encoding]::UTF8.GetString` reads back perfectly (verified
# 2026-08-06 on the S6-0 fill-table run). But **every gate script, every
# `win_exec` and every triage step in this project reads those logs with
# `Get-Content`**, and PowerShell 5.1's `Get-Content` defaults to the ANSI code
# page. The same line then reads:
#
#     ... this header's struct is 992 B <?" filling 984 B and leaving the rest
#
# So a non-ASCII character in a log format string is not a writer bug — it is a
# line that is mojibake in the one reader anyone actually uses. Same family as
# `BRINGUP_QUIRKS`' rule for PowerShell scripts on the `Z:\` 9p share, where an
# em dash inside a `throw` broke `build-g1-static.ps1`'s parse.
#
# ⛔ COMMENTS AND DOC COMMENTS ARE EXEMPT AND MUST STAY THAT WAY. They are read
# by `rustdoc`, by editors and by people, none of which are PowerShell 5.1, and
# the ⭐/⛔/⚠ markers this codebase uses to grade its own warnings are load-
# bearing. This checks only what can reach a log file.
#
# Usage:
#     tools/umd12-log-ascii-check.sh            # umd12 (default)
#     tools/umd12-log-ascii-check.sh umd12 umd_common
#
# Exit 0 when clean, 1 with the offending `file:line: text` otherwise.
#
# ⚠ KNOWN AND DELIBERATELY NOT FIXED HERE: `umd/src/adapter.rs`'s
# "adapter handle not ours: ... — counted only" carries the same em dash. It is
# the D3D11 driver, it sits behind an eight-hit log budget on a path that is
# expected never to fire, and changing it would perturb the shipping D3D11
# binary in a commit that has no other reason to. It is why `umd` is not in the
# default crate list rather than an oversight.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "${script_dir}/.." && pwd)"

crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
    crates=(umd12)
fi

status=0
for crate in "${crates[@]}"; do
    src="${REPO_ROOT}/${crate}/src"
    if [[ ! -d "${src}" ]]; then
        echo "umd12-log-ascii-check: no such crate source directory: ${src}" >&2
        exit 1
    fi
    # ⛔ `ddi12.rs` is excluded: it is one `include!` of 5.4 MB of bindgen output
    # regenerated from the SDK header on every Windows build. Nothing in it is a
    # log string, and a hand edit between the header and the generated ABI is
    # precisely what stage S3 exists to prevent.
    while IFS= read -r -d '' file; do
        case "${file}" in
            */ddi12.rs) continue ;;
        esac
        # Drop whole-line comments, then look for any byte above U+007F.
        # `grep -n` on the filtered stream would renumber, so filter per line.
        while IFS= read -r numbered; do
            line_no="${numbered%%:*}"
            text="${numbered#*:}"
            trimmed="${text#"${text%%[![:space:]]*}"}"
            case "${trimmed}" in
                //*) continue ;;
            esac
            if LC_ALL=C grep -qP '[^\x00-\x7F]' <<<"${text}"; then
                echo "${file#"${REPO_ROOT}/"}:${line_no}: ${text}"
                status=1
            fi
        done < <(grep -n '' "${file}")
    done < <(find "${src}" -name '*.rs' -print0)
done

if [[ ${status} -eq 0 ]]; then
    echo "umd12-log-ascii-check: clean (${crates[*]})"
else
    echo "" >&2
    echo "umd12-log-ascii-check: non-ASCII outside a comment — it will reach a log" >&2
    echo "line that PowerShell 5.1's Get-Content renders as mojibake. Use ASCII in" >&2
    echo "string literals; comments may keep their markers." >&2
fi
exit "${status}"
