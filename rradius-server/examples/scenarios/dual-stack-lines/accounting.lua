-- Accounting: confirm, extend and release IPv4 addresses and IPv6 prefixes. Every write is conditional, so handling the same packet twice gives the same result as handling it once.
-- This merges the two accounting scripts of manual chapter 14 ("IPv4 address pool" and "IPv6 prefix pool") into one:
--
--   Accounting-Start, Interim-Update: confirm the user's address and prefix as active, and set the expiry to 3 Interim intervals from now
--   Accounting-Stop:                  release the address (back to free, recording the previous user) and the prefix (set to released, kept for that user for HOLD_SECONDS seconds)
--   Accounting-On/Off:                the NAS rebooted; put all of this NAS's active addresses and prefixes back
--
-- One packet can carry both Framed-IP-Address and Delegated-IPv6-Prefix: do the part for whichever is present. A line with a fixed address has no row in the pool,
-- so the matching UPDATE touches no rows; these packets are still handled and cause no error.
--
-- Which line this is: the Class that line.lua returns to the BNG is lines.subscriber, and the BNG sends Class back in accounting packets. IPoE accounting packets have no reliable
-- User-Name, so it is not used. When a packet has no Class (the BNG did not send it back), reply to the NAS without processing it and log one line.
--
-- The accounting script must return 'accept' for the NAS to get a reply; otherwise the NAS retransmits.
-- The numbers below must match line.lua.

local INTERIM_SECONDS = 300
local HOLD_SECONDS = 86400    -- -- how long a released prefix is kept for its previous user
local REAP_SECONDS = 60

-- When [auth] is present, authentication never reaches here. In case the configuration changes, do not let this script accept anyone.
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- Request-triggered reaping, same as line.lua.
local function due(name)
  local rows = sql_query('SELECT 1 FROM housekeeping WHERE name = ? AND last_run < ?', {name, now - REAP_SECONDS})
  return #rows > 0 and sql_execute('UPDATE housekeeping SET last_run = ? WHERE name = ? AND last_run < ?', {now, name, now - REAP_SECONDS}) == 1
end
if due('reap-ip-pool') then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end
if due('reap-prefix-pool') then
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

-- Which NAS: one client can be a whole subnet, so tell NASes apart further by the address or name the NAS reports.
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')

-- NAS reboot: the addresses and prefixes it reported in use earlier have all ended.
if status == 7 or status == 8 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now, now, nas})
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now + HOLD_SECONDS, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- -- Start, Stop, Interim-Update
  return 'accept'
end

local user = get_attr('Class')
local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  return 'accept' -- -- a packet with no session to match; retransmitting would not help
end
if not user or user == '' then
  log('dual stack accounting: no Class in the packet of session', session, 'from', nas, ': the BNG does not echo it, nothing to confirm')
  return 'accept'
end

-- The two pools of this line: addresses and prefixes are looked up only in its own pools, and different pools can hold the same address.
local plan = sql_query([[SELECT p.pool, p.pool6, l.ipv4 IS NOT NULL AS fixed4, (l.wan_v6 IS NOT NULL OR l.pd_v6 IS NOT NULL) AS fixed6
                         FROM lines l JOIN plans p ON p.plan = l.plan WHERE l.subscriber = ?]], {user})
if #plan == 0 then
  return 'accept'
end
local pool4, pool6 = plan[1].pool, plan[1].pool6
-- A fixed line has no row in the pool: the matching UPDATE touches no rows, which is not an error and is not logged.
local fixed4, fixed6 = plan[1].fixed4 == 1, plan[1].fixed6 == 1

local ip = get_attr('Framed-IP-Address') or ''
-- If the packet carries a prefix it must match; if not, only the user is checked: when prefix is an empty string, the condition (? = '' OR prefix = ?) is always true.
-- (An SQL parameter list cannot hold nil, so an empty string is used.)
local prefix = get_attr('Delegated-IPv6-Prefix') or ''

if status == 2 then
  -- Stop: release only this session's binding, or a reservation that was never confirmed. A late Stop from an old session cannot touch the new binding.
  if ip ~= '' then
    sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                    expires = 0, released = ?, updated = ?
                  WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')
                    AND (session_id = ? OR session_id IS NULL)]], {now, now, pool4, ip, user, session})
  end
  if not fixed6 then
    -- If the packet carries a prefix it must match; if not, only the user is checked (a user has only one prefix in a pool).
    sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                  WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?) AND session_id = ? AND state = 'active']],
      {now + HOLD_SECONDS, now, pool6, user, prefix, prefix, session})
  end
  return 'accept'
end

-- Start, Interim-Update: confirm as active, with the expiry 3 Interim intervals from now.
if ip ~= '' and not fixed4 then
  local confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                  WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')]],
    {session, nas, now + 3 * INTERIM_SECONDS, now, pool4, ip, user})
  if confirmed == 0 then
    -- The binding has already expired and been put back (the NAS packet arrived late): if this address is still free, take it back.
    confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', username = ?, session_id = ?, nas = ?, expires = ?, updated = ?
                              WHERE pool = ? AND ip = ? AND state = 'free']],
      {user, session, nas, now + 3 * INTERIM_SECONDS, now, pool4, ip})
  end
  if confirmed == 0 then
    log('ip pool', pool4, 'address', ip, 'reported by', nas, 'for', user, 'is held by someone else')
  end
end

if not fixed6 then
  -- If the packet carries a prefix it must match; if not, only the user is checked: when prefix is an empty string, the condition (? = '' OR prefix = ?) is always true.
  -- Interim-Update updates only rows with no session or the same session; of the released rows, only those that "expired for lack of Interim" (still carrying
  -- the same session_id) can be turned back to active by it; those released by Stop or On/Off (session_id already cleared) do not count; Start can.
  local confirmed = sql_execute([[UPDATE prefix_leases SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                  WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?)
                                    AND (? = 1 OR (state <> 'released' AND (session_id IS NULL OR session_id = ?)) OR (state = 'released' AND session_id = ?))]],
    {session, nas, now + 3 * INTERIM_SECONDS, now, pool6, user, prefix, prefix, status == 1 and 1 or 0, session, session})
  if confirmed == 0 then
    -- The reservation has already expired and been deleted (the NAS packet arrived late), or the prefix the NAS reported is not this user's: it is not restored,
    -- because the text cannot be turned back into n; one line is logged. The prefix may already have been given to someone else, so check the NAS's records.
    log('prefix pool', pool6, 'prefix', (prefix ~= '' and prefix or '(none)'), 'reported by', nas, 'for', user, 'is not in prefix_leases')
  end
end
return 'accept'
