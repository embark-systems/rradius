-- Lua handler in the auth chain: allocate a delegated prefix (DHCPv6-PD) to a user who passed password verification and put it in the Access-Accept as
-- Delegated-IPv6-Prefix. The sql handler before it has already checked the password; all_accept stops at a reject, so users with a wrong password never get here.
--
-- The table holds the prefixes that are online or just went offline (prefix_leases): a prefix with no row is free. Order:
--   1. If the user already has a prefix, reuse it: reserved and active stay as they are; released (the user went offline and the prefix was kept for them for now) goes back to reserved.
--   2. Otherwise derive a start n from a hash of the user name, look among the next WINDOW values of n for a free n (one with no row), and claim it with INSERT OR IGNORE:
--      1 affected row means it was won, 0 means someone else just took it (the primary key blocked the insert).
--      The same user always starts from the same point, so if that position is free they get the same prefix; if someone else holds it they get a later one, which is not guaranteed.
--   3. If the window has no free n, take the oldest released row (the user who has been offline longest): pick one at random from the oldest few rows and
--      take it with a conditional UPDATE; it is won only if 1 row is affected.
--   4. Only if there is none (no released row) look for a free n in the whole pool; reject only when the whole pool is full, and write one line to the log
--      (the NAS only sees an Access-Reject).
--   When a claim is lost and the search repeats, pick at random among the later candidates so users arriving at the same time do not all go for the same n every round.
--
-- Finding a free n is one read (it walks the primary key), not a write one by one; the only write is the claim itself. The fuller the pool, the longer the run of consecutive occupied n values to walk,
-- so only a window is examined, and if it has no free n a released row is taken (see the measurements in manual chapter 14, "IPv6 prefix pool: prefix-pool").
-- INTERIM_SECONDS and HOLD_SECONDS must match accounting.lua; the reclaim interval (60 seconds) must also match its REAP_SECONDS.

local RESERVE_SECONDS = 120    --    how long to reserve: delete it if no Accounting-Start comes
local INTERIM_SECONDS = 300    --    ask the NAS to send an Interim-Update every this many seconds
local HOLD_SECONDS = 86400      --      how long a released prefix is kept for its previous user
local ATTEMPTS = 16            --            how many more times to retry after losing a claim (someone else claimed the same n at the same time)
local WINDOW = 1024            --            how many values of n to look at from the start point when searching for a free one
local STEAL_CHOICES = 8        --        when taking a released row, pick at random from the oldest few rows the first time; each lost claim doubles the number of candidates (see width below)

if phase ~= 'authentication' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

-- Which pool the user belongs to. Disabled or nonexistent accounts have no row, and nothing is allocated to them.
local subscriber = sql_query("SELECT pool FROM subscribers WHERE username = ? AND enabled = 1", {user})
if #subscriber == 0 then
  return 'reject'
end
local pool = subscriber[1].pool
local now = math.floor(now_unix())

-- Request-triggered reclaim: reservations that expired without confirmation and prefixes with no Interim-Update; deleting them makes them free.
-- Read the local replica first to decide whether to run, then claim it with one UPDATE and only the winner does the work, so with several nodes it runs once per period.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-prefix-pool' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-prefix-pool' AND last_run < ?", {now, now - 60}) == 1 then
  --   An active row that expired without an Interim: becomes released and is kept for the user for HOLD_SECONDS; session_id is kept, because the NAS may still be online
  --   and only the Interim was missed, so a later Interim can bring it back to active (Stop and On/Off clear session_id, so they cannot). An expired reserved or released row: deleting it makes it free.
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

local function give(prefix)
  set_attr('Delegated-IPv6-Prefix', prefix)
  --   The WAN /64 is not allocated here: it is returned as a pool name, and the BNG's own pool decides it.
  set_attr('Framed-IPv6-Pool', pool)
  set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
  return 'accept'
end

-- 1. The user already has a prefix: reuse it. A reservation is extended to RESERVE_SECONDS from now; an active one is extended by accounting.
local function mine()
  return sql_query("SELECT prefix, state FROM prefix_leases WHERE username = ? AND pool = ?", {user, pool})
