-- Limits how many sessions a user can have at once, across the whole cluster.
--
--   CREATE TABLE active_sessions (client TEXT NOT NULL, session_id TEXT NOT NULL,
--                                 username TEXT NOT NULL, last_seen INTEGER NOT NULL,
--                                 nas TEXT NOT NULL, PRIMARY KEY (client, session_id));
--   CREATE INDEX active_sessions_user ON active_sessions (username);
--
-- This checks policy only, not passwords. Use it as [lua].script, which accounting always
-- goes to, and also as a `lua` handler after a `sql` or `ldap` handler in a chain with
-- policy = "all_accept", so that a login has to pass both.
--
-- The limit is not exact. A session counts from its Accounting-Start, so two logins at the
-- same moment can both pass, and a node's copy of the database can lag slightly.
-- The NAS must send Interim-Updates more often than STALE_AFTER, or a session
-- still running stops counting. Older rows can be deleted at any time.
--
-- The CoA relay's session_query can read the same table, to end every session of a user:
--   SELECT nas, session_id FROM active_sessions
--   WHERE username = ? AND last_seen > strftime('%s', 'now') - 900 ORDER BY last_seen DESC
-- `nas` is where to send it: the NAS-IP-Address or NAS-IPv6-Address the NAS reports, or the
-- client name when it reports neither. The relay sends only to a configured client, so an
-- address has to be one a client covers. A NAS behind NAT reports an address the relay cannot
-- reach; for those, select `client` instead, which works when the client has a single address.
-- A client with only tls_name has no address to send to.
--
-- Accounting can arrive twice or late, when it comes back from the accounting spool. Writing the
-- same row again changes nothing, and Acct-Delay-Time keeps an old report from looking recent.

local MAX_SESSIONS = 2
-- A NAS that crashed never sends a Stop, so a session it does not report on stops counting.
local STALE_AFTER = 900

-- A NAS can send an attribute with nothing in it, which is as good as none.
local function given(value)
  return value and value ~= ''
end

local user = get_attr('User-Name')
local now = math.floor(now_unix())

if phase == 'accounting' then
  local session = get_attr('Acct-Session-Id')
  local status = get_attr('Acct-Status-Type')
  if status == 1 or status == 2 or status == 3 then -- Start, Stop, Interim-Update
    -- Session IDs are only unique within a NAS.
    if not (given(user) and given(session)) then
      log('session not counted, no user or session ID:', user, session)
    elseif status == 2 then
      sql_execute("DELETE FROM active_sessions WHERE client = ? AND session_id = ?", {client, session})
    else
      local seen = now - math.floor(get_attr('Acct-Delay-Time') or 0)
      local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address')
      if not given(nas) then
        nas = client
      end
      sql_execute("INSERT OR REPLACE INTO active_sessions (client, session_id, username, last_seen, nas) VALUES (?, ?, ?, ?, ?)",
        {client, session, user, seen, nas})
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
local rows = sql_query("SELECT count(*) AS n FROM active_sessions WHERE username = ? AND last_seen > ?",
  {user, now - STALE_AFTER})
if rows[1].n >= MAX_SESSIONS then
  return 'reject'
end
return 'accept'
