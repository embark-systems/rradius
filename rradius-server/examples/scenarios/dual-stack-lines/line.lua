-- Dual-stack line: the user has no password; the DHCP Option 82 relayed by the BNG says which line this is (manual chapter 14, "Authenticate by line identity").
-- One script does four things, in this order:
--   1. Line identity: only a line that is known, not disabled and whose MAC lock (lock_mac) matches goes on (chapter 14, "Authenticate by line identity").
--   2. IPv4: if the line has a fixed ipv4, return it (chapter 14, "Fixed addresses and prefixes"); otherwise take one from the plan's IPv4 pool (chapter 14, "IPv4 address pool").
--   3. IPv6: if the line has a fixed wan_v6 or pd_v6, return it (chapter 14, "Fixed addresses and prefixes"); otherwise take one from the plan's prefix pool (chapter 14, "IPv6 prefix pool").
--   4. When the prefix pool is exhausted, put back the IPv4 just reserved for this request so no orphan reservation is left; then reject by default (see ALLOW_V4_ONLY below).
--
-- Why one script: handlers in an auth chain pass no values to each other (reply attributes set by an earlier handler cannot be read by a later one), so two pool scripts would
-- each have to work out "which user is this" again, and only a single script can revoke the IPv4 it just reserved when the prefix runs out.
--
-- The pool key is lines.subscriber (one user name per line). The Class returned to the BNG is the same: the BNG sends Class back in accounting packets,
-- and accounting.lua uses it to find this line, because IPoE accounting packets have no reliable User-Name.
--
-- Line identity is not a secret: anyone who can make the BNG send an arbitrary Option 82 can impersonate any line, so the client list contains only the BNG.

-- When the prefix runs out: false rejects the whole login (the NAS only sees Access-Reject; the reason is in the log);
-- true keeps the IPv4 and returns only IPv4 attributes, so the user has no IPv6 but can get online.
local ALLOW_V4_ONLY = false

-- The numbers below must match accounting.lua.
local RESERVE_SECONDS = 120    -- -- how long to reserve: put back if Accounting-Start does not arrive
local HOLD_SECONDS = 86400     -- -- how long to stick: after an address is put back, its previous user gets it first within this time
local REAP_SECONDS = 60        -- -- interval of the request-triggered reaping
-- Chapter 14, "IPv4 address pool":
local CANDIDATES = 8           -- -- how many free addresses to read at once
local V4_ATTEMPTS = 4          -- -- how many rounds of candidates to read at most
-- Chapter 14, "IPv6 prefix pool":
local ATTEMPTS = 16            -- -- how many more times to retry after losing a race
local WINDOW = 1024            -- -- how many n to look at from the start point to find a free one, at most
local STEAL_CHOICES = 8        -- -- when stealing a released prefix, the first pick is random among this many oldest rows

-- Apart from this top-level script there is no handler other than [auth], so handle protocol and phase first: if the same server also enables [tacacs],
-- accepting everything would authorize every command.
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end

local now = math.floor(now_unix())

---------------------------------------------------------------------------
-- 1. Line identity (chapter 14, "Authenticate by line identity")
---------------------------------------------------------------------------

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
  -- Access-Reject carries no attributes; the reason is in the log.
  log('dual stack: no circuit-id and no NAS-Port-Id in the request')
  return 'reject'
end
local remote = line_id(get_attr('ADSL-Agent-Remote-Id')) or ''
local mac = mac_address(get_attr('Calling-Station-Id'))

local rows = sql_query([[SELECT l.subscriber, l.enabled, l.lock_mac, l.bound_mac,
                                l.ipv4, l.netmask, l.routed_v4, l.wan_v6, l.pd_v6,
                                p.pool, p.pool6, p.profile, p.interim
                         FROM lines l LEFT JOIN plans p ON p.plan = l.plan
                         WHERE l.remote_id = ? AND l.circuit_id = ?]], {remote, circuit})
if #rows ~= 1 then
  log('dual stack: unknown line', remote, circuit)
  return 'reject'
end
local line = rows[1]
if line.enabled ~= 1 then
  log('dual stack: line', remote, circuit, 'of', line.subscriber, 'is disabled')
  return 'reject'
end
if line.pool == nil then
  log('dual stack: the plan of line', remote, circuit, 'does not exist')
  return 'reject'
end
local user = line.subscriber

