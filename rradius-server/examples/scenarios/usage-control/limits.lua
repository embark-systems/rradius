-- lua handler in the auth chain: checks the user's plan for the traffic limit and concurrent sessions, and puts the accepted result in Filter-Id of the Access-Accept.
-- The sql handler before it has already checked the password; all_accept stops at a reject, so a user with a wrong password never gets here.
--
-- Order:
--   1. Look up the user's plan; a disabled or missing account is rejected.
--   2. Traffic: sum this user's traffic over the last period_days days. If under the limit, return the normal Filter-Id; if over, either throttle by plan
--      (login succeeds, return the throttle Filter-Id) or reject. Traffic is checked first, so a rejected user never takes a slot.
--   3. Concurrent sessions: each user has max_sessions slots, and one free slot is taken with a compare-and-update; if none can be taken, reject.
--
-- Taking a slot relies on the affected-row count returned by sql_execute: UPDATE ... WHERE state = 'free' affecting 1 row means it was taken.
-- All writes go through the cluster leader, so two simultaneous logins cannot take the same slot.
--
-- On a reject, Access-Reject does not carry the reply attributes set by handlers, so the reason is written to the log.
-- The two numbers below must match accounting.lua.

local RESERVE_SECONDS = 120    -- how long to reserve: put it back if no Accounting-Start arrives
local INTERIM_SECONDS = 300    -- ask the NAS to send an Interim-Update every this many seconds

if phase ~= 'authentication' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

-- 1. The user's plan. A disabled or missing account has no row.
local found = sql_query([[SELECT s.plan AS plan, p.max_sessions AS max_sessions, p.quota_octets AS quota_octets,
                                 p.period_days AS period_days, p.over_quota AS over_quota,
                                 p.normal_filter AS normal_filter, p.throttle_filter AS throttle_filter
                          FROM subscribers s JOIN plans p ON p.plan = s.plan
                          WHERE s.username = ? AND s.enabled = 1]], {user})
if #found == 0 then
  return 'reject'
end
local plan = found[1]
local now = math.floor(now_unix())

-- Recovery triggered by a request: reservations that expired without confirmation and sessions that got no Interim-Update are put back as free slots.
-- First read the local replica to decide whether to run, then try to win with an UPDATE; only the one that wins does the work, so across nodes it runs once per period.
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-session-slots' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-session-slots' AND last_run < ?", {now, now - 60}) == 1 then
  sql_execute([[UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE rowid IN (SELECT rowid FROM session_slots WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now})
end

-- 2. Traffic. This includes what open sessions have accumulated so far, so what was used within the same connection also counts.
local filter = plan.normal_filter
if plan.quota_octets > 0 then
  local usage = sql_query("SELECT coalesce(sum(input_octets + output_octets), 0) AS used FROM accounting WHERE username = ? AND started >= ?",
    {user, now - plan.period_days * 86400})
  if usage[1].used >= plan.quota_octets then
    if plan.over_quota == 'reject' then
      log('usage control: quota of plan', plan.plan, 'is used up by', user, '- rejected')
      return 'reject'
    end
    filter = plan.throttle_filter or plan.normal_filter
  end
end

-- 3. Concurrent sessions. On first login, fill in the slots (INSERT OR IGNORE, no more writes after that).
local have = sql_query("SELECT count(*) AS n FROM session_slots WHERE username = ?", {user})
if have[1].n < plan.max_sessions then
  for slot = 1, plan.max_sessions do
    sql_execute("INSERT OR IGNORE INTO session_slots (username, slot) VALUES (?, ?)", {user, slot})
  end
end

-- Try the free slots one by one. After a plan is made smaller, slots beyond max_sessions are no longer handed out.
local free = sql_query("SELECT slot FROM session_slots WHERE username = ? AND state = 'free' AND slot <= ? ORDER BY slot", {user, plan.max_sessions})
for _, candidate in ipairs(free) do
  if sql_execute([[UPDATE session_slots SET state = 'reserved', session_id = NULL, nas = NULL, expires = ?, updated = ?
                   WHERE username = ? AND slot = ? AND state = 'free']], {now + RESERVE_SECONDS, now, user, candidate.slot}) == 1 then
    if filter and filter ~= '' then
      set_attr('Filter-Id', filter)
    end
    -- Slots the NAS reports as online are extended by Interim-Update; ask it to send them regularly.
    set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
    return 'accept'
  end
end

log('usage control: session limit of plan', plan.plan, '(', plan.max_sessions, ') reached by', user, '- rejected')
return 'reject'