end
local have = mine()
if #have > 0 then
  --   released: the user came back and the prefix is still there, so it goes back to reserved (waiting for Accounting-Start). The condition state = 'released': if the row was just taken by someone else,
  --   its username is no longer this user and the UPDATE does not touch it.
  sql_execute("UPDATE prefix_leases SET state = 'reserved', expires = ?, updated = ? WHERE username = ? AND pool = ? AND state = 'released'",
    {now + RESERVE_SECONDS, now, user, pool})
  sql_execute("UPDATE prefix_leases SET expires = max(expires, ?), updated = ? WHERE username = ? AND pool = ? AND state = 'reserved'",
    {now + RESERVE_SECONDS, now, user, pool})
  local current = mine()
  if #current > 0 then
    return give(current[1].prefix)
  end
  --   It was just taken by someone else: treat it as not found and go on to find a new one below.
end

local capacity_row = sql_query("SELECT (1 << (dlen - plen)) AS capacity FROM prefix_pools WHERE pool = ?", {pool})
if #capacity_row == 0 then
  log('prefix pool', pool, 'of', user, 'is not in prefix_pools')
  return 'reject'
end
local capacity = capacity_row[1].capacity

-- FNV-1a, 32 bit. Luau numbers are doubles, and multiplying a 32-bit value by 16777619 exceeds 2^53, so the multiplication is split:
-- 16777619 = 2^24 + 403, and multiplying by 2^24 and keeping 32 bits is the low 8 bits shifted left by 24.
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

-- The first n of each free run (consecutive free n) in [from, upper), at most count of them, in ascending order: those n whose next n is not occupied.
local GAP = [[SELECT a.n + 1 AS n FROM prefix_leases a
              WHERE a.pool = ? AND a.n >= ? AND a.n < ?
                AND NOT EXISTS (SELECT 1 FROM prefix_leases b WHERE b.pool = a.pool AND b.n = a.n + 1)
              ORDER BY a.n LIMIT ?]]

-- The first n of at most count free runs in [from, upper): if from itself is free, it is the first.
local function free_in(from, upper, count)
  local found = {}
  if from >= upper or count < 1 then
    return found
  end
  if not taken(from) then
    found[1] = from
  end
  --   a.n < upper - 1: the upper bound can use the index, so when nothing is found it does not scan all the way past the range.
  for _, row in ipairs(sql_query(GAP, {pool, from, upper - 1, count - #found})) do
    found[#found + 1] = row.n
  end
  return found
end

-- The high 64 bits of the prefix are base | (n << (64 - dlen)), computed in SQL: Luau has no 64-bit integers.
-- The text is the form the server decodes Delegated-IPv6-Prefix to (compressed, lower case): a prefix is at most /64, so the low 64 bits are always 0,
-- so the end has at least four zero groups, and they are always the compressed part: the groups without the trailing zero groups (leading zeros removed) plus ::/length.
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

-- The first n of at most count free runs within WINDOW values of n from the start point (wrapping to 0 at the end of the pool).
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

-- The first n of at most count free runs in the whole pool from the start point; if that is not enough by the end of the pool, search from 0 up to the start point. Used when the window has no free n and there is no released row to take:
-- on a retry after losing a claim the number of candidates doubles, so users arriving at the same time do not all go for the same n every round.
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

-- The oldest released row (the prefix of the user offline longest), using the partial index (pool, updated).
local OLDEST = "SELECT n, username, updated FROM prefix_leases WHERE pool = ? AND state = 'released' ORDER BY updated LIMIT ?"
-- Take it: the condition is that it still belongs to that user, is still released, and is still that same release (updated unchanged). UPDATE OR IGNORE: when another concurrent
-- request of this user already got a row, the unique index (username, pool) blocks it, which counts as a lost claim and raises no error.
local STEAL = [[UPDATE OR IGNORE prefix_leases
                SET username = ?, state = 'reserved', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE pool = ? AND n = ? AND state = 'released' AND username = ? AND updated = ?]]

-- Take the oldest released row. Returns (won, whether a released row was available to take).
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
        return 'reject'
      end
      won = claim(anywhere[math.random(#anywhere)])
    end
  end
  if won then
    local got = mine()
    if #got > 0 then
      return give(got[1].prefix)
    end
  end
  --   Lost: this n was just taken by someone else, or another concurrent request of this user already got a prefix.
  local again = mine()
  if #again > 0 then
    return give(again[1].prefix)
  end
  --   More than one loser: users who arrive at the same time and start in the same segment all find the same free n, and only one wins each round; the fuller the pool, the shorter the free runs,
  --   and the losers of one round all go for the same free run in the next. So the retry picks at random from the later free runs, and the number of candidates doubles with each loss (up to 64).
  width = math.min(2 ^ attempt, 64)
end

log('prefix pool', pool, ': no prefix for', user, 'after', ATTEMPTS, 'tries: others took the free ones as fast')
return 'reject'