if line.lock_mac == 1 then
  if not mac then
    log('dual stack: line', remote, circuit, 'is locked to a MAC and the request has none')
    return 'reject'
  end
  local bound = line.bound_mac
  if bound == nil then
    -- First time: the write succeeds only if bound_mac is still empty; if two requests arrive at once only one wins, and the other re-reads and compares.
    local took = sql_execute('UPDATE lines SET bound_mac = ? WHERE remote_id = ? AND circuit_id = ? AND bound_mac IS NULL', {mac, remote, circuit})
    if took == 1 then
      bound = mac
    else
      local again = sql_query('SELECT bound_mac FROM lines WHERE remote_id = ? AND circuit_id = ?', {remote, circuit})
      bound = again[1] and again[1].bound_mac
    end
  end
  if bound ~= mac then
    log('dual stack: line', remote, circuit, 'is locked to', bound, 'and the request has', mac)
    return 'reject'
  end
end

---------------------------------------------------------------------------
-- Check of fixed data (chapter 14, "Fixed addresses and prefixes"): when a field value is invalid, reject and log it; do not silently fall back to dynamic,
-- otherwise a user who pays for a fixed address would get one that changes, and nobody would know.
---------------------------------------------------------------------------

-- set_attr checks the value format against the dictionary and throws on an invalid value; catch it here and log which field it was.
local function put(name, column, value)
  local ok, err = pcall(set_attr, name, value)
  if not ok then
    log('dual stack: bad', column, 'for', user, ':', value, '-', err)
  end
  return ok
end

local function octets(text)
  local a, b, c, d = string.match(text, '^(%d+)%.(%d+)%.(%d+)%.(%d+)')
  a, b, c, d = tonumber(a), tonumber(b), tonumber(c), tonumber(d)
  if not (a and b and c and d) or a > 255 or b > 255 or c > 255 or d > 255 then
    return nil
  end
  return a * 16777216 + b * 65536 + c * 256 + d
end

-- Subnet: a.b.c.d/n, n from 0 to 32, host bits 0.
local function valid_subnet(text)
  local address, length = string.match(text, '^([%d%.]+)/(%d+)$')
  local value, bits = address and octets(address), tonumber(length)
  if not value or string.find(address, '^%d+%.%d+%.%d+%.%d+$') == nil or bits > 32 then
    return false
  end
  local host = 2 ^ (32 - bits)
  return value % host == 0
end

-- Subnet mask: 1s contiguous at the front, 0s contiguous at the back.
local function valid_netmask(text)
  local value = string.find(text, '^%d+%.%d+%.%d+%.%d+$') and octets(text)
  if not value then
    return false
  end
  local inverse = 4294967295 - value
  return bit32.band(inverse, inverse + 1) == 0
end

local fixed_v4 = line.ipv4 ~= nil
local fixed_v6 = line.wan_v6 ~= nil or line.pd_v6 ~= nil
if not fixed_v4 and (line.routed_v4 ~= nil or line.netmask ~= nil) then
  log('dual stack: line of', user, 'has routed_v4 or netmask but no ipv4')
  return 'reject'
end
if line.netmask and not valid_netmask(line.netmask) then
  log('dual stack: bad netmask for', user, ':', line.netmask)
  return 'reject'
end
if line.routed_v4 and not valid_subnet(line.routed_v4) then
  log('dual stack: bad routed_v4 for', user, ':', line.routed_v4)
  return 'reject'
end

---------------------------------------------------------------------------
-- Request-triggered reaping (chapters 14 and 19): put back reservations that expired unconfirmed and sessions that received no Interim-Update.
-- First read the local copy to decide whether to run, then race with an UPDATE; only the one that wins does the work, so with multiple nodes it runs once per period.
---------------------------------------------------------------------------

local function due(name)
  local rows = sql_query('SELECT 1 FROM housekeeping WHERE name = ? AND last_run < ?', {name, now - REAP_SECONDS})
  return #rows > 0 and sql_execute('UPDATE housekeeping SET last_run = ? WHERE name = ? AND last_run < ?', {now, name, now - REAP_SECONDS}) == 1
end

if not fixed_v4 and due('reap-ip-pool') then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end
if not fixed_v6 and due('reap-prefix-pool') then
  -- An active row that expired for lack of Interim: set to released, kept for the user for HOLD_SECONDS seconds; expired reserved and released rows: delete them and they are free.
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

