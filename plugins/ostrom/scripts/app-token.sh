#!/usr/bin/env bash
# Mint a short-lived GitHub App installation token for a delivery role.

# A caller may have tracing enabled. Disable it before credentials enter shell
# variables or command input, and never enable it again in this process.
set +x
set -euo pipefail

fail() {
  printf 'app-token: %s\n' "$1" >&2
  exit 2
}

if [ "$#" -ne 2 ]; then
  fail "usage: app-token.sh <role> <owner>/<repo>"
fi
# A shared App means this name may not select a credential, but keeping it
# required makes every harness call name its caller. It is a legibility
# control, not an access control.
role="$1"
case "$role" in
  ''|[!a-z]*|*[!a-z0-9_-]*)
    fail "invalid role: must match [a-z][a-z0-9_-]*"
    ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=mandate-lib.sh
source "$SCRIPT_DIR/mandate-lib.sh"

repository="$2"
case "$repository" in
  */*) ;;
  *) fail "usage: app-token.sh <role> <owner>/<repo>" ;;
esac
owner="${repository%%/*}"
repo="${repository#*/}"
case "$owner" in
  ''|*[!A-Za-z0-9_.-]*) fail "usage: app-token.sh <role> <owner>/<repo>" ;;
esac
case "$repo" in
  ''|*/*|*[!A-Za-z0-9_.-]*) fail "usage: app-token.sh <role> <owner>/<repo>" ;;
esac

for required_command in jq openssl curl; do
  command -v "$required_command" >/dev/null 2>&1 || \
    fail "required command is unavailable: $required_command"
done

credentials="$(mandate_load_role_credentials "$role")" || exit $?
app_id="$(printf '%s' "$credentials" | jq -er '.app_id')" || \
  fail "could not read $role app_id"
private_key_path="$(printf '%s' "$credentials" | jq -er '.private_key_path')" || \
  fail "could not read $role private_key_path"
unset credentials

case "$private_key_path" in
  '~') private_key_path="$HOME" ;;
  '~/'*) private_key_path="$HOME/${private_key_path#"~/"}" ;;
esac
if [ ! -f "$private_key_path" ] || [ ! -r "$private_key_path" ]; then
  fail "$role private key file is missing or unreadable"
fi

base64url() {
  openssl base64 -A 2>/dev/null | tr '+/' '-_' | tr -d '='
}

now="$(date +%s)" || fail "could not read the system clock"
iat=$((now - 60))
exp=$((now + 540))

header='{"alg":"RS256","typ":"JWT"}'
payload="$(
  jq -cn \
    --argjson iat "$iat" \
    --argjson exp "$exp" \
    --argjson iss "$app_id" \
    '{iat: $iat, exp: $exp, iss: $iss}'
)" || fail "could not build JWT claims"

encoded_header="$(printf '%s' "$header" | base64url)" || \
  fail "could not encode the JWT header"
encoded_payload="$(printf '%s' "$payload" | base64url)" || \
  fail "could not encode the JWT claims"
signing_input="$encoded_header.$encoded_payload"
signature="$(
  printf '%s' "$signing_input" |
    openssl dgst -sha256 -sign "$private_key_path" 2>/dev/null |
    base64url
)" || fail "JWT signing failed"
jwt="$signing_input.$signature"
unset header payload encoded_header encoded_payload signing_input signature private_key_path app_id

# Feed the Authorization header through stdin so the JWT is absent from the
# process argument list. The response remains in memory and is never echoed.
lookup_response="$(
  printf 'header = "Authorization: Bearer %s"\n' "$jwt" |
    curl --config - \
      --silent \
      --request GET \
      --header 'Accept: application/vnd.github+json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --write-out '\n%{http_code}' \
      "https://api.github.com/repos/$repository/installation" \
      2>/dev/null
)" || fail "GitHub App installation lookup failed"

lookup_status="${lookup_response##*$'\n'}"
lookup_body="${lookup_response%$'\n'*}"
unset lookup_response
if [ "$lookup_status" = "404" ]; then
  unset jwt lookup_body lookup_status
  fail "GitHub App is not installed on repository $repository"
fi
if [ "$lookup_status" != "200" ]; then
  unset jwt lookup_body lookup_status repository owner repo
  fail "GitHub App installation lookup failed"
fi
installation_id="$(
  printf '%s' "$lookup_body" |
    jq -er '.id | select(type == "number" and . > 0) | tostring' 2>/dev/null
)" || fail "GitHub App installation lookup response was invalid"
unset lookup_body lookup_status repository owner repo

response="$(
  printf 'header = "Authorization: Bearer %s"\n' "$jwt" |
    curl --config - \
      --silent \
      --fail \
      --request POST \
      --header 'Accept: application/vnd.github+json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --data '' \
      "https://api.github.com/app/installations/$installation_id/access_tokens" \
      2>/dev/null
)" || fail "GitHub installation token exchange failed"
unset jwt installation_id

token="$(
  printf '%s' "$response" |
    jq -er '.token | select(type == "string" and length > 0)' 2>/dev/null
)" || fail "GitHub installation token response did not contain a token"
unset response

printf '%s\n' "$token"
