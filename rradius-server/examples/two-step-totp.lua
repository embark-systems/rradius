-- Two-step login: the password first, then a TOTP code answering an Access-Challenge.
--
--   CREATE TABLE mfa_users (username TEXT PRIMARY KEY, password TEXT, totp_secret TEXT,
--                           last_step INTEGER NOT NULL DEFAULT 0,
--                           failed_attempts INTEGER NOT NULL DEFAULT 0,
--                           locked_until INTEGER NOT NULL DEFAULT 0);
--   CREATE TABLE mfa_pending (state TEXT PRIMARY KEY, username TEXT, expires INTEGER);
--
-- A challenge can come back to another node, whose copy of the database may lag slightly.
-- This limits guesses at the code, not at the password: only someone who knows the
-- password can lock an account.

if phase ~= 'authentication' then
  return 'accept'
end

local user = get_attr('User-Name')
if not user then
  return 'reject'
end

local rows = sql_query("SELECT password, totp_secret, locked_until FROM mfa_users WHERE username = ?", {user})
local account = rows[1]
-- Without a stored password, a request without one would otherwise compare as equal.
if not account or not account.password or account.password == '' then
  return 'reject'
end

local now = math.floor(now_unix())
if account.locked_until > now then
  return 'reject'
end
local state = get_attr('State')

-- The fifth wrong code in a row locks the account for five minutes. Written through the
-- cluster, so it holds on every node.
local function record_failure()
  sql_execute("UPDATE mfa_users SET" ..
    " locked_until = CASE WHEN failed_attempts >= 4 THEN ? ELSE locked_until END," ..
    " failed_attempts = CASE WHEN failed_attempts >= 4 THEN 0 ELSE failed_attempts + 1 END" ..
    " WHERE username = ?", {now + 300, user})
end

if not state then
  if get_attr('User-Password') ~= account.password then
    return 'reject'
  end
  local token = random_token()
  sql_execute("DELETE FROM mfa_pending WHERE expires < ?", {now})
  sql_execute("INSERT INTO mfa_pending VALUES (?, ?, ?)", {token, user, now + 60})
  set_attr('State', token)
  set_attr('Reply-Message', 'Enter your code')
  return 'challenge'
end

-- Used once: whoever deletes the row owns the challenge.
local claimed = sql_execute("DELETE FROM mfa_pending WHERE state = ? AND username = ? AND expires >= ?",
  {state, user, now})
if claimed ~= 1 then
  return 'reject'
end

local step = totp_verify(account.totp_secret, get_attr('User-Password') or '')
if not step then
  record_failure()
  return 'reject'
end

-- A code works once: this only updates the row for a later step than the last one used.
local fresh = sql_execute("UPDATE mfa_users SET failed_attempts = 0, last_step = ? WHERE username = ? AND last_step < ?",
  {step, user, step})
if fresh ~= 1 then
  record_failure()
  return 'reject'
end
return 'accept'
