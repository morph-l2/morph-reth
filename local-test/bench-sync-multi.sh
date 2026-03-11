#!/usr/bin/env bash

set -euo pipefail

# Run bench-sync.sh multiple rounds and collect results.

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

: "${ROUNDS:=3}"
: "${TEST_DURATION:=300}"
: "${SKIP_GETH:=0}"
: "${SKIP_RETH:=0}"

echo "=========================================================="
echo "  Multi-Round Benchmark (${ROUNDS} rounds)"
echo "  TEST_DURATION=${TEST_DURATION}s per config per round"
echo "=========================================================="
echo

# Arrays to store per-round results
geth_bps_list=()
reth_bps_list=()

geth_peak_list=()
reth_peak_list=()

for round in $(seq 1 "${ROUNDS}"); do
  echo "############################################################"
  echo "  ROUND ${round}/${ROUNDS}"
  echo "############################################################"
  echo

  # Capture output and parse results
  output=$(TEST_DURATION="${TEST_DURATION}" \
    SKIP_GETH="${SKIP_GETH}" \
    SKIP_RETH="${SKIP_RETH}" \
    "${SCRIPT_DIR}/bench-sync.sh" 2>&1)

  echo "${output}"
  echo

  # Parse Avg BPS from the RESULTS table
  # Format: "Avg BPS              86.25            74.17"
  avg_line=$(echo "${output}" | grep "^Avg BPS" || true)
  if [[ -n "${avg_line}" ]]; then
    geth_avg=$(echo "${avg_line}" | awk '{print $3}')
    reth_avg=$(echo "${avg_line}" | awk '{print $4}')

    geth_bps_list+=("${geth_avg}")
    reth_bps_list+=("${reth_avg}")
  fi

  peak_line=$(echo "${output}" | grep "^Peak BPS" || true)
  if [[ -n "${peak_line}" ]]; then
    geth_peak=$(echo "${peak_line}" | awk '{print $3}')
    reth_peak=$(echo "${peak_line}" | awk '{print $4}')

    geth_peak_list+=("${geth_peak}")
    reth_peak_list+=("${reth_peak}")
  fi

  echo
  echo "  Round ${round} complete."
  echo
done

# ─── Compute averages ─────────────────────────────────────────────────────────

calc_avg() {
  local arr=("$@")
  local sum=0
  local count=0
  for v in "${arr[@]}"; do
    if [[ "${v}" != "0" ]]; then
      sum=$(echo "${sum} + ${v}" | bc)
      count=$((count + 1))
    fi
  done
  if [[ ${count} -gt 0 ]]; then
    echo "scale=2; ${sum} / ${count}" | bc
  else
    echo "0"
  fi
}

calc_min() {
  local arr=("$@")
  local min=999999
  for v in "${arr[@]}"; do
    if [[ "${v}" != "0" ]] && [[ $(echo "${v} < ${min}" | bc -l) -eq 1 ]]; then
      min="${v}"
    fi
  done
  if [[ "${min}" == "999999" ]]; then echo "0"; else echo "${min}"; fi
}

calc_max() {
  local arr=("$@")
  local max=0
  for v in "${arr[@]}"; do
    if [[ $(echo "${v} > ${max}" | bc -l) -eq 1 ]]; then
      max="${v}"
    fi
  done
  echo "${max}"
}

# ─── Final Summary ────────────────────────────────────────────────────────────

echo "=========================================================="
echo "  MULTI-ROUND SUMMARY (${ROUNDS} rounds)"
echo "=========================================================="
echo

# Per-round data
echo "--- Per-Round Avg BPS ---"
printf "%-8s  %12s  %12s\n" "Round" "Geth" "Reth"
for i in $(seq 0 $((ROUNDS - 1))); do
  printf "%-8s  %12s  %12s\n" \
    "$((i + 1))" \
    "${geth_bps_list[$i]:-N/A}" \
    "${reth_bps_list[$i]:-N/A}"
done

echo
echo "--- Aggregated Results (Avg BPS) ---"
geth_mean=$(calc_avg "${geth_bps_list[@]}")
reth_mean=$(calc_avg "${reth_bps_list[@]}")

geth_min=$(calc_min "${geth_bps_list[@]}")
reth_min=$(calc_min "${reth_bps_list[@]}")

geth_max=$(calc_max "${geth_bps_list[@]}")
reth_max=$(calc_max "${reth_bps_list[@]}")

printf "%-12s  %12s  %12s\n" "" "Geth" "Reth"
printf "%-12s  %12s  %12s\n" "---" "---" "---"
printf "%-12s  %12s  %12s\n" "Mean" "${geth_mean}" "${reth_mean}"
printf "%-12s  %12s  %12s\n" "Min" "${geth_min}" "${reth_min}"
printf "%-12s  %12s  %12s\n" "Max" "${geth_max}" "${reth_max}"

echo
echo "--- Peak BPS (per round) ---"
printf "%-8s  %12s  %12s\n" "Round" "Geth" "Reth"
for i in $(seq 0 $((ROUNDS - 1))); do
  printf "%-8s  %12s  %12s\n" \
    "$((i + 1))" \
    "${geth_peak_list[$i]:-N/A}" \
    "${reth_peak_list[$i]:-N/A}"
done

# Comparison
echo
echo "--- Geth vs Reth ---"
if [[ $(echo "${geth_mean} > 0 && ${reth_mean} > 0" | bc -l) -eq 1 ]]; then
  if [[ $(echo "${reth_mean} > ${geth_mean}" | bc -l) -eq 1 ]]; then
    diff_pct=$(echo "scale=1; (${reth_mean} - ${geth_mean}) * 100 / ${reth_mean}" | bc)
    echo "Reth faster by ${diff_pct}% (mean: geth=${geth_mean}, reth=${reth_mean})"
  elif [[ $(echo "${geth_mean} > ${reth_mean}" | bc -l) -eq 1 ]]; then
    diff_pct=$(echo "scale=1; (${geth_mean} - ${reth_mean}) * 100 / ${geth_mean}" | bc)
    echo "Geth faster by ${diff_pct}% (mean: geth=${geth_mean}, reth=${reth_mean})"
  else
    echo "Tie (mean: ${geth_mean})"
  fi
else
  echo "Insufficient data"
fi

echo
echo "=========================================================="
echo "  Multi-round benchmark complete"
echo "=========================================================="
