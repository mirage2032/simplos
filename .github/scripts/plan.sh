#!/usr/bin/env bash
# Works out which of ci.yml's jobs a run has to run: its plan job.
#
# What a job's result depends on is named by its key: the contents of the
# paths ci.yml lists for it and of those every job depends on, as git hashes
# them (a directory by its tree), and the runner's image, whose QEMU and
# e2fsprogs the tests run with. A job that passes uploads an artifact named
# for its key (.github/actions/passed), and a run that finds one needn't run
# that job again: not a pull request that changes nothing the job depends on,
# nor develop after merging a pull request that passed it. Only artifacts of
# this repository's own runs count; a fork's pull request could upload any.
#
# On develop, and in a promotion, a job runs anyway if its cargo cache isn't
# there: pull requests only read the caches develop's runs save, so it's
# develop's runs that have to run the jobs that save them.
#
# Reads JOBS, a line for each job: its name, the cargo cache it saves (- for
# none) and the paths it depends on; and COMMON, the paths all of them do.
# Writes, as the step's outputs, `keys`, each job's key, and `run`, whether
# to run it, as JSON objects; and `tests`, the halves of the tests to run, as
# a JSON array.
#
# Usage: .github/scripts/plan.sh (from the repo root, in a workflow's step)
set -euo pipefail

: "${JOBS:?} ${COMMON:?} ${GITHUB_OUTPUT:?} ${GITHUB_REPOSITORY:?} ${GITHUB_REF:?} ${GITHUB_EVENT_NAME:?}"
image="${ImageOS:-unknown}-${ImageVersion:-unknown}"
keys='{}'
run='{}'
summary="| Job | Runs | Why |"$'\n'"| --- | --- | --- |"

while read -r job cache paths; do
    [[ -n $job ]] || continue
    for path in $COMMON $paths; do
        git cat-file -e "HEAD:$path" 2>/dev/null || {
            echo "::error::plan: $job depends on $path, which isn't in the repository"
            exit 1
        }
    done
    # shellcheck disable=SC2086 # the paths are words
    hash=$(git ls-tree HEAD -- $COMMON $paths | sha256sum | cut -c1-32)
    key=$job-$image-$hash

    # Run it if GitHub can't say: a check can be slow, but not wrong.
    passed=$(gh api "repos/$GITHUB_REPOSITORY/actions/artifacts?name=passed-$key" --jq '
        [.artifacts[] | select(.expired | not)
            | select(.workflow_run.head_repository_id == .workflow_run.repository_id)]
        | first | .workflow_run.id // empty') || passed=""
    if [[ -z $passed ]]; then
        runs=true why="no run has passed it with what it depends on as it is"
    elif [[ $GITHUB_EVENT_NAME != pull_request && $cache != - ]] &&
        cargo=$(.github/scripts/cargo-cache-key.sh "$cache") &&
        [[ $(gh api "repos/$GITHUB_REPOSITORY/actions/caches?ref=$GITHUB_REF&key=$cargo" \
            --jq "[.actions_caches[] | select(.key == \"$cargo\")] | length") == 0 ]]; then
        runs=true why="passed in run $passed, but $GITHUB_REF hasn't its cargo cache, $cargo, yet"
    else
        runs=false why="passed with the same in [run $passed]($GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$passed)"
    fi
    echo "$job: $runs, as $why ($key)"
    keys=$(jq -c --arg job "$job" --arg key "$key" '.[$job] = $key' <<<"$keys")
    run=$(jq -c --arg job "$job" --argjson runs "$runs" '.[$job] = $runs' <<<"$run")
    summary+=$'\n'"| $job | $runs | $why |"
done <<<"$JOBS"

tests=$(jq -c '[to_entries[] | select(.value and (.key | startswith("test-"))) | .key | ltrimstr("test-")]' <<<"$run")
{
    echo "keys=$keys"
    echo "run=$run"
    echo "tests=$tests"
} >>"$GITHUB_OUTPUT"
echo "$summary" >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
