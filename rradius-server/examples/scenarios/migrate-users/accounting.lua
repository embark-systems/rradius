-- Accounting: one row per session; Start creates it, Interim-Update and Stop update it.
--
-- The accounting script must return 'accept' for the NAS to get a reply; for any other result the NAS retransmits and the script runs again, so every write
-- must be safe to repeat. Writing the same packet twice gives the same result as writing it once.

-- With [auth] set, authentication never reaches this script. If the configuration changes later, this script still must not let anyone in.
if phase ~= 'accounting' then
  return 'reject'
end

-- Which NAS: one client can be a whole subnet, so also tell NASes apart by the address or name the NAS reports.
-- These values are what the NAS says about itself, but they are only compared within one client: NASes in the same client share a secret, so they are equally trusted.
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')
-- NAS reboot (Accounting-On/Off): all earlier sessions of this NAS have ended.
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start, Stop, Interim-Update
  return 'accept'
end

local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  log('accounting without Acct-Session-Id from', client)
  return 'accept' -- cannot record it, but a retransmit would not help
end

-- For packets replayed from the accounting spool (the on-disk queue of accounting requests, kept while the database cannot be written) or sent late by the NAS, use Acct-Delay-Time to restore the time the event happened.
local seen = math.floor(now_unix()) - math.floor(get_attr('Acct-Delay-Time') or 0)
local seconds = math.floor(get_attr('Acct-Session-Time') or 0)
-- For traffic over 4 GiB, the NAS puts the carry in Gigawords.
local function octets(name)
  return math.floor(get_attr(name .. '-Octets') or 0) + math.floor(get_attr(name .. '-Gigawords') or 0) * 4294967296
end

-- Packets that arrive after the session ended (a late Interim-Update, a retransmitted Stop) no longer change updated, so updated is never later than stopped.
sql_execute([[
  INSERT INTO accounting (client, nas, session_id, username, started, updated, session_time, input_octets, output_octets)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (client, nas, session_id) DO UPDATE SET
    updated = CASE WHEN stopped IS NULL THEN max(updated, excluded.updated) ELSE updated END,
    session_time = max(session_time, excluded.session_time),
    input_octets = max(input_octets, excluded.input_octets),
    output_octets = max(output_octets, excluded.output_octets)
]], {client, nas, session, get_attr('User-Name') or '', seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
end
return 'accept'
