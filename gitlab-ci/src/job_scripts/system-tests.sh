#!/usr/bin/env bash

set -exuo pipefail

cd "${CI_PROJECT_DIR}/rs"
# Setup PATH
mkdir local-bin
for f in "${CI_PROJECT_DIR}"/artifacts/release/*.gz; do
    target=$(basename "$f" .gz)
    gunzip -c -d "$f" >"local-bin/$target"
    chmod +x "local-bin/$target"
done

for f in "${CI_PROJECT_DIR}"/artifacts/release-malicious/*.gz; do
    target=$(basename "$f" .gz)
    gunzip -c -d "$f" >"local-bin/$target"
    chmod +x "local-bin/$target"
done

gunzip -c -d "${CI_PROJECT_DIR}/artifacts/release/orchestrator.gz" >"local-bin/orchestrator"
chmod +x "local-bin/orchestrator"
export PATH=$PWD/local-bin:$PATH

# shellcheck source=/dev/null
source "$CI_PROJECT_DIR/gitlab-ci/src/canisters/wasm-build-functions.sh"
export_wasm_canister_paths "${CI_PROJECT_DIR}/artifacts/canisters"

# Run system tests, writing its JSON output to disk to be uploaded to CI.
# Only tests that are being selected by test runner options are run.
# Note: due to the bash settings to fail on any error, we have to be very careful how we
# get the command exit status. If we don't collect the exit status properly, GitLab status
# will not be updated at the end of this script
"$SHELL_WRAPPER" nix-shell --run "
  set -exuo pipefail
  system-tests $TEST_RUNNER_ARGS | tee ci_output.json
" && RES=0 || RES=$?
echo "System tests finished with exit code $RES"

# Export runtime statistics of system tests to Honeycomb.
python3 "${CI_PROJECT_DIR}"/gitlab-ci/src/test_spans/exporter.py \
    --runtime_stats "${CI_PROJECT_DIR}"/runtime-stats.json \
    --trace_id "$ROOT_PIPELINE_ID" \
    --parent_id "$CI_JOB_ID" \
    --type "system-tests"

/usr/bin/time "${CI_PROJECT_DIR}/gitlab-ci/src/artifacts/collect_core_dumps.sh"
if [[ "$?" == 0 ]] && [[ $RES == 0 ]]; then
    # Check LTL predicates for replica logs collected during execution of the system tests.
    echo "Running the LTL analyzer..."
    REPLICA_LOGS_BASE_DIR=$(find "${CI_PROJECT_DIR}"/replica-logs/* -type d | head -1)
    cd "${CI_PROJECT_DIR}/hs/analyzer"
    buildevents cmd "$ROOT_PIPELINE_ID" "$CI_JOB_ID" transducer -- \
        "$SHELL_WRAPPER" nix-shell --run "
    set -exuo pipefail
    cabal run analyze $REPLICA_LOGS_BASE_DIR
  "
    RES=$?
else
    RES=1
fi

if [[ $RES -ne 0 ]]; then
    echo "FAILURE. READ ME:"
    echo "================="
    echo ""
    echo "(0) Currently, logs are analyzed only on CI. So you might encounter"
    echo "    failures on CI that cannot be reproduced locally (e.g. when"
    echo "    running setup-and-cargo-test.sh)."
    echo ""
    echo "(1) The logs produced by all nodes are stored with the CI Job artifacts."
    echo "    In case of any failure, please take a look at them before reporting "
    echo "    a problem."
    echo ""
    echo "(2) If any of the pots that are marked as 'experimental' failed (e.g."
    echo "    exp_basic_health_pot), NOTIFY the testing team and disable the test"
    echo "    on your PR with a corresponding comment."
    echo ""
    echo "    (Unfortunately, as of now, the tests are not run if some of the"
    echo "    of the dependencies, such as ic-os scripts, change. Thus, failures"
    echo "    might be reduced silently.)"
    echo ""
fi

exit $RES
