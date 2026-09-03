#!/bin/sh
# guest-session-e2e.sh -- autonomy/t:227 Dispatch C, criterion 2
# (guest-https-floor).
#
# Verifies the full guest path end to end, from a CLEAN environment with
# nothing installed but a POSIX shell and curl:
#
#   guest API key
#     -> POST /auth/login/key            (key -> session)
#     -> GET  /whoami   through the edge  (board READ: which identity
#                                          does the board see for this
#                                          session?)
#     -> POST /feed     through the edge  (board WRITE, attributed via
#                                          whatever X-Auth-User/-Actor
#                                          Caddy injected for this
#                                          session -- never a client-
#                                          supplied identity)
#     -> GET  /feed?since=m:<hash>        (read the write back and
#                                          confirm its author matches
#                                          what /whoami reported)
#
# This is the rhea "no installed software" guest floor (Auth Service --
# Design Exploration (2026-08-27) design note, S3): rhea's corporate MDM
# firewall rules out a durable browser-login path, so the guest floor is
# this curl script, not a UI page.
#
# Usage:
#   GUEST_API_KEY=<key> ./guest-session-e2e.sh
#   GUEST_API_KEY=<key> ./guest-session-e2e.sh --dry-run
#
# The guest key comes ONLY from the GUEST_API_KEY environment variable --
# never a command-line argument (visible in `ps`/shell history) and never
# a file (a one-shot verification script has no business persisting a
# secret to disk). Exits non-zero on any failed assertion. Never prints
# the key or the session token -- only their lengths and a cksum(1)
# fingerprint (POSIX, present on both BSD/macOS and GNU userlands, unlike
# md5/md5sum which differ by platform), enough to catch an obviously
# truncated/mismatched copy-paste without ever putting the secret itself
# in a log.
#
# ---------------------------------------------------------------------------
# RECONCILE WITH DISPATCH A (crates/auth-service) BEFORE THE FIRST REAL RUN.
#
# This worker (Dispatch C) built this script from the plan's prose
# description of Dispatch A2 (`POST /login/key`), not from Dispatch A's
# landed code -- at the time this script was written, Dispatch A had not
# yet shipped a `#[derive(Deserialize)]`/`#[derive(Serialize)]` struct for
# the route to check against. Grep Dispatch A's diff for the actual
# request/response structs behind `POST /login/key` and correct the three
# constants below (and the passkey-cookie-name assumption noted at their
# use site) if they differ:
#
#   LOGIN_KEY_FIELD      the JSON field name the /login/key request body
#                         expects the API key under.
#                         Guessed: "key" (mirrors config.json's existing
#                         {key, name, active} shape, per plan P1/A1).
#   LOGIN_TOKEN_FIELD     the JSON field name the /login/key response body
#                         carries the session token under -- the "agents
#                         get the token in the JSON body" half of the
#                         plan (Dispatch A2).
#                         Guessed: "token".
#   SESSION_COOKIE_NAME   the cookie name the session is presented under
#                         on every subsequent request (this script builds
#                         the Cookie header by hand rather than relying on
#                         curl's cookie-jar following a Set-Cookie, since
#                         the plan states agents get the raw token value,
#                         not a browser Set-Cookie round-trip, from
#                         /login/key).
#                         Guessed: "auth_session" (already used by
#                         StoredSession / deploy/t228/verify's seed
#                         fixtures for the PASSKEY session cookie -- high
#                         confidence the key->session exchange reuses the
#                         same cookie-builder per Dispatch A's tripwire
#                         (a), but not re-verified against Dispatch A's
#                         actual landed code).
# ---------------------------------------------------------------------------
LOGIN_KEY_FIELD="key"
LOGIN_TOKEN_FIELD="session_token"
SESSION_COOKIE_NAME="auth_session"

set -u

HOST="${HOST:-umbra.computer}"
BASE_URL="https://${HOST}"
DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=1
fi

FAILURES=0
NONCE="$(date -u +%Y%m%dT%H%M%SZ)-$$"
CONTENT="guest-session-e2e verification post ${NONCE}"

fail() {
  echo "FAIL: $1" >&2
  FAILURES=$((FAILURES + 1))
}

pass() {
  echo "PASS: $1"
}

# Prints a secret's length and a cksum(1) fingerprint -- NEVER the value
# itself. Only handles flat string/number fields (no nesting, no escaped
# quotes) -- that is all this script's controlled, fixed-shape responses
# ever need.
fingerprint() {
  label="$1"
  value="$2"
  if [ -z "$value" ]; then
    echo "  ${label}: <empty>"
    return
  fi
  len=$(printf '%s' "$value" | wc -c | tr -d ' ')
  sum=$(printf '%s' "$value" | cksum | awk '{print $1 "/" $2}')
  echo "  ${label}: length=${len} cksum=${sum}"
}

