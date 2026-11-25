-- Verify the user name and password with the member system's API.
--
-- The API this script assumes (replace it with what your system really provides):
--   POST https://members.example.com/v1/radius/login
--   Authorization: Bearer <API key>
--   Content-Type: application/x-www-form-urlencoded
--   body: username=<user>&password=<password>
--   200: user name and password are correct and access is allowed; the response header X-Plan is the plan name (optional)
--   401, 403: wrong user name or password, or access is not allowed
-- The script has no JSON functions, so have the API answer with status codes and headers.
local API = 'https://members.example.com/v1/radius/login'
local API_KEY = 'member-api-key-change-me'   -- demo value. Only the account that runs rradius should be able to read this file

local user = get_attr('User-Name') or ''
local password = get_attr('User-Password') or ''
-- Only PAP gives the plaintext password; CHAP and MS-CHAPv2 cannot be verified here. An empty name or password need not be sent to the API.
if user == '' or password == '' then
  return 'reject'
end

local response, problem = http_request{
  url = API,
  method = 'POST',
  headers = {
    Authorization = 'Bearer ' .. API_KEY,
    ['Content-Type'] = 'application/x-www-form-urlencoded',
  },
  -- The name and password come from the user, so always encode them first.
  body = 'username=' .. url_encode(user) .. '&password=' .. url_encode(password),
}

-- The API did not answer (timeout, unreachable, too busy): reject, and leave one log line. Do not log the password.
if not response then
  log('member API gave no answer:', problem)
  return 'reject'
end
if response.status ~= 200 then
  if response.status ~= 401 and response.status ~= 403 then
    log('member API answered', response.status)
  end
  return 'reject'
end

-- Give the plan name to the NAS as Filter-Id, and the rule of the same name on the NAS sets the rate. Accept only values that look like names.
local plan = response.headers['x-plan']
if plan and plan:match('^[%w_-]+$') then
  set_attr('Filter-Id', plan)
end
set_attr('Session-Timeout', 14400)
return 'accept'
