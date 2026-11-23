-- 認證串接的 lua 處理器：替通過密碼驗證的用戶分配一個委派前綴（DHCPv6-PD），放進 Access-Accept 的
-- Delegated-IPv6-Prefix。前面的 sql 處理器已經驗過密碼；all_accept 遇到拒絕就停，所以密碼錯的人不會來到這裡。
--
-- 表放在線的、剛離線的前綴（prefix_leases）：沒有那一列就是空閒。順序：
--   1. 已經有前綴就沿用：reserved、active 照舊；released（他離線了，前綴暫時留給他）改回 reserved。
--   2. 沒有就從用戶名稱的雜湊決定一個起點 n，在往後 WINDOW 個 n 裡找空的 n（沒有那一列的），用 INSERT OR IGNORE
--      去佔：受影響列數是 1 就搶到，0 就是剛被別人拿走（主鍵擋住）。
--      同一個用戶每次從同一個起點開始，那個位置空著就拿到同一個前綴；被別人佔走時拿到後面的，不是保證。
--   3. 窗口裡沒有空的，就搶最舊的 released 列（那個用戶離線最久）：從最舊的幾列隨機挑一個，用一條有條件的 UPDATE
--      搶，受影響列數是 1 才算贏。
--   4. 都沒有（沒有 released 列）才在整個池裡找空的 n；整個池都滿了才拒絕，並在記錄寫一行
--      （NAS 只看到 Access-Reject）。
--   搶輸了再找時，在往後的候選裡隨機挑，免得同時進來的人每一輪都搶同一個 n。
--
-- 找空的 n 是一次讀（走主鍵），不是一個一個去寫；寫入只有搶的那一次。池越滿，要走過的連續已佔用的 n 越長，
-- 所以只看一個窗口，窗口裡沒有就搶 released（見手冊第 14 章〈IPv6 前綴池〉的量測）。
-- INTERIM_SECONDS、HOLD_SECONDS 要和 accounting.lua 一致；回收的間隔（60 秒）也要和它的 REAP_SECONDS 一致。

local RESERVE_SECONDS = 120    -- 預留多久：Accounting-Start 沒來就刪掉
local INTERIM_SECONDS = 300    -- 要求 NAS 每幾秒送一次 Interim-Update
local HOLD_SECONDS = 86400      -- 釋放後留給原來的用戶多久
local ATTEMPTS = 16            -- 搶輸了（同時有人也搶這個 n）最多再試幾次
local WINDOW = 1024            -- 從起點往後最多看幾個 n 找空的
local STEAL_CHOICES = 8        -- 搶 released 時，第一次從最舊的幾列裡隨機挑；搶輸一次，候選數乘 2（見下面的 width）

if phase ~= 'authentication' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

-- 用戶屬於哪個池。停用或不存在的帳號沒有列，不替它們分配。
local subscriber = sql_query("SELECT pool FROM subscribers WHERE username = ? AND enabled = 1", {user})
if #subscriber == 0 then
  return 'reject'
end
local pool = subscriber[1].pool
local now = math.floor(now_unix())

-- 請求觸發的回收：到期還沒確認的預留、沒收到 Interim-Update 的前綴，刪掉就是空閒。
-- 先讀本機副本判斷該不該跑，再用 UPDATE 搶一次，只有搶到的那一個去做，所以多個節點每個週期只做一次。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-prefix-pool' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-prefix-pool' AND last_run < ?", {now, now - 60}) == 1 then
  -- 沒收到 Interim 而過期的 active：改成 released，留給用戶 HOLD_SECONDS 秒；session_id 留著，NAS 其實還在線、
  -- 只是 Interim 沒到的話，之後到的 Interim 可以讓它回到 active（Stop 與 On／Off 會清掉 session_id，不行）。過期的 reserved、released：刪掉就是空閒。
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

local function give(prefix)
  set_attr('Delegated-IPv6-Prefix', prefix)
  -- WAN 的 /64 不在這裡分配：回池的名稱，由 BNG 自己的池決定。
  set_attr('Framed-IPv6-Pool', pool)
  set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
  return 'accept'
end

-- 1. 已經有前綴：沿用。預留的延長到從現在起 RESERVE_SECONDS；active 的由計費延長。
local function mine()
  return sql_query("SELECT prefix, state FROM prefix_leases WHERE username = ? AND pool = ?", {user, pool})
