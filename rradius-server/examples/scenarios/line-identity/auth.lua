-- Line identity authentication: the subscriber has no password; the DHCP Option 82 relayed by the BNG says which line this is.
--
-- Line: ADSL-Agent-Circuit-Id (RFC 4679, the circuit-id relayed by the BNG), or NAS-Port-Id if absent;
-- remote-id is ADSL-Agent-Remote-Id, or an empty string if absent. Both ids are normalized before the lookup in the lines table:
--   - a value with non-printable (binary) characters is written as 0x plus lowercase hex, with nothing else done to it;
--   - any other value has leading and trailing whitespace removed and is lowercased.
--
-- Line identity is not a secret: anyone who can make the BNG send any Option 82 can impersonate any line, so the client list contains only the BNGs.
-- For one more layer, set lock_mac to 1: the first login binds the MAC, and a different MAC afterwards is rejected.
--
-- This script is the top-level script (there is no [auth]), so it handles protocol and phase first: if the same server also enables [tacacs],
-- accepting everything would authorize every command.

if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end

local function line_id(value)
  if value == nil or value == '' then
    return nil
  end
  if string.find(value, '[^\32-\126]') then
    return '0x' .. (string.gsub(value, '.', function(c) return string.format('%02x', string.byte(c)) end))
  end
  local text = string.lower(string.match(value, '^%s*(.-)%s*$'))
  if text == '' then
    return nil
  end
  return text
end

local function mac_address(text)
  local hex = string.gsub(string.lower(text or ''), '[^0-9a-f]', '')
  return #hex == 12 and hex or nil
end

local circuit = line_id(get_attr('ADSL-Agent-Circuit-Id')) or line_id(get_attr('NAS-Port-Id'))
if not circuit then
  -- Access-Reject carries no attributes; the reason goes in the log.
  log('line identity: no circuit-id and no NAS-Port-Id in the request')
  return 'reject'
end
local remote = line_id(get_attr('ADSL-Agent-Remote-Id')) or ''
local mac = mac_address(get_attr('Calling-Station-Id'))

local rows = sql_query([[SELECT l.subscriber, l.enabled, l.lock_mac, l.bound_mac, p.pool, p.profile, p.interim
                         FROM lines l LEFT JOIN plans p ON p.plan = l.plan
                         WHERE l.remote_id = ? AND l.circuit_id = ?]], {remote, circuit})
if #rows ~= 1 then
  log('line identity: unknown line', remote, circuit)
  return 'reject'
end
local line = rows[1]
if line.enabled ~= 1 then
  log('line identity: line', remote, circuit, 'of', line.subscriber, 'is disabled')
  return 'reject'
end
if line.pool == nil then
  log('line identity: the plan of line', remote, circuit, 'does not exist')
  return 'reject'
end

if line.lock_mac == 1 then
  if not mac then
    log('line identity: line', remote, circuit, 'is locked to a MAC and the request has none')
    return 'reject'
  end
  local bound = line.bound_mac
  if bound == nil then
    -- First time: the write succeeds only while bound_mac is still empty; if two requests arrive together, only one wins and the other rereads and compares.
    local took = sql_execute('UPDATE lines SET bound_mac = ? WHERE remote_id = ? AND circuit_id = ? AND bound_mac IS NULL', {mac, remote, circuit})
    if took == 1 then
      bound = mac
    else
      local again = sql_query('SELECT bound_mac FROM lines WHERE remote_id = ? AND circuit_id = ?', {remote, circuit})
      bound = again[1] and again[1].bound_mac
    end
  end
  if bound ~= mac then
    log('line identity: line', remote, circuit, 'is locked to', bound, 'and the request has', mac)
    return 'reject'
  end
end

-- Record the last MAC and time; this line writes once per login and can be removed if not needed (manual, "Known limitations").
-- If the request has no MAC, only the time is updated and the MAC seen last time is kept.
if mac then
  sql_execute('UPDATE lines SET last_mac = ?, last_seen = ? WHERE remote_id = ? AND circuit_id = ?',
    {mac, math.floor(now_unix()), remote, circuit})
else
  sql_execute('UPDATE lines SET last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {math.floor(now_unix()), remote, circuit})
end

set_attr('Framed-Pool', line.pool)
set_attr('Filter-Id', line.profile)
set_attr('Acct-Interim-Interval', line.interim)
-- The BNG returns Class in accounting packets, so the accounting records know who this line is.
set_attr('Class', line.subscriber)
return 'accept'
