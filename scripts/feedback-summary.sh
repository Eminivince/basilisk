#!/usr/bin/env bash
# Set 9.6 / CP9.6.6 — top-N feedback aggregation.
#
# Walks `sessions.db` (and the side `bench_runs` / `bench_review_verdicts`
# tables) and prints the most common patterns the agent reports across
# sessions. Surfaces:
#
#   1. Recurring limitation / suspicion themes (top tool-input phrases).
#   2. Recurring miss-classes from `audit bench review` verdicts.
#   3. Sessions with the highest limitation / suspicion counts (likely
#      the messiest engagements — first place to look during calibration).
#
# Usage:
#   scripts/feedback-summary.sh                         # default db, N=10
#   scripts/feedback-summary.sh --db /path/to/db        # different db
#   scripts/feedback-summary.sh --top 25                # different cap
#   scripts/feedback-summary.sh --json                  # machine-readable
#
# Requires: sqlite3, jq.
#
# This is a calibration tool, not a permanent test. It exists so the
# operator can decide what to teach the agent next based on its own
# self-reported gaps, rather than guessing.

set -euo pipefail

DB="${BASILISK_SESSION_DB:-$HOME/.basilisk/sessions.db}"
TOP=10
JSON=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db) DB="$2"; shift 2 ;;
        --top) TOP="$2"; shift 2 ;;
        --json) JSON=1; shift ;;
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

if [[ ! -f "$DB" ]]; then
    echo "no session db at $DB. Run a session first or pass --db." >&2
    exit 1
fi

for cmd in sqlite3 jq; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "missing required tool: $cmd" >&2
        exit 1
    fi
done

# --- 1. Recurring limitation / suspicion themes ------------------------------
#
# Agent feedback is stored as JSON in payload_json. The agent's prompt
# convention puts a short headline in either `title`, `summary`, or
# `category` — extract whichever is present and bucket by lowercased
# stem. This is heuristic; the operator's job is to read the top-N and
# decide what's worth a permanent fix.

extract_themes_query() {
    local kind="$1"
    sqlite3 -separator $'\t' "$DB" \
        "SELECT payload_json FROM session_feedback WHERE kind = '$kind';"
}

aggregate_themes() {
    local kind="$1"
    # The agent's tool inputs vary by kind:
    #   - suspicion / limitation: `description`, often with a `location`
    #   - self_critique: `methodology_gaps` essay
    # Extract whichever field is present; truncate to the first 100
    # chars so recurring phrasings cluster cleanly under uniq.
    extract_themes_query "$kind" | \
        jq -R -r 'fromjson? // empty
               | (.description // .methodology_gaps // .title
                  // .category // .summary // "")
               | ascii_downcase
               | .[0:100]' | \
        awk 'NF' | \
        sort | uniq -c | sort -rn | head -n "$TOP"
}

# --- 2. Bench-review miss-class leaderboard ----------------------------------

aggregate_review_verdicts() {
    sqlite3 -separator $'\t' "$DB" \
        "SELECT verdict, COUNT(*) AS n
         FROM bench_review_verdicts
         GROUP BY verdict
         ORDER BY n DESC
         LIMIT $TOP;" 2>/dev/null || true
}

aggregate_recurring_miss_classes() {
    sqlite3 -separator $'\t' "$DB" \
        "SELECT label, COUNT(*) AS n
         FROM bench_review_verdicts
         WHERE kind = 'miss' AND verdict = 'actual_miss'
         GROUP BY label
         ORDER BY n DESC
         LIMIT $TOP;" 2>/dev/null || true
}

# --- 3. Messiest sessions ----------------------------------------------------

messiest_sessions() {
    sqlite3 -separator $'\t' "$DB" \
        "SELECT s.id, s.target,
                COALESCE(SUM(CASE WHEN f.kind = 'limitation' THEN 1 ELSE 0 END), 0) AS lim,
                COALESCE(SUM(CASE WHEN f.kind = 'suspicion'  THEN 1 ELSE 0 END), 0) AS sus
         FROM sessions s
         LEFT JOIN session_feedback f ON f.session_id = s.id
         GROUP BY s.id, s.target
         HAVING (lim + sus) > 0
         ORDER BY (lim + sus) DESC
         LIMIT $TOP;"
}

# --- output ------------------------------------------------------------------

if [[ "$JSON" -eq 1 ]]; then
    # JSON mode: emit one object with each section as an array of records.
    # Keeps it greppable and pipe-friendly for ad-hoc tooling.
    extract_jq='fromjson? // empty | (.description // .methodology_gaps // .title // .category // .summary // "") | .[0:100]'
    j_lim=$(extract_themes_query limitation \
        | jq -R -r "$extract_jq" \
        | jq -R . \
        | jq -s 'group_by(.) | map({theme: .[0], count: length}) | sort_by(-.count) | .[0:'"$TOP"']')
    j_sus=$(extract_themes_query suspicion \
        | jq -R -r "$extract_jq" \
        | jq -R . \
        | jq -s 'group_by(.) | map({theme: .[0], count: length}) | sort_by(-.count) | .[0:'"$TOP"']')
    j_verdicts=$(aggregate_review_verdicts | jq -R 'split("\t") | {verdict: .[0], count: (.[1] // "0" | tonumber)}' | jq -s '.')
    j_misses=$(aggregate_recurring_miss_classes | jq -R 'split("\t") | {miss_class: .[0], count: (.[1] // "0" | tonumber)}' | jq -s '.')
    j_sessions=$(messiest_sessions | jq -R 'split("\t") | {session_id: .[0], target: .[1], limitations: (.[2] // "0" | tonumber), suspicions: (.[3] // "0" | tonumber)}' | jq -s '.')
    jq -n \
        --argjson lim "$j_lim" \
        --argjson sus "$j_sus" \
        --argjson verdicts "$j_verdicts" \
        --argjson misses "$j_misses" \
        --argjson sessions "$j_sessions" \
        '{db: $ENV.DB, top_n: ($ENV.TOP|tonumber),
          limitations: $lim, suspicions: $sus,
          review_verdicts: $verdicts, recurring_miss_classes: $misses,
          messiest_sessions: $sessions}'
    exit 0
fi

# Pretty mode.
echo "=== feedback summary (db: $DB, top $TOP) ==="
echo
echo "--- recurring limitations (top $TOP) ---"
out=$(aggregate_themes limitation || true)
if [[ -z "$out" ]]; then
    echo "(none recorded)"
else
    echo "$out"
fi
echo
echo "--- recurring suspicions (top $TOP) ---"
out=$(aggregate_themes suspicion || true)
if [[ -z "$out" ]]; then
    echo "(none recorded)"
else
    echo "$out"
fi
echo
echo "--- bench review verdict tally ---"
out=$(aggregate_review_verdicts)
if [[ -z "$out" ]]; then
    echo "(no bench reviews recorded — run 'audit bench review <run_id>')"
else
    echo "$out"
fi
echo
echo "--- recurring actual-miss classes ---"
out=$(aggregate_recurring_miss_classes)
if [[ -z "$out" ]]; then
    echo "(none recorded)"
else
    echo "$out"
fi
echo
echo "--- messiest sessions (sum of limitations + suspicions) ---"
out=$(messiest_sessions)
if [[ -z "$out" ]]; then
    echo "(no feedback recorded)"
else
    printf "%-40s  %-30s  %5s  %5s\n" "session_id" "target" "lim" "sus"
    echo "$out" | awk -F'\t' '{ printf "%-40s  %-30s  %5s  %5s\n", $1, substr($2,1,30), $3, $4 }'
fi
