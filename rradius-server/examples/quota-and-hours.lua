-- Limits users by total session time and by hours of the day (UTC).
--
--   CREATE TABLE quota_users (username TEXT PRIMARY KEY,
--                             quota_seconds INTEGER NOT NULL,
--                             from_hour INTEGER NOT NULL DEFAULT 0,  -- 0 to 23
--                             to_hour INTEGER NOT NULL DEFAULT 24);  -- up to, not including
--   CREATE TABLE quota_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL,
--                                username TEXT NOT NULL, seconds INTEGER NOT NULL,
--                                PRIMARY KEY (client, session_id));
--   CREATE INDEX quota_sessions_user ON quota_sessions (username);
--
-- This checks policy only, not passwords. Use it as [lua].script, which accounting always
-- goes to, and also as a `lua` handler after a `sql` or `ldap` handler in a chain with
-- policy = "all_accept", so that a login has to pass both.
--
-- Time counts when a session stops. Until then the Session-Timeout below limits a session to
-- what was left when it logged in, but sessions at the same time each get that much: two
-- logins with 100 seconds left can use 200. For a hard limit, use this together with
-- simultaneous-use.lua, with a limit of 1.
--
-- quota_sessions is the usage, so deleting a user's rows starts them over, for example each
-- month. Until then a NAS that restarts and uses a session ID again, for the same client, looks
-- like the old Stop sent again and is not counted.

-- A NAS can send an attribute with nothing in it, which is as good as none.
local function given(value)
  return value and value ~= ''
end

local user = get_attr('User-Name')

if phase == 'accounting' then
  local session = get_attr('Acct-Session-Id')
  if get_attr('Acct-Status-Type') == 2 then -- Stop
    if given(user) and given(session) then
      -- A NAS can resend a Stop as a new packet, which must not count twice. One write, so that
      -- a failure leaves nothing done, and the NAS sending it again counts it. Session IDs are
      -- only unique within a NAS.
      sql_execute("INSERT OR IGNORE INTO quota_sessions VALUES (?, ?, ?, ?)",
        {client, session, user, math.floor(get_attr('Acct-Session-Time') or 0)})
    else
      log('time not counted, no user or session ID:', user, session)
    end
  end
  return 'accept'
end

if phase ~= 'authentication' then
  return 'accept'
end

if not given(user) then
  return 'reject'
end
local account = sql_query("SELECT quota_seconds, from_hour, to_hour," ..
  " (SELECT COALESCE(SUM(seconds), 0) FROM quota_sessions WHERE username = ?) AS used_seconds" ..
  " FROM quota_users WHERE username = ?", {user, user})[1]
if not account then
  return 'reject'
end

local hour = math.floor(now_unix() / 3600) % 24
local within_hours
if account.from_hour <= account.to_hour then
  within_hours = hour >= account.from_hour and hour < account.to_hour
else -- a range across midnight, such as 22 to 6
  within_hours = hour >= account.from_hour or hour < account.to_hour
end
if not within_hours then
  return 'reject'
end

local left = account.quota_seconds - account.used_seconds
if left <= 0 then
  return 'reject'
end
set_attr('Session-Timeout', left)
return 'accept'
