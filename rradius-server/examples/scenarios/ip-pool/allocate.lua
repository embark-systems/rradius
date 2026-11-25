-- Lua handler in the auth chain: allocate an address to a user who passed password verification and put it in the Access-Accept as Framed-IP-Address.
-- The sql handler before it has already checked the password; all_accept stops at a reject, so users with a wrong password never get here.
--
-- Order:
--   1. If the user already has a binding (reserved or active), reuse it and do not take another.
--   2. Otherwise prefer the address they used last time that is free now (stickiness).
--   3. Then pick a few free addresses at random and try to claim each with a compare-and-update.
--   4. If none can be claimed, reject and write one line to the log (the NAS only sees an Access-Reject).
--
-- Claiming relies on the affected-row count returned by sql_execute: UPDATE ... WHERE state = 'free' counts as a win only if it affects 1 row.
-- Writes all go through the cluster leader, so two requests cannot claim the same address at once; the candidates are read from the local replica,
-- which may be slightly stale, costing at most one extra try.
--
-- The three numbers below must match accounting.lua.

local RESERVE_SECONDS = 120    --    how long to reserve: put it back if no Accounting-Start comes
local HOLD_SECONDS = 86400     --     how long it sticks: after an address is put back, its previous user gets it first within this time
local INTERIM_SECONDS = 300    --    ask the NAS to send an Interim-Update every this many seconds
local CANDIDATES = 8           --           how many free addresses to read at a time
local ATTEMPTS = 4             --             how many rounds of candidates to read at most

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

-- Request-triggered reclaim: reservations that expired without confirmation and sessions with no Interim-Update are put back to free.
-- Read the local replica first to decide whether to run, then claim it with one UPDATE and only the winner does the work, so with several nodes it runs once per period.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-ip-pool' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-ip-pool' AND last_run < ?", {now, now - 60}) == 1 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end

local function give(ip)
  set_attr('Framed-IP-Address', ip)
  --   An address the NAS reports in use is extended by Interim-Update; ask it to send them periodically.
  set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
  return 'accept'
end

-- 1. The user already has a binding: reuse it. A reservation is extended to RESERVE_SECONDS from now; an active one is extended by accounting.
local mine = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND username = ? AND state IN ('reserved', 'active') LIMIT 1", {pool, user})
if #mine > 0 then
  sql_execute("UPDATE ip_pool SET expires = max(expires, ?), updated = ? WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved'",
    {now + RESERVE_SECONDS, now, pool, mine[1].ip, user})
  return give(mine[1].ip)
end

-- Claim a free address: the update succeeds only if it is still free.
local function claim(ip)
  return sql_execute([[UPDATE ip_pool SET state = 'reserved', username = ?, session_id = NULL, nas = NULL, expires = ?, updated = ?
                       WHERE pool = ? AND ip = ? AND state = 'free']], {user, now + RESERVE_SECONDS, now, pool, ip}) == 1
end

-- 2. Stickiness: an address they used last time, that is free now, and that was put back less than HOLD_SECONDS ago.
local sticky = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND last_user = ? AND released >= ? ORDER BY released DESC LIMIT 1",
  {pool, user, now - HOLD_SECONDS})
if #sticky > 0 and claim(sticky[1].ip) then
  return give(sticky[1].ip)
end

-- 3. Pick a few free addresses to claim. The start point is chosen at random within the range of n of the free addresses, so requests arriving at the same time do not all go for the same batch;
-- take CANDIDATES from the start point onward, and if there are not enough after it, take from the smallest n. The smallest and largest n are two separate queries, each using the (pool, state, n)
-- index, so even a large pool does not scan all the free rows (one query that fetches min and max together cannot use the index).
-- LIMIT is written directly into the statement: it is a constant of this script, not a value sent by the request.
local from_here = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND n >= ? ORDER BY n LIMIT %d", CANDIDATES)
local from_start = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT %d", CANDIDATES)
for _ = 1, ATTEMPTS do
  local lowest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT 1", {pool})
  if #lowest == 0 then
    break
  end
  local highest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n DESC LIMIT 1", {pool})
  local first = lowest[1].n
  --   The smallest and largest are two reads, and an address may be claimed or put back in between, so the largest is not necessarily >= the smallest; take the larger so the range is not empty.
  local last = math.max(first, highest[1] and highest[1].n or first)
  local candidates = sql_query(from_here, {pool, first + math.random(0, last - first)})
  if #candidates == 0 then
    candidates = sql_query(from_start, {pool})
  end
  for _, candidate in ipairs(candidates) do
    if claim(candidate.ip) then
      return give(candidate.ip)
    end
  end
end

-- 4. No addresses left. Reject; an Access-Reject carries no reply attributes from handlers, so the reason is written to the log, where operators can see it.
log('ip pool', pool, 'is exhausted: no address for', user)
return 'reject'