end
local have = mine()
if #have > 0 then
  -- released：他回來了，前綴還在，改回 reserved（等 Accounting-Start）。條件 state = 'released'：這一列剛被別人搶走時
  -- 它的 username 已經不是他，UPDATE 不會動到。
  sql_execute("UPDATE prefix_leases SET state = 'reserved', expires = ?, updated = ? WHERE username = ? AND pool = ? AND state = 'released'",
    {now + RESERVE_SECONDS, now, user, pool})
  sql_execute("UPDATE prefix_leases SET expires = max(expires, ?), updated = ? WHERE username = ? AND pool = ? AND state = 'reserved'",
    {now + RESERVE_SECONDS, now, user, pool})
  local current = mine()
  if #current > 0 then
    return give(current[1].prefix)
  end
  -- 剛好被搶走了：當作沒有，往下重新找一個。
end

local capacity_row = sql_query("SELECT (1 << (dlen - plen)) AS capacity FROM prefix_pools WHERE pool = ?", {pool})
if #capacity_row == 0 then
  log('prefix pool', pool, 'of', user, 'is not in prefix_pools')
  return 'reject'
end
local capacity = capacity_row[1].capacity

-- FNV-1a，32 位元。Luau 的數字是雙精度，32 位元乘 16777619 會超過 2^53，所以把乘法拆開：
-- 16777619 = 2^24 + 403，乘 2^24 再取 32 位元就是低 8 位元左移 24 位。
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

-- [from, upper) 裡的空段（連續的空 n）的第一個 n，最多 count 個，由小到大：已佔用的 n 的下一個 n 沒有被佔用的那些。
local GAP = [[SELECT a.n + 1 AS n FROM prefix_leases a
              WHERE a.pool = ? AND a.n >= ? AND a.n < ?
                AND NOT EXISTS (SELECT 1 FROM prefix_leases b WHERE b.pool = a.pool AND b.n = a.n + 1)
              ORDER BY a.n LIMIT ?]]

-- [from, upper) 裡最多 count 個空段的第一個 n：from 自己空著的話就是第一個。
local function free_in(from, upper, count)
  local found = {}
  if from >= upper or count < 1 then
    return found
  end
  if not taken(from) then
    found[1] = from
  end
  -- a.n < upper - 1：上界能走索引，找不到時不會一路掃到範圍外。
  for _, row in ipairs(sql_query(GAP, {pool, from, upper - 1, count - #found})) do
    found[#found + 1] = row.n
  end
  return found
end

-- 前綴的高 64 位元是 base | (n << (64 - dlen))，在 SQL 裡算：Luau 沒有 64 位元整數。
-- 文字是伺服器解碼 Delegated-IPv6-Prefix 的形式（壓縮、小寫）：前綴最長 /64，低 64 位元一定是 0，
-- 所以尾端至少四個零群，一定是被壓縮的那一段：不含尾端零群的各群（去前導零）加 ::/長度。
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

-- 起點往後 WINDOW 個 n（到池尾繞回 0）裡最多 count 個空段的第一個 n。
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

-- 整個池裡從起點往後最多 count 個空段的第一個 n；到池尾都不夠就從 0 找到起點。窗口裡沒有空的、也沒有 released 可搶時
-- 用：搶輸再來時候選數加倍，同時進來的人才不會每一輪都搶同一個 n。
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

-- 最舊的 released 列（離線最久的用戶的前綴），走部分索引 (pool, updated)。
local OLDEST = "SELECT n, username, updated FROM prefix_leases WHERE pool = ? AND state = 'released' ORDER BY updated LIMIT ?"
-- 搶它：條件是它還是那個用戶的、還是 released、還是那一次釋放（updated 沒變）。UPDATE OR IGNORE：這個用戶同時的另一個
-- 請求已經拿到一列時，(username, pool) 唯一索引會擋下，當作搶輸，不丟錯誤。
local STEAL = [[UPDATE OR IGNORE prefix_leases
                SET username = ?, state = 'reserved', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE pool = ? AND n = ? AND state = 'released' AND username = ? AND updated = ?]]

-- 搶一列最舊的 released。回傳 (搶到了, 有 released 列可搶)。
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
  -- 搶輸了：這個 n 剛被別人拿走，或這個用戶同時的另一個請求已經拿到前綴。
  local again = mine()
  if #again > 0 then
    return give(again[1].prefix)
  end
  -- 輸的不只一個：同時進來、起點落在同一段的人，都會找到同一個空的 n，每一輪只有一個贏；池越滿，空段越短，
  -- 一輪輸的人下一輪又都搶同一個空段。所以再找時從往後的空段裡隨機挑一個，候選的個數每輸一次加倍（最多 64）。
  width = math.min(2 ^ attempt, 64)
end

log('prefix pool', pool, ': no prefix for', user, 'after', ATTEMPTS, 'tries: others took the free ones as fast')
return 'reject'