---------------------------------------------------------------------------
-- 2. IPv4 address pool (function version of chapter 14, "IPv4 address pool" allocate.lua). Returns (address, whether this request just reserved it); returns nil if there is no address.
---------------------------------------------------------------------------

local function allocate_v4(pool)
  -- If there is already a binding (reserved or active), reuse it and do not take another. A reservation is extended; an active one is extended by accounting.
  local mine = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND username = ? AND state IN ('reserved', 'active') LIMIT 1", {pool, user})
  if #mine > 0 then
    sql_execute("UPDATE ip_pool SET expires = max(expires, ?), updated = ? WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved'",
      {now + RESERVE_SECONDS, now, pool, mine[1].ip, user})
    return mine[1].ip, false
  end

  -- Race for a free address: it succeeds only if the row is still free.
  local function claim(ip)
    return sql_execute([[UPDATE ip_pool SET state = 'reserved', username = ?, session_id = NULL, nas = NULL, expires = ?, updated = ?
                         WHERE pool = ? AND ip = ? AND state = 'free']], {user, now + RESERVE_SECONDS, now, pool, ip}) == 1
  end

  -- Sticky: an address this user used last time, that is free now, and was put back less than HOLD_SECONDS ago.
  local sticky = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND last_user = ? AND released >= ? ORDER BY released DESC LIMIT 1",
    {pool, user, now - HOLD_SECONDS})
  if #sticky > 0 and claim(sticky[1].ip) then
    return sticky[1].ip, true
  end

  -- Pick a few of the free addresses to race for. The start point is random within the n range of free addresses, so requests arriving together do not all race for the same batch.
  -- LIMIT is written straight into the statement: it is a constant of this script, not a value sent by the request.
  local from_here = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND n >= ? ORDER BY n LIMIT %d", CANDIDATES)
  local from_start = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT %d", CANDIDATES)
  for _ = 1, V4_ATTEMPTS do
    local lowest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT 1", {pool})
    if #lowest == 0 then
      break
    end
    local highest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n DESC LIMIT 1", {pool})
    local first = lowest[1].n
    local last = math.max(first, highest[1] and highest[1].n or first)
    local candidates = sql_query(from_here, {pool, first + math.random(0, last - first)})
    if #candidates == 0 then
      candidates = sql_query(from_start, {pool})
    end
    for _, candidate in ipairs(candidates) do
      if claim(candidate.ip) then
        return candidate.ip, true
      end
    end
  end

  log('ip pool', pool, 'is exhausted: no address for', user)
  return nil, false
end