# Extract a top-level JSON string field's value via grep+sed -- no jq
# dependency (this script's whole point is "nothing installed"). Only
# handles a flat string field with no embedded escaped quote, which is
# all the fixed API responses below ever carry.
json_field() {
  field="$1"
  json="$2"
  printf '%s' "$json" \
    | grep -o "\"${field}\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" \
    | head -n 1 \
    | sed -E "s/.*\"${field}\"[[:space:]]*:[[:space:]]*\"([^\"]*)\"/\1/"
}

# Extract the FIRST flat JSON object ({...}, no nested braces) from a
# JSON array response. Safe here because every field FeedMessage ever
# carries is a flat string/enum/null (no nested objects), and this
# script's own posted `content` is built above without any literal
# brace, so the first "{...}" in the array text is always the first
# array element.
first_json_object() {
  printf '%s' "$1" | grep -o '{[^}]*}' | head -n 1
}

mask() {
  # Show only that a value is non-empty, never its content.
  if [ -z "$1" ]; then
    echo "<empty>"
  else
    echo "<redacted, $( printf '%s' "$1" | wc -c | tr -d ' ') bytes>"
  fi
}

if [ "$DRY_RUN" -eq 1 ]; then
  echo "=== DRY RUN: requests this script would make (secrets masked) ==="
  echo ""
  echo "1) POST ${BASE_URL}/auth/login/key"
  echo "   body: {\"${LOGIN_KEY_FIELD}\": $(mask "${GUEST_API_KEY:-}")}"
  echo ""
  echo "2) GET ${BASE_URL}/whoami"
  echo "   header: Cookie: ${SESSION_COOKIE_NAME}=<redacted session token>"
  echo ""
  echo "3) POST ${BASE_URL}/feed"
  echo "   header: Cookie: ${SESSION_COOKIE_NAME}=<redacted session token>"
  echo "   body: {\"author\": \"guest-e2e-script\", \"kind\": \"note\", \"content\": \"${CONTENT}\"}"
  echo "   (author is ignored server-side when X-Auth-User is present --"
  echo "    see routes/mod.rs's resolve_authenticated_author -- this is a"
  echo "    required-field placeholder, not the real attribution.)"
  echo ""
  echo "4) GET ${BASE_URL}/feed?since=m:<hash from step 3>"
  echo ""
  echo "No network calls made (--dry-run)."
  exit 0
fi

: "${GUEST_API_KEY:?GUEST_API_KEY must be set (never pass the key as an argument)}"

echo "=== guest-session-e2e: ${BASE_URL} ==="
fingerprint "GUEST_API_KEY" "$GUEST_API_KEY"

# --- Step 1: key -> session -----------------------------------------------
echo ""
echo "--- Step 1: POST /auth/login/key ---"
LOGIN_BODY="{\"${LOGIN_KEY_FIELD}\": \"${GUEST_API_KEY}\"}"
LOGIN_RESP=$(curl -sS -w '\n%{http_code}' -X POST "${BASE_URL}/auth/login/key" \
  -H 'Content-Type: application/json' \
  -d "$LOGIN_BODY")
LOGIN_STATUS=$(printf '%s' "$LOGIN_RESP" | tail -n 1)
LOGIN_JSON=$(printf '%s' "$LOGIN_RESP" | sed '$d')

if [ "$LOGIN_STATUS" = "200" ]; then
  pass "login returned 200"
else
  fail "login expected 200, got ${LOGIN_STATUS} (body: ${LOGIN_JSON})"
fi

SESSION_TOKEN=$(json_field "$LOGIN_TOKEN_FIELD" "$LOGIN_JSON")
if [ -n "$SESSION_TOKEN" ]; then
  pass "session token present in login response"
else
  fail "no \"${LOGIN_TOKEN_FIELD}\" field found in login response -- see the RECONCILE WITH DISPATCH A header comment, LOGIN_TOKEN_FIELD may be wrong"
fi
fingerprint "session token" "$SESSION_TOKEN"

COOKIE_HEADER="${SESSION_COOKIE_NAME}=${SESSION_TOKEN}"

if [ "$FAILURES" -gt 0 ]; then
  echo ""
  echo "=== ${FAILURES} failure(s) before login even succeeded; stopping. ==="
  exit "$FAILURES"
fi

# --- Step 2: board READ (GET /whoami) --------------------------------------
echo ""
echo "--- Step 2: GET /whoami (board read, through the edge) ---"
WHOAMI_RESP=$(curl -sS -w '\n%{http_code}' "${BASE_URL}/whoami" \
  -H "Cookie: ${COOKIE_HEADER}")
WHOAMI_STATUS=$(printf '%s' "$WHOAMI_RESP" | tail -n 1)
WHOAMI_JSON=$(printf '%s' "$WHOAMI_RESP" | sed '$d')

echo "  identity headers the board sees (GET /whoami response): ${WHOAMI_JSON}"

if [ "$WHOAMI_STATUS" = "200" ]; then
  pass "whoami returned 200"
else
  fail "whoami expected 200, got ${WHOAMI_STATUS} (body: ${WHOAMI_JSON})"
