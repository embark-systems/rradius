-- Accounting: confirm, extend and release addresses. Every write is conditional, so processing the same packet twice gives the same result as once.
--
--   Accounting-Start, Interim-Update: confirm the user's address as active, with the expiry set to
--                                     3 Interim intervals from now
--   Accounting-Stop:                  release the address and record the previous user and the time (for stickiness)
--   Accounting-On/Off:                the NAS rebooted, so all active addresses of this NAS are put back
--
-- The accounting script must return 'accept' for the NAS to get a reply; for any other result the NAS retransmits.
-- The numbers below must match allocate.lua.

local INTERIM_SECONDS = 300
local REAP_SECONDS = 60

-- With [auth] configured, authentication never reaches this script. If the configuration changes, still do not let this script accept anyone.
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- Request-triggered reclaim, same as allocate.lua.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-ip-pool' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-ip-pool' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end

-- Which NAS: one client can be a whole network range, so also tell them apart by the address or name the NAS reports.
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')

-- NAS rebooted: the addresses it reported in use earlier have all ended.
if status == 7 or status == 8 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start, Stop, Interim-Update
  return 'accept'
end

local ip = get_attr('Framed-IP-Address')
local user = get_attr('User-Name')
local session = get_attr('Acct-Session-Id')
if not ip or ip == '' or not user or user == '' or not session or session == '' then
  return 'accept' -- no address to match (for example a session that is not from this pool); a retransmission would not help
end

-- The user's pool: an address is looked up only in the user's pool, and different pools can have the same address.
local pool = sql_query("SELECT pool FROM subscribers WHERE username = ?", {user})
if #pool == 0 then
  return 'accept'
end
local pool_name = pool[1].pool

if status == 2 then
  --   Stop: release only this session's address, or a reservation that was never confirmed. A Stop from an old session that arrives late cannot touch a newer binding.
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')
                  AND (session_id = ? OR session_id IS NULL)]], {now, now, pool_name, ip, user, session})
  return 'accept'
end

-- Start, Interim-Update: confirm as active, with the expiry 3 Interim intervals from now.
local confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')]],
  {session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, ip, user})
if confirmed == 0 then
  --   The binding already expired and was put back (the NAS packet arrived late): if the address is still free, take it back.
  confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', username = ?, session_id = ?, nas = ?, expires = ?, updated = ?
                            WHERE pool = ? AND ip = ? AND state = 'free']],
    {user, session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, ip})
end
if confirmed == 0 then
  log('ip pool', pool_name, 'address', ip, 'reported by', nas, 'for', user, 'is held by someone else')
end
return 'accept'
