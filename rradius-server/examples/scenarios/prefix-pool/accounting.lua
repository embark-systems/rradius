-- Accounting: confirm, extend and release prefixes. Every write is conditional, so processing the same packet twice gives the same result as once.
--
--   Accounting-Start, Interim-Update: confirm the user's prefix as active, with the expiry set to 3 Interim intervals from now
--   Accounting-Stop:                  release the prefix: the row becomes released and is kept for this user for HOLD_SECONDS
--   Accounting-On/Off:                the NAS rebooted, so all active prefixes of this NAS become released
--
-- A released prefix: the user went offline, the prefix is kept for them for now and they get the same one first when they log in again; other users take the oldest
-- released one only when the pool has no free n. It is deleted after HOLD_SECONDS, and deletion is what makes it free.
--
-- The prefix row is found by the text of Delegated-IPv6-Prefix: that is the text the server decodes from the attribute the NAS sent
-- (compressed, lower case), and the prefix column that allocate.lua stores uses the same form, so equal strings mean the same prefix.
-- When the NAS accounting packet carries no prefix, fall back to the user name alone: a user has only one prefix in one pool.
--
-- The accounting script must return 'accept' for the NAS to get a reply; for any other result the NAS retransmits.
-- INTERIM_SECONDS and HOLD_SECONDS below must match allocate.lua; REAP_SECONDS is the reclaim interval (the 60 in allocate.lua).

local INTERIM_SECONDS = 300
local HOLD_SECONDS = 86400    -- how long a released prefix is kept for its previous user
local REAP_SECONDS = 60

-- With [auth] configured, authentication never reaches this script. If the configuration changes, still do not let this script accept anyone.
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- Request-triggered reclaim, same as allocate.lua.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-prefix-pool' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-prefix-pool' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  --   An active row that expired without an Interim: becomes released and is kept for the user for HOLD_SECONDS; session_id is kept, because the NAS may still be online
  --   and only the Interim was missed, so a later Interim can bring it back to active (Stop and On/Off clear session_id, so they cannot). An expired reserved or released row: deleting it makes it free.
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

-- Which NAS: one client can be a whole network range, so also tell them apart by the address or name the NAS reports.
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')

-- NAS rebooted: the prefixes it reported in use earlier have all ended.
if status == 7 or status == 8 then
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now + HOLD_SECONDS, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start, Stop, Interim-Update
  return 'accept'
end

local user = get_attr('User-Name')
local session = get_attr('Acct-Session-Id')
if not user or user == '' or not session or session == '' then
  return 'accept' -- no user to match; a retransmission would not help
end

-- The user's pool: a prefix is looked up only in the user's pool, and different pools can have the same prefix text.
local pool = sql_query("SELECT pool FROM subscribers WHERE username = ?", {user})
if #pool == 0 then
  return 'accept'
end
local pool_name = pool[1].pool

-- If the packet carries a prefix it must match; if not, only the user counts: when prefix is an empty string, the condition below (? = '' OR prefix = ?) is always true.
-- (An SQL parameter list cannot hold nil, so an empty string is used.)
local prefix = get_attr('Delegated-IPv6-Prefix') or ''

if status == 2 then
  --   Stop: release only this session's row (it becomes released and is kept for the user for HOLD_SECONDS). A Stop from an old session that arrives late cannot touch
  --   a newer binding, nor a reservation that is not yet confirmed (reclaim handles that, and an expired reservation is deleted).
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?) AND session_id = ? AND state = 'active']],
    {now + HOLD_SECONDS, now, pool_name, user, prefix, prefix, session})
  return 'accept'
end

-- Start, Interim-Update: confirm as active, with the expiry 3 Interim intervals from now.
-- An Interim-Update only updates rows with no session or the same session; of the released rows, only one that expired because no Interim arrived (still holding
-- the same session_id) can be turned back to active by it, and rows released by Stop or On/Off (session_id already cleared) do not count; a Start can: on a quick reconnect the old session's Stop releases first, then the new session's Start arrives, and the prefix is still theirs.
local confirmed = sql_execute([[UPDATE prefix_leases SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?)
                                  AND (? = 1 OR (state <> 'released' AND (session_id IS NULL OR session_id = ?)) OR (state = 'released' AND session_id = ?))]],
  {session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, user, prefix, prefix, status == 1 and 1 or 0, session, session})
if confirmed == 0 then
  --   The reservation already expired and was deleted (the NAS packet arrived late), or the prefix the NAS reported is not this user's: it is not restored,
  --   because n cannot be derived from the text; log one line. The prefix may already have been given to someone else, so check the NAS records.
  log('prefix pool', pool_name, 'prefix', (prefix ~= '' and prefix or '(none)'), 'reported by', nas, 'for', user, 'is not in prefix_leases')
end
return 'accept'