fi

WHOAMI_USER=$(json_field "user" "$WHOAMI_JSON")
if [ -n "$WHOAMI_USER" ]; then
  pass "whoami reports a non-empty identity: ${WHOAMI_USER}"
else
  fail "whoami reported an empty/absent \"user\" -- the session did not carry an identity through the edge"
fi

# --- Step 3: board WRITE (POST /feed) attributed to the guest --------------
echo ""
echo "--- Step 3: POST /feed (board write, attributed by the edge) ---"
FEED_POST_BODY="{\"author\": \"guest-e2e-script\", \"kind\": \"note\", \"content\": \"${CONTENT}\"}"
FEED_POST_RESP=$(curl -sS -w '\n%{http_code}' -X POST "${BASE_URL}/feed" \
  -H "Cookie: ${COOKIE_HEADER}" \
  -H 'Content-Type: application/json' \
  -d "$FEED_POST_BODY")
FEED_POST_STATUS=$(printf '%s' "$FEED_POST_RESP" | tail -n 1)
FEED_POST_JSON=$(printf '%s' "$FEED_POST_RESP" | sed '$d')

if [ "$FEED_POST_STATUS" = "200" ]; then
  pass "feed post returned 200"
else
  fail "feed post expected 200, got ${FEED_POST_STATUS} (body: ${FEED_POST_JSON})"
fi

POST_HASH=$(json_field "hash" "$FEED_POST_JSON")
POST_AUTHOR=$(json_field "author" "$FEED_POST_JSON")
echo "  POST /feed response: hash=${POST_HASH:-<none>} author=${POST_AUTHOR:-<none>}"

if [ -n "$POST_HASH" ]; then
  pass "feed post response carries a hash"
else
  fail "feed post response carries no hash -- cannot read the write back"
fi

if [ -n "$POST_AUTHOR" ] && [ "$POST_AUTHOR" = "$WHOAMI_USER" ]; then
  pass "feed post response author (${POST_AUTHOR}) matches whoami's identity (${WHOAMI_USER})"
else
  fail "feed post response author '${POST_AUTHOR}' does not match whoami's identity '${WHOAMI_USER}' -- the write was not attributed to this guest session"
fi

if [ "$FAILURES" -gt 0 ] || [ -z "$POST_HASH" ]; then
  echo ""
  echo "=== ${FAILURES} failure(s); skipping read-back (no usable hash). ==="
  exit "$FAILURES"
fi

# --- Step 4: read the write back and re-confirm attribution -----------------
echo ""
echo "--- Step 4: GET /feed?since=m:<hash> (read the write back) ---"
FEED_GET_RESP=$(curl -sS -w '\n%{http_code}' \
  "${BASE_URL}/feed?since=m:${POST_HASH}" \
  -H "Cookie: ${COOKIE_HEADER}")
FEED_GET_STATUS=$(printf '%s' "$FEED_GET_RESP" | tail -n 1)
FEED_GET_JSON=$(printf '%s' "$FEED_GET_RESP" | sed '$d')

if [ "$FEED_GET_STATUS" = "200" ]; then
  pass "feed read-back returned 200"
else
  fail "feed read-back expected 200, got ${FEED_GET_STATUS} (body: ${FEED_GET_JSON})"
fi

# since=m:<hash> is inclusive and ascending (manual/http-api.md), so our
# own just-posted entry is guaranteed to be the FIRST array element --
# anything posted concurrently by someone else lands after it, never
# before.
FIRST_ENTRY=$(first_json_object "$FEED_GET_JSON")
READBACK_HASH=$(json_field "hash" "$FIRST_ENTRY")
READBACK_AUTHOR=$(json_field "author" "$FIRST_ENTRY")
READBACK_CONTENT=$(json_field "content" "$FIRST_ENTRY")

echo "  read-back entry: hash=${READBACK_HASH:-<none>} author=${READBACK_AUTHOR:-<none>}"

if [ "$READBACK_HASH" = "$POST_HASH" ]; then
  pass "read-back entry hash matches the posted hash"
else
  fail "read-back entry hash '${READBACK_HASH}' does not match posted hash '${POST_HASH}'"
fi

if [ "$READBACK_CONTENT" = "$CONTENT" ]; then
  pass "read-back entry content matches what was posted"
else
  fail "read-back entry content '${READBACK_CONTENT}' does not match posted content '${CONTENT}'"
fi

if [ -n "$READBACK_AUTHOR" ] && [ "$READBACK_AUTHOR" = "$WHOAMI_USER" ]; then
  pass "read-back entry author (${READBACK_AUTHOR}) matches whoami's identity (${WHOAMI_USER}) -- guest write attribution verified end to end"
else
  fail "read-back entry author '${READBACK_AUTHOR}' does not match whoami's identity '${WHOAMI_USER}'"
fi

echo ""
echo "=== guest-session-e2e finished: ${FAILURES} failure(s) ==="
exit "$FAILURES"
