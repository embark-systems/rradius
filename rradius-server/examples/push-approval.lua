-- A second step by push: after the password, the user approves the login on their phone.
--
-- A handler at the end of an all_accept chain, after the one that compares the password, so it
-- runs only once the password is right:
--
--   [auth]
--   policy = "all_accept"
--   [[auth.handlers]]
--   type = "sql"                       # or ldap: whatever checks the password
--   user_query = "SELECT password FROM users WHERE username = ?"
--   [[auth.handlers]]
--   type = "lua"
--   script = "push-approval.lua"
--
--   [lua.http]
--   allowed_hosts = ["push.example.com"]
--   timeout_ms = 7000                  # more than POLL_SECONDS below
--
-- The push service this script assumes (change the requests to what yours provides). Scripts
-- have no JSON functions, so it answers with status codes and headers:
--   POST https://push.example.com/v1/push
--     Authorization: Bearer <API key>, body username=<name>
--     201 with the header X-Push-Id: the push sent to the user's phone
--   GET https://push.example.com/v1/push/<id>?wait=<seconds>
--     Holds the request until the user answers or <seconds> pass, then 200 with the header
--     X-Push-Result: approved, denied or pending
--
-- Waiting takes time the NAS has to give:
--   * The NAS's RADIUS timeout must be longer than WAIT_SECONDS, or it gives up (or sends the
--     request again) while the user is still reading their phone. For pam_radius that is the
--     timeout in its server line, for a VPN gateway its RADIUS server timeout.
--   * Each login waiting here holds one of max_auth_inflight's places until it ends, so many
--     users waiting at once can leave none for other logins. Raise max_auth_inflight with the
--     number of users who may be waiting at the same time.
--   * max_execution_ms does not count the waiting, only the script's own work.

local API = 'https://push.example.com/v1/push'
local API_KEY = 'push-api-key-change-me'   -- An example value. Keep this file readable by rradius only.
local WAIT_SECONDS = 30                    -- How long the user has to answer.
local POLL_SECONDS = 5                     -- How long one request is held; below [lua.http] timeout_ms.

local user = get_attr('User-Name') or ''
if user == '' then
  return 'reject'
end

local sent, problem = http_request{
  url = API,
  method = 'POST',
  headers = { Authorization = 'Bearer ' .. API_KEY, ['Content-Type'] = 'application/x-www-form-urlencoded' },
  body = 'username=' .. url_encode(user),
}
if not sent then
  log('push service gave no answer:', problem)
  return 'reject'
end
local id = sent.headers['x-push-id']
if sent.status ~= 201 or not id or not id:match('^[%w_-]+$') then
  log('push service refused a push for', user, 'with', sent.status)
  return 'reject'
end

-- Asks until the user answers or the time is up. A service that answers pending at once would
-- otherwise be asked as fast as it answers, so the number of questions is limited too.
local deadline = now_unix() + WAIT_SECONDS
for _ = 1, WAIT_SECONDS do
  local left = math.floor(deadline - now_unix())
  if left <= 0 then
    break
  end
  local answer, failure = http_request{
    url = API .. '/' .. url_encode(id) .. '?wait=' .. math.min(left, POLL_SECONDS),
    headers = { Authorization = 'Bearer ' .. API_KEY },
  }
  if not answer then
    log('push service gave no answer:', failure)
    return 'reject'
  end
  local result = answer.status == 200 and answer.headers['x-push-result'] or nil
  if result == 'approved' then
    return 'accept'
  end
  if result ~= 'pending' then
    log('push for', user, 'not approved:', result or answer.status)
    return 'reject'
  end
end
log('push for', user, 'not answered in time')
return 'reject'
