#!/usr/bin/env bash
# Mint a short-lived, explicitly scoped GitHub App installation token.

# A caller may have tracing enabled. Disable it before credentials enter shell
# variables or command input, and never enable it again in this process.
set +x
set -euo pipefail

fail() {
  printf 'app-token: %s\n' "$1" >&2
  exit 2
}

usage='usage: app-token.sh <role> <owner>/<repo> --repositories <repo[,repo...]> --permissions <permission:level[,permission:level...]>'

if [ "$#" -lt 2 ]; then
  fail "$usage"
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
  *) fail "$usage" ;;
esac
owner="${repository%%/*}"
repo="${repository#*/}"
case "$owner" in
  ''|*[!A-Za-z0-9_.-]*) fail "$usage" ;;
esac
case "$repo" in
  ''|*/*|*[!A-Za-z0-9_.-]*) fail "$usage" ;;
esac
shift 2

repositories_csv=""
permissions_csv=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repositories)
      [ -z "$repositories_csv" ] || fail "caller scope is invalid: --repositories was supplied more than once"
      [ "$#" -ge 2 ] || fail "$usage"
      repositories_csv="$2"
      shift 2
      ;;
    --permissions)
      [ -z "$permissions_csv" ] || fail "caller scope is invalid: --permissions was supplied more than once"
      [ "$#" -ge 2 ] || fail "$usage"
      permissions_csv="$2"
      shift 2
      ;;
    *) fail "$usage" ;;
  esac
done

# There is deliberately no default. Omitting either half of the scope must
# fail before credentials are loaded or a GitHub endpoint is contacted.
[ -n "$repositories_csv" ] || \
  fail "unscoped token request rejected: caller must supply --repositories"
[ -n "$permissions_csv" ] || \
  fail "unscoped token request rejected: caller must supply --permissions"

for required_command in jq openssl curl; do
  command -v "$required_command" >/dev/null 2>&1 || \
    fail "required command is unavailable: $required_command"
done

# GitHub's exchange body takes repository *names*, not owner/repo pointers.
# Accept either spelling from callers, require every entry to belong to the
# installation owner selected by the lookup repository, and require the
# lookup repository itself to be in the requested repository set.
IFS=',' read -r -a requested_repository_entries <<<"$repositories_csv"
requested_repository_names=()
lookup_repository_in_scope=0
for requested_repository in "${requested_repository_entries[@]}"; do
  case "$requested_repository" in
    */*)
      requested_owner="${requested_repository%%/*}"
      requested_name="${requested_repository#*/}"
      [ "$requested_owner" = "$owner" ] || \
        fail "caller scope is invalid: repository $requested_repository is outside installation owner $owner"
      ;;
    *)
      requested_name="$requested_repository"
      ;;
  esac
  case "$requested_name" in
    ''|*/*|*[!A-Za-z0-9_.-]*)
      fail "caller scope is invalid: invalid repository entry '$requested_repository'"
      ;;
  esac
  if [ "$requested_name" = "$repo" ]; then
    lookup_repository_in_scope=1
  fi
  requested_repository_names+=("$requested_name")
done
[ "${#requested_repository_names[@]}" -gt 0 ] || \
  fail "unscoped token request rejected: caller must name at least one repository"
[ "$lookup_repository_in_scope" -eq 1 ] || \
  fail "caller scope is invalid: lookup repository $repository is absent from --repositories"

repositories_json="$({
  printf '%s\n' "${requested_repository_names[@]}" |
    jq -Rsc 'split("\n") | map(select(length > 0)) | unique'
})" || fail "could not encode caller repository scope"
if [ "$(jq 'length' <<<"$repositories_json")" -ne "${#requested_repository_names[@]}" ]; then
  fail "caller scope is invalid: --repositories contains a duplicate"
fi

IFS=',' read -r -a requested_permission_entries <<<"$permissions_csv"
permissions_json='{}'
for requested_permission in "${requested_permission_entries[@]}"; do
  case "$requested_permission" in
    *:*)
      permission_name="${requested_permission%%:*}"
      permission_level="${requested_permission#*:}"
      ;;
    *)
      fail "caller scope is invalid: permission '$requested_permission' must use permission:level"
      ;;
  esac
  case "$permission_name" in
    ''|*[!a-z0-9_]*)
      fail "caller scope is invalid: invalid permission name '$permission_name'"
      ;;
  esac
  case "$permission_level" in
    read|write) ;;
    *) fail "caller scope is invalid: permission $permission_name must request read or write" ;;
  esac
  if jq -e --arg permission "$permission_name" 'has($permission)' \
    >/dev/null <<<"$permissions_json"; then
    fail "caller scope is invalid: permission $permission_name was supplied more than once"
  fi
  permissions_json="$(
    jq -cn --argjson permissions "$permissions_json" \
      --arg permission "$permission_name" --arg level "$permission_level" \
      '$permissions + {($permission): $level}'
  )" || fail "could not encode caller permission scope"
done
[ "$(jq 'length' <<<"$permissions_json")" -gt 0 ] || \
  fail "unscoped token request rejected: caller must name at least one permission"

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
)" || fail "GitHub App installation lookup network failure"

lookup_status="${lookup_response##*$'\n'}"
lookup_body="${lookup_response%$'\n'*}"
unset lookup_response
if [ "$lookup_status" = "404" ]; then
  unset jwt lookup_body lookup_status
  fail "GitHub App is not installed on repository $repository"
fi
if [ "$lookup_status" != "200" ]; then
  unset jwt lookup_body lookup_status
  fail "GitHub App installation lookup failed with HTTP $lookup_status"
fi
installation_id="$(
  printf '%s' "$lookup_body" |
    jq -er '.id | select(type == "number" and . > 0) | tostring' 2>/dev/null
)" || fail "GitHub App installation lookup response was invalid"
installation_permissions="$(
  printf '%s' "$lookup_body" |
    jq -ec '.permissions | select(type == "object")' 2>/dev/null
)" || fail "GitHub App installation lookup response did not describe its permissions"
unset lookup_body lookup_status

# Refuse a request that is known to exceed the installation before attempting
# the exchange. This has a deliberately distinct prefix from malformed caller
# scope and network/HTTP failures, so App configuration is diagnosable.
permission_refusals="$(
  jq -cnr \
    --argjson requested "$permissions_json" \
    --argjson granted "$installation_permissions" '
      [
        $requested | to_entries[]
        | . as $request
        | ($granted[$request.key] // "none") as $held
        | select(
            $held == "none"
            or ($request.value == "write" and $held != "write")
          )
        | ($request.key + ":" + $request.value + " (installation grants " + $held + ")")
      ]
      | join(", ")
    '
)" || fail "could not compare requested permissions with the installation"
if [ -n "$permission_refusals" ]; then
  unset jwt installation_id installation_permissions
  fail "scope refused: GitHub App installation lacks requested permission(s): $permission_refusals"
fi
unset installation_permissions permission_refusals

exchange_body="$(
  jq -cn \
    --argjson repositories "$repositories_json" \
    --argjson permissions "$permissions_json" \
    '{repositories: $repositories, permissions: $permissions}'
)" || fail "could not build scoped token exchange request"

exchange_response="$(
  printf 'header = "Authorization: Bearer %s"\n' "$jwt" |
    curl --config - \
      --silent \
      --request POST \
      --header 'Accept: application/vnd.github+json' \
      --header 'X-GitHub-Api-Version: 2022-11-28' \
      --header 'Content-Type: application/json' \
      --data "$exchange_body" \
      --write-out '\n%{http_code}' \
      "https://api.github.com/app/installations/$installation_id/access_tokens" \
      2>/dev/null
)" || fail "GitHub installation token exchange network failure"
unset jwt installation_id exchange_body

exchange_status="${exchange_response##*$'\n'}"
response="${exchange_response%$'\n'*}"
unset exchange_response
if [ "$exchange_status" != "201" ]; then
  exchange_message="$(jq -r '.message // empty' <<<"$response" 2>/dev/null || true)"
  case "$exchange_status:$exchange_message" in
    403:*[Pp]ermission*|422:*[Pp]ermission*)
      fail "scope refused: GitHub App installation rejected the requested permissions"
      ;;
    *) fail "GitHub installation token exchange failed with HTTP $exchange_status" ;;
  esac
fi
unset exchange_status exchange_message

token="$(
  printf '%s' "$response" |
    jq -er '.token | select(type == "string" and length > 0)' 2>/dev/null
)" || fail "GitHub installation token response did not contain a token"
granted_permissions="$(
  printf '%s' "$response" |
    jq -ec '.permissions | select(type == "object")' 2>/dev/null
)" || fail "GitHub installation token response did not contain its permission scope"
repository_selection="$(
  printf '%s' "$response" |
    jq -er '.repository_selection | select(. == "selected")' 2>/dev/null
)" || fail "GitHub installation token response did not confirm its selected repository scope"
unset response

granted_repositories="$(
  jq -cn --arg owner "$owner" --argjson names "$repositories_json" \
    '$names | map($owner + "/" + .) | sort'
)"
if ! jq -en --argjson requested "$permissions_json" \
  --argjson permissions "$granted_permissions" \
  '$permissions == $requested' \
  >/dev/null; then
  unset token
  fail "GitHub installation token response granted permissions different from the caller request (requested=$(jq -c . <<<"$permissions_json"); returned=$(jq -c . <<<"$granted_permissions"))"
fi

# A successful exchange with repository_selection=selected grants the explicit
# repository names from the request; GitHub returns the granted permission map
# directly. Keep that effective scope in fact. Never record the token, JWT,
# App/installation identifiers, or key material.
scope_fact="$(
  jq -cn --arg role "$role" \
    --argjson repositories "$granted_repositories" \
    --argjson permissions "$granted_permissions" \
    '{role: $role, repositories: $repositories, permissions: $permissions}'
)" || {
  unset token
  fail "could not encode the granted scope for tracing"
}
if ! OSTROM_HOME="$MANDATE_DATA_DIR" ostrom trace append installation-token-minted \
  "$scope_fact" '{}' >/dev/null; then
  unset token
  fail "minted scoped token but could not record its granted scope on the trace"
fi
unset scope_fact granted_repositories granted_permissions repository_selection \
  repositories_json permissions_json requested_repository_entries \
  requested_repository_names requested_permission_entries repository owner repo role

printf '%s\n' "$token"