-- When the prefix pool is exhausted, put back the IPv4 just reserved: touch only the row this request just reserved and that is not yet confirmed.
local function give_back_v4(pool, ip)
  sql_execute([[UPDATE ip_pool SET state = 'free', username = NULL, session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved' AND session_id IS NULL]], {now, pool, ip, user})
end

---------------------------------------------------------------------------
-- 3. IPv6 prefix pool (function version of chapter 14, "IPv6 prefix pool" allocate.lua). Returns the prefix text; returns nil if there is no prefix.
---------------------------------------------------------------------------

local function allocate_prefix(pool)
  local function mine()
    return sql_query("SELECT prefix, state FROM prefix_leases WHERE username = ? AND pool = ?", {user, pool})
  end
  -- If the user already has a prefix, reuse it: reserved and active stay as they are; released (the user went offline and the prefix was kept for them) goes back to reserved.
  local have = mine()
  if #have > 0 then
    sql_execute("UPDATE prefix_leases SET state = 'reserved', expires = ?, updated = ? WHERE username = ? AND pool = ? AND state = 'released'",
      {now + RESERVE_SECONDS, now, user, pool})
    sql_execute("UPDATE prefix_leases SET expires = max(expires, ?), updated = ? WHERE username = ? AND pool = ? AND state = 'reserved'",
      {now + RESERVE_SECONDS, now, user, pool})
    local current = mine()
    if #current > 0 then
      return current[1].prefix
    end
    -- It was just taken by someone else: treat it as none and look for a new one below.
  end

  local capacity_row = sql_query("SELECT (1 << (dlen - plen)) AS capacity FROM prefix_pools WHERE pool = ?", {pool})
  if #capacity_row == 0 then
    log('prefix pool', pool, 'of', user, 'is not in prefix_pools')
    return nil
  end
  local capacity = capacity_row[1].capacity

  -- FNV-1a, 32-bit. Luau numbers are doubles, and a 32-bit value times 16777619 would exceed 2^53, so the multiplication is split:
  -- 16777619 = 2^24 + 403; multiplying by 2^24 and keeping 32 bits is the low 8 bits shifted left by 24.
  local function hash(text)
    local h = 2166136261
    for i = 1, #text do
      h = bit32.bxor(h, string.byte(text, i))
      h = ((bit32.band(h, 255) * 16777216) + h * 403) % 4294967296
    end
    return h
  end

  local function taken(n)
    return #sql_query("SELECT 1 FROM prefix_leases WHERE pool = ? AND n = ?", {pool, n}) > 0
  end

  -- The first n of each empty run (consecutive empty n) in [from, upper), at most count of them, in ascending order.
  local GAP = [[SELECT a.n + 1 AS n FROM prefix_leases a
                WHERE a.pool = ? AND a.n >= ? AND a.n < ?
                  AND NOT EXISTS (SELECT 1 FROM prefix_leases b WHERE b.pool = a.pool AND b.n = a.n + 1)
                ORDER BY a.n LIMIT ?]]

  local function free_in(from, upper, count)
    local found = {}
    if from >= upper or count < 1 then
      return found
    end
    if not taken(from) then
      found[1] = from
    end
    for _, row in ipairs(sql_query(GAP, {pool, from, upper - 1, count - #found})) do
      found[#found + 1] = row.n
    end
    return found
  end

  -- The high 64 bits of the prefix are base | (n << (64 - dlen)), computed in SQL: Luau has no 64-bit integers.
  -- The text is the form in which the server decodes Delegated-IPv6-Prefix (compressed, lowercase).
  local CLAIM = [[WITH h AS (SELECT p.pool AS pool, p.dlen AS dlen, p.base | (? << (64 - p.dlen)) AS v
                             FROM prefix_pools p WHERE p.pool = ?)
                  INSERT OR IGNORE INTO prefix_leases (pool, n, prefix, state, username, expires, updated)
                  SELECT h.pool, ?,
                         CASE WHEN (h.v & 65535) <> 0
                                THEN printf('%x:%x:%x:%x::/', (h.v >> 48) & 65535, (h.v >> 32) & 65535, (h.v >> 16) & 65535, h.v & 65535)
                              WHEN ((h.v >> 16) & 65535) <> 0
                                THEN printf('%x:%x:%x::/', (h.v >> 48) & 65535, (h.v >> 32) & 65535, (h.v >> 16) & 65535)
                              WHEN ((h.v >> 32) & 65535) <> 0
                                THEN printf('%x:%x::/', (h.v >> 48) & 65535, (h.v >> 32) & 65535)
                              WHEN ((h.v >> 48) & 65535) <> 0
                                THEN printf('%x::/', (h.v >> 48) & 65535)
                              ELSE '::/' END || h.dlen,
                         'reserved', ?, ?, ?
                  FROM h]]

  local start = hash(user) % capacity

  -- The first n of at most count empty runs within WINDOW n after the start point (wrapping to 0 at the end of the pool).
  local function free_near(count)
    local upper = math.min(start + WINDOW, capacity)
    local found = free_in(start, upper, count)
    if #found < count and start + WINDOW > capacity then
      for _, free in ipairs(free_in(0, math.min(start + WINDOW - capacity, start), count - #found)) do
        found[#found + 1] = free
      end
    end
    return found
  end

  -- The first n of at most count empty runs from the start point onward in the whole pool; used when the window has no empty n and there is no released prefix to steal.
  local function free_anywhere(count)
    local found = free_in(start, capacity, count)
    if #found < count then
      for _, free in ipairs(free_in(0, start, count - #found)) do
        found[#found + 1] = free
      end
    end
    return found
  end

  local function claim(n)
    return sql_execute(CLAIM, {n, pool, n, user, now + RESERVE_SECONDS, now}) == 1
  end

  -- The oldest released row (the prefix of the user who has been offline longest).
  local OLDEST = "SELECT n, username, updated FROM prefix_leases WHERE pool = ? AND state = 'released' ORDER BY updated LIMIT ?"
  -- Steal it: the condition is that it still belongs to that user, is still released, and is still that same release (updated unchanged).
  local STEAL = [[UPDATE OR IGNORE prefix_leases
                  SET username = ?, state = 'reserved', session_id = NULL, nas = NULL, expires = ?, updated = ?
                  WHERE pool = ? AND n = ? AND state = 'released' AND username = ? AND updated = ?]]

  -- Steal the oldest released row. Returns (whether it was stolen, whether there was a released row to steal).
  local function steal(width)
    local oldest = sql_query(OLDEST, {pool, STEAL_CHOICES * width})
    if #oldest == 0 then
      return false, false
    end
    local row = oldest[math.random(#oldest)]
    return sql_execute(STEAL, {user, now + RESERVE_SECONDS, now, pool, row.n, row.username, row.updated}) == 1, true
  end

  local width = 1
  for attempt = 1, ATTEMPTS do
    local won = false
    local free = free_near(width)
    if #free > 0 then
      won = claim(free[math.random(#free)])
    else
      local stole, any = steal(width)
      if stole then
        won = true
      elseif not any then
        local anywhere = free_anywhere(width)
        if #anywhere == 0 then
          log('prefix pool', pool, 'is exhausted: no prefix for', user)
          return nil
        end
        won = claim(anywhere[math.random(#anywhere)])
      end
    end
    if won then
      local got = mine()
      if #got > 0 then
        return got[1].prefix
      end
    end
    -- Lost the race: this n was just taken by someone else, or another concurrent request of this user already got a prefix.
    local again = mine()
    if #again > 0 then
      return again[1].prefix
    end
    width = math.min(2 ^ attempt, 64)
  end

  log('prefix pool', pool, ': no prefix for', user, 'after', ATTEMPTS, 'tries: others took the free ones as fast')
  return nil
end

---------------------------------------------------------------------------
-- Combine: IPv4 first, then IPv6 (the IPv4 only needs revoking if IPv6 fails).
---------------------------------------------------------------------------

local v4, v4_fresh
-- Before rejecting, put back the IPv4 just reserved for this request so no orphan reservation is left.
local function refuse()
  if v4 and v4_fresh then
    give_back_v4(line.pool, v4)
  end
  return 'reject'
end

if fixed_v4 then
  if not (put('Framed-IP-Address', 'ipv4', line.ipv4) and put('Framed-IP-Netmask', 'netmask', line.netmask or '255.255.255.255')) then
    return 'reject'
  end
  if line.routed_v4 and not put('Framed-Route', 'routed_v4', line.routed_v4 .. ' 0.0.0.0 1') then
    return 'reject'
  end
else
  v4, v4_fresh = allocate_v4(line.pool)
  if not v4 then
    return 'reject'
  end
end

local v6
if fixed_v6 then
  if line.wan_v6 and not put('Framed-IPv6-Prefix', 'wan_v6', line.wan_v6) then
    return refuse()
  end
  if line.pd_v6 and not put('Delegated-IPv6-Prefix', 'pd_v6', line.pd_v6) then
    return refuse()
  end
else
  v6 = allocate_prefix(line.pool6)
  -- A fixed IPv4 also counts as "has IPv4": a line with only a fixed IPv4 is also let through per ALLOW_V4_ONLY when the prefix pool is exhausted.
  if not v6 and (not ALLOW_V4_ONLY or not (fixed_v4 or v4)) then
    return refuse()
  end
end

-- Fixed attributes were set above; those from the pools are set here.
if v4 then
  set_attr('Framed-IP-Address', v4)
end
if v6 then
  set_attr('Delegated-IPv6-Prefix', v6)
  -- The WAN /64 is not allocated here: return the pool's name and let the BNG's own pool decide.
  set_attr('Framed-IPv6-Pool', line.pool6)
end
set_attr('Filter-Id', line.profile)
set_attr('Acct-Interim-Interval', line.interim)
-- The BNG sends Class back in accounting packets; accounting.lua uses it to know which line this is.
set_attr('Class', user)

-- Record the last MAC and time; this line is written once per login and can be removed if not needed (docs/manual/14-isp-recipes.md "Known limitations" of "Authenticate by line identity").
if mac then
  sql_execute('UPDATE lines SET last_mac = ?, last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {mac, now, remote, circuit})
else
  sql_execute('UPDATE lines SET last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {now, remote, circuit})
end
return 'accept'
