-- 認證串接的 lua 處理器：替通過密碼驗證的用戶分配位址，放進 Access-Accept 的 Framed-IP-Address。
-- 前面的 sql 處理器已經驗過密碼；all_accept 遇到拒絕就停，所以密碼錯的人不會來到這裡。
--
-- 順序：
--   1. 已經有綁定（reserved 或 active）就沿用，不多佔一個。
--   2. 沒有就優先拿他上次用過、現在空閒的位址（黏著）。
--   3. 再從空閒的位址隨機挑幾個，逐個用「比較後更新」去搶。
--   4. 搶不到任何一個就拒絕，並在記錄寫一行（NAS 只看到 Access-Reject）。
--
-- 搶位址靠 sql_execute 回傳的受影響列數：UPDATE ... WHERE state = 'free' 影響 1 列才算搶到。
-- 寫入都經過叢集的領導節點，所以兩個請求不可能同時搶到同一個位址；候選是從本機副本讀的，
-- 可能稍舊，頂多多試一個。
--
-- 下面三個數字要和 accounting.lua 一致。

local RESERVE_SECONDS = 120    -- 預留多久：Accounting-Start 沒來就放回
local HOLD_SECONDS = 86400     -- 黏著多久：位址放回後，原用戶在這段時間內優先拿回它
local INTERIM_SECONDS = 300    -- 要求 NAS 每幾秒送一次 Interim-Update
local CANDIDATES = 8           -- 一次讀幾個空閒位址
local ATTEMPTS = 4             -- 最多讀幾輪候選

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

-- 請求觸發的回收：到期還沒確認的預留、沒收到 Interim-Update 的 session，放回空閒。
-- 先讀本機副本判斷該不該跑，再用 UPDATE 搶一次，只有搶到的那一個去做，所以多個節點每個週期只做一次。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-ip-pool' AND last_run < ?", {now - 60})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-ip-pool' AND last_run < ?", {now, now - 60}) == 1 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end

local function give(ip)
  set_attr('Framed-IP-Address', ip)
  -- NAS 回報在用的位址靠 Interim-Update 延長；要求它定期送。
  set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
  return 'accept'
end

-- 1. 已經有綁定：沿用。預留的延長到從現在起 RESERVE_SECONDS；active 的由計費延長。
local mine = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND username = ? AND state IN ('reserved', 'active') ORDER BY ip LIMIT 1", {pool, user})
if #mine > 0 then
  sql_execute("UPDATE ip_pool SET expires = max(expires, ?), updated = ? WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved'",
    {now + RESERVE_SECONDS, now, pool, mine[1].ip, user})
  return give(mine[1].ip)
end

-- 搶一個空閒的位址：只有它還是 free 才會改成功。
local function claim(ip)
  return sql_execute([[UPDATE ip_pool SET state = 'reserved', username = ?, session_id = NULL, nas = NULL, expires = ?, updated = ?
                       WHERE pool = ? AND ip = ? AND state = 'free']], {user, now + RESERVE_SECONDS, now, pool, ip}) == 1
end

-- 2. 黏著：他上次用過、現在空閒、而且放回不到 HOLD_SECONDS 的位址。
local sticky = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND last_user = ? AND released >= ? ORDER BY released DESC LIMIT 1",
  {pool, user, now - HOLD_SECONDS})
if #sticky > 0 and claim(sticky[1].ip) then
  return give(sticky[1].ip)
end

-- 3. 隨機挑幾個空閒的位址來搶。隨機是為了讓同時進來的請求不要都搶同一個。
-- LIMIT 直接寫進語句：它是這個腳本的常數，不是請求送來的值。
local pick = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY random() LIMIT %d", CANDIDATES)
for _ = 1, ATTEMPTS do
  local candidates = sql_query(pick, {pool})
  if #candidates == 0 then
    break
  end
  for _, candidate in ipairs(candidates) do
    if claim(candidate.ip) then
      return give(candidate.ip)
    end
  end
end

-- 4. 沒有位址了。拒絕；Access-Reject 不帶處理器的回覆屬性，所以原因寫在記錄裡，維運從記錄看得到。
log('ip pool', pool, 'is exhausted: no address for', user)
return 'reject'
