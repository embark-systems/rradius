-- 認證串接的 lua 處理器：檢查用戶方案的流量上限和同時連線數，通過的放進 Access-Accept 的 Filter-Id。
-- 前面的 sql 處理器已經驗過密碼；all_accept 遇到拒絕就停，所以密碼錯的人不會來到這裡。
--
-- 順序：
--   1. 查用戶的方案；停用或不存在的帳號拒絕。
--   2. 流量：加總這個用戶最近 period_days 天的流量。沒超過回正常的 Filter-Id；超過就依方案
--      降速（登入成功，回降速的 Filter-Id）或拒絕。先檢查流量，被拒絕的人就不會佔連線位。
--   3. 同時連線數：每個用戶有 max_sessions 個連線位，用「比較後更新」搶一個空的；搶不到就拒絕。
--
-- 搶連線位靠 sql_execute 回傳的受影響列數：UPDATE ... WHERE state = 'free' 影響 1 列才算搶到。
-- 寫入都經過叢集的領導節點，所以兩個同時進來的登入不可能搶到同一個連線位。
--
-- 拒絕時 Access-Reject 不帶處理器設的回覆屬性，所以原因寫在記錄裡。
-- 下面兩個數字要和 accounting.lua 一致。

local RESERVE_SECONDS = 120    -- 預留多久：Accounting-Start 沒來就放回
local INTERIM_SECONDS = 300    -- 要求 NAS 每幾秒送一次 Interim-Update

if phase ~= 'authentication' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

-- 1. 用戶的方案。停用或不存在的帳號沒有列。
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

-- 請求觸發的回收：到期還沒確認的預留、沒收到 Interim-Update 的 session，放回空位。
-- 先讀本機副本判斷該不該跑，再用 UPDATE 搶一次，只有搶到的那一個去做，所以多個節點每個週期只做一次。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-session-slots' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-session-slots' AND last_run < ?", {now, now - 60}) == 1 then
  sql_execute([[UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE rowid IN (SELECT rowid FROM session_slots WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now})
end

-- 2. 流量。包含還開著的 session 目前累計的量，所以同一次連線裡用掉的也算。
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

-- 3. 同時連線數。第一次登入時補齊連線位（INSERT OR IGNORE，之後不用再寫）。
local have = sql_query("SELECT count(*) AS n FROM session_slots WHERE username = ?", {user})
if have[1].n < plan.max_sessions then
  for slot = 1, plan.max_sessions do
    sql_execute("INSERT OR IGNORE INTO session_slots (username, slot) VALUES (?, ?)", {user, slot})
  end
end

-- 逐個空的連線位去搶。方案改小之後，超出 max_sessions 的連線位不再被分配。
local free = sql_query("SELECT slot FROM session_slots WHERE username = ? AND state = 'free' AND slot <= ? ORDER BY slot", {user, plan.max_sessions})
for _, candidate in ipairs(free) do
  if sql_execute([[UPDATE session_slots SET state = 'reserved', session_id = NULL, nas = NULL, expires = ?, updated = ?
                   WHERE username = ? AND slot = ? AND state = 'free']], {now + RESERVE_SECONDS, now, user, candidate.slot}) == 1 then
    if filter and filter ~= '' then
      set_attr('Filter-Id', filter)
    end
    -- NAS 回報在線的連線位靠 Interim-Update 延長；要求它定期送。
    set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
    return 'accept'
  end
end

log('usage control: session limit of plan', plan.plan, '(', plan.max_sessions, ') reached by', user, '- rejected')
return 'reject'
