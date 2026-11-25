-- lua handler in the auth chain: returns a fixed address, routed subnet and IPv6 prefix per user.
-- The sql handler before it has already checked the password; all_accept stops at a reject, so a user with a wrong password never gets here.
--
-- A user with a row in static_assignments: return whichever attribute has a column value.
--   ipv4       -> Framed-IP-Address, plus Framed-IP-Netmask (255.255.255.255 if not set)
--   routed_v4  -> Framed-Route: <subnet> 0.0.0.0 1
--   wan_v6     -> Framed-IPv6-Prefix
--   pd_v6      -> Delegated-IPv6-Prefix
-- A user with no such row (or a row with no address, subnet or prefix): return Framed-Pool, and the BNG's pool decides the address.
-- If a column value is not valid (not an address or prefix; the script checks netmask and routed_v4 itself), reject and write the user, column and value to the log; do not quietly fall back to dynamic,
-- or a user who pays for a fixed address would get an address that changes, and nobody would know.

local INTERIM_SECONDS = 300

if phase ~= 'authentication' then
  return 'accept'
end
if protocol ~= 'radius' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

local subscriber = sql_query('SELECT pool FROM subscribers WHERE username = ? AND enabled = 1', {user})
if #subscriber == 0 then
  log('static ip: unknown or disabled user', user)
  return 'reject'
end

local rows = sql_query('SELECT ipv4, netmask, routed_v4, wan_v6, pd_v6 FROM static_assignments WHERE username = ?', {user})

-- set_attr checks the value format against the dictionary; on an invalid value it throws, which is caught here to record the column, and then rejects.
local function put(name, column, value)
  local ok, err = pcall(set_attr, name, value)
  if not ok then
    log('static ip: bad', column, 'for', user, ':', value, '-', err)
  end
  return ok
end

local row = rows[1]
-- No row, or a row with no address, subnet or prefix (a netmask without ipv4 also counts as none): same as having no fixed data.
if row == nil or not (row.ipv4 or row.routed_v4 or row.wan_v6 or row.pd_v6) then
  set_attr('Framed-Pool', subscriber[1].pool)
  return 'accept'
end

-- The dictionary types of Framed-Route and Framed-IP-Netmask are string/address, so set_attr cannot stop values with wrong content
-- ('garbage', '255.0.255.0'); these two are therefore checked here.
local function octets(text)
  local a, b, c, d = string.match(text, '^(%d+)%.(%d+)%.(%d+)%.(%d+)')
  a, b, c, d = tonumber(a), tonumber(b), tonumber(c), tonumber(d)
  if not (a and b and c and d) or a > 255 or b > 255 or c > 255 or d > 255 then
    return nil
  end
  return a * 16777216 + b * 65536 + c * 256 + d
end

-- Subnet: a.b.c.d/n, with n from 0 to 32 and the host bits 0.
local function valid_subnet(text)
  local address, length = string.match(text, '^([%d%.]+)/(%d+)$')
  local value, bits = address and octets(address), tonumber(length)
  if not value or string.find(address, '^%d+%.%d+%.%d+%.%d+$') == nil or bits > 32 then
    return false
  end
  local host = 2 ^ (32 - bits)
  return value % host == 0
end

-- Subnet mask: 1 bits contiguous at the front, 0 bits contiguous at the back.
local function valid_netmask(text)
  local value = string.find(text, '^%d+%.%d+%.%d+%.%d+$') and octets(text)
  if not value then
    return false
  end
  local inverse = 4294967295 - value
  return bit32.band(inverse, inverse + 1) == 0
end

if row.netmask and not valid_netmask(row.netmask) then
  log('static ip: bad netmask for', user, ':', row.netmask)
  return 'reject'
end
if row.routed_v4 and not valid_subnet(row.routed_v4) then
  log('static ip: bad routed_v4 for', user, ':', row.routed_v4)
  return 'reject'
end
if row.ipv4 and not (put('Framed-IP-Address', 'ipv4', row.ipv4) and put('Framed-IP-Netmask', 'netmask', row.netmask or '255.255.255.255')) then
  return 'reject'
end
if row.routed_v4 and not put('Framed-Route', 'routed_v4', row.routed_v4 .. ' 0.0.0.0 1') then
  return 'reject'
end
if row.wan_v6 and not put('Framed-IPv6-Prefix', 'wan_v6', row.wan_v6) then
  return 'reject'
end
if row.pd_v6 and not put('Delegated-IPv6-Prefix', 'pd_v6', row.pd_v6) then
  return 'reject'
end
set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
return 'accept'
