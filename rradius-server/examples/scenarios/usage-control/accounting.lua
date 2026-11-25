-- Accounting: record each session's traffic (written as in quick-start) and confirm, extend and release the user's session slots.
--
--   Accounting-Start:   confirm a reserved slot as active, with the expiry set to 3 Interim intervals from now
--   Interim-Update:     extend this session's slot; if none is found, treat it as a Start
--   Accounting-Stop:    release this session's slot
--   Accounting-On/Off:  the NAS rebooted and all its sessions ended, so put all its slots back
--
-- The NAS gets a reply only when the accounting script returns 'accept'; for any other result the NAS retransmits and the script runs again, so every write
-- must be safe to repeat. The numbers below must match limits.lua.

local INTERIM_SECONDS = 300
local REAP_SECONDS = 60

-- With [auth], authentication never reaches this script. If the configuration changes, still do not let this script admit anyone.
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- Recovery triggered by a request, same as in limits.lua.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-session-slots' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-session-slots' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  sql_execute([[UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE rowid IN (SELECT rowid FROM session_slots WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now})
end

-- Which NAS: one client can be a whole network range, so also tell them apart by the address or name the NAS reports.
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')
-- NAS reboot (Accounting-On/Off): all earlier sessions of this NAS have ended.
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL", {client, nas})
  sql_execute("UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ? WHERE nas = ? AND state = 'active'", {now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start, Stop, Interim-Update
  return 'accept'
end

local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  log('accounting without Acct-Session-Id from', client)
  return 'accept' -- cannot record it, but a retransmission would not do better
end
local user = get_attr('User-Name') or ''

-- For packets replayed from the accounting spool or sent late by the NAS, restore the time of the event from Acct-Delay-Time.
local seen = now - math.floor(get_attr('Acct-Delay-Time') or 0)
local seconds = math.floor(get_attr('Acct-Session-Time') or 0)
-- For traffic above 4 GiB, the NAS puts the carry in Gigawords.
local function octets(name)
  return math.floor(get_attr(name .. '-Octets') or 0) + math.floor(get_attr(name .. '-Gigawords') or 0) * 4294967296
end

-- Packets that arrive after the end (a late Interim-Update, a retransmitted Stop) no longer change updated, so updated is never later than stopped.
sql_execute([[
  INSERT INTO accounting (client, nas, session_id, username, started, updated, session_time, input_octets, output_octets)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (client, nas, session_id) DO UPDATE SET
    updated = CASE WHEN stopped IS NULL THEN max(updated, excluded.updated) ELSE updated END,
    session_time = max(session_time, excluded.session_time),
    input_octets = max(input_octets, excluded.input_octets),
    output_octets = max(output_octets, excluded.output_octets)
]], {client, nas, session, user, seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
  -- Release this session's slot; a retransmitted Stop finds nothing and changes nothing.
  if user ~= '' then
    sql_execute("UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ? WHERE username = ? AND session_id = ? AND state = 'active'",
      {now, user, session})
  end
  return 'accept'
end

-- Start, Interim-Update: the slot.
if user == '' then
  return 'accept'
end
local expires = now + 3 * INTERIM_SECONDS
-- This session already has a slot: extend it (a retransmitted Start and Interim-Update both come here).
local held = sql_execute("UPDATE session_slots SET expires = ?, updated = ? WHERE username = ? AND session_id = ? AND state = 'active'",
  {expires, now, user, session})
if held == 0 then
  -- None: confirm a reserved slot as this session's; if there is no reserved one (the reservation expired and was released, and the NAS packet arrived late), take a free one.
  for _, state in ipairs({'reserved', 'free'}) do
    held = sql_execute([[UPDATE session_slots SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                         WHERE username = ? AND state = ?
                           AND slot = (SELECT min(slot) FROM session_slots WHERE username = ? AND state = ?)]],
      {session, nas, expires, now, user, state, user, state})
    if held == 1 then
      break
    end
  end
end
if held == 0 then
  log('usage control: no session slot for', user, 'session', session, 'from', nas)
end
return 'accept'
