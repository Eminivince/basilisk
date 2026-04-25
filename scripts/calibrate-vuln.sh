#!/usr/bin/env bash
# Set 9.5 / CP9.5.2 — one-shot operator script.
#
# Re-runs the canonical Set 9 calibration target (WaveLauncherMainnet,
# `0xB9873b482d51b8b0f989DCD6CCf1D91520092b95` on ethereum) under
# `--vuln` with Sonnet as the model. Captures the session id and prints
# a summary that can be diffed against the original Run C
# (`b6e2dc7c-a44a-43ca-bd97-f04ab4cf0d74`, Opus, $25.12, 9 min).
#
# Usage:
#   scripts/calibrate-vuln.sh                    # Sonnet, default
#   scripts/calibrate-vuln.sh --opus             # re-run with Opus for parity
#   scripts/calibrate-vuln.sh --target 0x...     # different address
#
# This is NOT a permanent test. It is a calibration tool for evaluating
# whether the Sonnet default holds. After Set 9.5 ships, operators run
# this once on their environment and decide.

set -euo pipefail

TARGET="0xB9873b482d51b8b0f989DCD6CCf1D91520092b95"
MODEL="claude-sonnet-4-6"
PROVIDER="anthropic"
DB="${BASILISK_SESSION_DB:-$HOME/.basilisk/sessions.db}"
NOTE="set 9.5 calibration on WaveLauncher (sonnet)"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --opus)
            MODEL="claude-opus-4-7"
            NOTE="set 9.5 calibration on WaveLauncher (opus, parity)"
            shift
            ;;
        --target)
            TARGET="$2"
            shift 2
            ;;
        --provider)
            PROVIDER="$2"
            shift 2
            ;;
        --model)
            MODEL="$2"
            shift 2
            ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

# When the provider is openrouter, the model id needs the prefix.
if [[ "$PROVIDER" == "openrouter" && "$MODEL" != */* ]]; then
    MODEL="anthropic/$MODEL"
fi

echo "=== Set 9.5 calibration ==="
echo "  target:   $TARGET"
echo "  provider: $PROVIDER"
echo "  model:    $MODEL"
echo "  db:       $DB"
echo

audit recon "$TARGET" \
    --chain ethereum \
    --vuln \
    --provider "$PROVIDER" \
    --model "$MODEL" \
    --agent-output=pretty \
    --session-note "$NOTE" \
    || { echo "audit run failed" >&2; exit 1; }

# Pull the freshly-created session row.
SESSION_ID=$(
    sqlite3 "$DB" \
        "SELECT id FROM sessions ORDER BY created_at_ms DESC LIMIT 1;"
)

echo
echo "=== Calibration summary ==="
echo "session_id: $SESSION_ID"
echo

sqlite3 -separator $'\t' "$DB" "
    SELECT
        json_extract(stats_json, '\$.turns')        AS turns,
        json_extract(stats_json, '\$.tool_calls')   AS tool_calls,
        json_extract(stats_json, '\$.cost_cents')   AS cost_cents,
        json_extract(stats_json, '\$.duration_ms')  AS duration_ms,
        stop_reason
    FROM sessions WHERE id = '$SESSION_ID';" \
    | awk -F'\t' '{
        printf "  turns:       %s\n", $1
        printf "  tool_calls:  %s\n", $2
        printf "  cost:        $%.2f\n", $3 / 100.0
        printf "  duration:    %ds\n", $4 / 1000
        printf "  stop_reason: %s\n", $5
    }'

echo
echo "  feedback rows by kind:"
sqlite3 -separator $'\t' "$DB" "
    SELECT kind, COUNT(*) FROM session_feedback
    WHERE session_id = '$SESSION_ID' GROUP BY kind;" \
    | awk -F'\t' '{ printf "    %-15s  %s\n", $1, $2 }'

echo
echo "  scratchpad item count:"
sqlite3 "$DB" "
    SELECT
        CAST(
            (LENGTH(state_json) - LENGTH(REPLACE(state_json, '\"id\":', '')))
            / LENGTH('\"id\":')
            AS INTEGER
        ) AS approx_items
    FROM scratchpads WHERE session_id = '$SESSION_ID';" \
    | awk '{ printf "    approx items: %s\n", $1 }'

echo
echo "Compare against Run C (b6e2dc7c, Opus, WaveLauncher):"
echo "  baseline cost:     \$25.12"
echo "  baseline duration: 543s (9m 03s)"
echo "  baseline turns:    8"
echo "  baseline tools:    13"
echo "  baseline rows:     self_critique=1, suspicion=0, limitation=0"
echo
echo "Inspect the full output:"
echo "  sqlite3 $DB \"SELECT final_report_markdown FROM sessions WHERE id = '$SESSION_ID';\""
echo "  audit session scratchpad show $SESSION_ID"
