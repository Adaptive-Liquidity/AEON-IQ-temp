#!/usr/bin/env bash
#
# Turn a gitleaks invocation into a CI verdict.
#
# Usage: gitleaks-verdict.sh <exit-code> <report-path> <scope-label>
#
# The point of this script is that "gitleaks did not find a secret" and
# "gitleaks did not run" both leave you with a zero-length finding list. Only
# the first is a pass. A scan that crashed, was killed, hit a bad config, or
# never executed at all must fail the job rather than quietly report clean --
# a secret scanner that silently no-ops is worse than none, because the green
# check gets taken as evidence.
#
# gitleaks exit codes, confirmed against 8.30.1:
#   0  scan completed, no findings
#   1  scan completed, findings present (the value of --exit-code)
#   *  anything else is a failure of the scan itself, not a verdict about the
#      code. Treated as fatal.
set -euo pipefail

rc="${1:?exit code required}"
report="${2:?report path required}"
scope="${3:?scope label required}"

summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

fail() {
  echo "::error::$*"
  echo "**Secret scan FAILED** (${scope}): $*" >> "${summary}"
  exit 1
}

# gitleaks writes the report file even when it finds nothing. A missing file
# means the process died before it got that far.
if [ ! -f "${report}" ]; then
  fail "gitleaks exited ${rc} but wrote no report to ${report}; the scan did not complete."
fi

if [ "${rc}" != "0" ] && [ "${rc}" != "1" ]; then
  fail "gitleaks exited with unexpected code ${rc}; treating as a failed scan, not a clean result."
fi

# Parse the report rather than trusting the exit code alone, so that a
# disagreement between the two is itself an error.
findings=$(python3 -c "
import json, os, sys
p = sys.argv[1]
if os.path.getsize(p) == 0:
    print(0); sys.exit()
try:
    print(len(json.load(open(p))))
except Exception as exc:
    print('PARSE_ERROR:%s' % exc)
" "${report}")

case "${findings}" in
  PARSE_ERROR:*)
    fail "could not parse the gitleaks report (${findings#PARSE_ERROR:}); cannot confirm the scan produced a usable result."
    ;;
esac

if [ "${rc}" = "0" ] && [ "${findings}" != "0" ]; then
  fail "gitleaks exited 0 but reported ${findings} finding(s); refusing to pass on a contradictory result."
fi

if [ "${rc}" = "1" ] && [ "${findings}" = "0" ]; then
  fail "gitleaks exited 1 but reported no findings; refusing to guess at the outcome."
fi

if [ "${findings}" = "0" ]; then
  echo "Secret scan clean (${scope})."
  echo "Secret scan clean — ${scope}." >> "${summary}"
  exit 0
fi

# Findings exist. Print location only. The report was produced with --redact so
# the secret values are already masked, but there is no reason to echo even a
# redacted match into a log that may be world-readable.
echo "::error::${findings} secret(s) detected in ${scope}."
{
  echo "### Secret scan FAILED — ${scope}"
  echo
  echo "${findings} finding(s). Secret values are redacted and are not printed here."
  echo
  echo "| Rule | File | Line | Commit |"
  echo "| --- | --- | --- | --- |"
} >> "${summary}"

python3 -c "
import json, sys
for f in json.load(open(sys.argv[1])):
    print('| %s | %s | %s | %s |' % (
        f.get('RuleID', '?'),
        f.get('File', '?'),
        f.get('StartLine', '?'),
        (f.get('Commit') or 'working tree')[:12],
    ))
" "${report}" | tee -a "${summary}"

echo
echo "Remediate by removing the secret and rotating it. If the value is provably"
echo "not a credential, add a narrowly scoped entry to .gitleaks.toml keyed to"
echo "the literal -- never to a path. See the comments in that file."
exit 1
