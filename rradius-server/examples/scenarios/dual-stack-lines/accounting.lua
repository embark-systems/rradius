-- 計費：確認、延長、釋放 IPv4 位址與 IPv6 前綴。每個寫入都有條件，同一個封包處理兩次的結果和一次相同。
-- 這是手冊第 14 章與第 19 章兩個計費腳本合併成一個：
--
--   Accounting-Start、Interim-Update：把用戶的位址與前綴確認成 active，到期時間設成從現在起 3 個 Interim 間隔
--   Accounting-Stop：                  釋放位址（放回空閒，記下上一個用的人）與前綴（改成 released，留給他 HOLD_SECONDS 秒）
--   Accounting-On／Off：               NAS 重開機，這台 NAS 的 active 位址與前綴全部放回
--
-- 一個封包可以同時帶 Framed-IP-Address 與 Delegated-IPv6-Prefix：有哪個就做哪個的部分。固定位址的線路在池裡
-- 沒有列，對應的 UPDATE 不會動到任何列，所以這些封包照樣處理，不會出錯。
--
-- 這條線是誰：line.lua 回給 BNG 的 Class 就是 lines.subscriber，BNG 在計費封包帶回 Class。IPoE 的計費封包沒有可靠的
-- User-Name，所以不用它。封包沒有 Class（BNG 沒帶回）時回覆 NAS 但不處理，記一行。
--
-- 計費腳本回傳 'accept' 才會回覆 NAS；其他結果 NAS 會重送。
-- 下面的數字要和 line.lua 一致。

local INTERIM_SECONDS = 300
local HOLD_SECONDS = 86400    -- 前綴釋放後留給原來的用戶多久
local REAP_SECONDS = 60

-- 有 [auth] 時認證不會來到這裡。萬一設定改了，也不要讓這個腳本放行任何人。
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- 請求觸發的回收，同 line.lua。
local function due(name)
  local rows = sql_query('SELECT 1 FROM housekeeping WHERE name = ? AND last_run < ?', {name, now - REAP_SECONDS})
  return #rows > 0 and sql_execute('UPDATE housekeeping SET last_run = ? WHERE name = ? AND last_run < ?', {now, name, now - REAP_SECONDS}) == 1
end
if due('reap-ip-pool') then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end
if due('reap-prefix-pool') then
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

-- 哪一台 NAS：一個 client 可以是整個網段，所以再用 NAS 回報的位址或名稱分開。
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')

-- NAS 重開機：它之前回報在用的位址與前綴都結束了。
if status == 7 or status == 8 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now, now, nas})
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now + HOLD_SECONDS, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start、Stop、Interim-Update
  return 'accept'
end

local user = get_attr('Class')
local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  return 'accept' -- 沒有 session 可以對的封包；重送也不會比較好
end
if not user or user == '' then
  log('dual stack accounting: no Class in the packet of session', session, 'from', nas, ': the BNG does not echo it, nothing to confirm')
  return 'accept'
end

-- 這條線的兩個池：位址和前綴只在它的池裡找，不同的池可以有相同的位址。
local plan = sql_query([[SELECT p.pool, p.pool6, l.ipv4 IS NOT NULL AS fixed4, (l.wan_v6 IS NOT NULL OR l.pd_v6 IS NOT NULL) AS fixed6
                         FROM lines l JOIN plans p ON p.plan = l.plan WHERE l.subscriber = ?]], {user})
if #plan == 0 then
  return 'accept'
end
local pool4, pool6 = plan[1].pool, plan[1].pool6
-- 固定的線路在池裡沒有列：對應的 UPDATE 不動任何列，也不是錯誤，不記錄。
local fixed4, fixed6 = plan[1].fixed4 == 1, plan[1].fixed6 == 1

local ip = get_attr('Framed-IP-Address') or ''
-- 封包帶了前綴就要對得上；沒帶就只看用戶：prefix 是空字串時，條件 (? = '' OR prefix = ?) 恆成立。
-- （SQL 的參數表裡不能放 nil，所以用空字串。）
local prefix = get_attr('Delegated-IPv6-Prefix') or ''

if status == 2 then
  -- Stop：只釋放這個 session 的，或是從沒被確認過的預留。晚到的舊 session 的 Stop 動不到新的綁定。
  if ip ~= '' then
    sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                    expires = 0, released = ?, updated = ?
                  WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')
                    AND (session_id = ? OR session_id IS NULL)]], {now, now, pool4, ip, user, session})
  end
  if not fixed6 then
    -- 封包帶了前綴就要對得上；沒帶就只看用戶（一個用戶在一個池裡只有一個前綴）。
    sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                  WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?) AND session_id = ? AND state = 'active']],
      {now + HOLD_SECONDS, now, pool6, user, prefix, prefix, session})
  end
  return 'accept'
end

-- Start、Interim-Update：確認成 active，到期時間從現在起 3 個 Interim 間隔。
if ip ~= '' and not fixed4 then
  local confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                  WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')]],
    {session, nas, now + 3 * INTERIM_SECONDS, now, pool4, ip, user})
  if confirmed == 0 then
    -- 綁定已經過期放回了（NAS 的封包晚到）：如果這個位址還空著，把它拿回來。
    confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', username = ?, session_id = ?, nas = ?, expires = ?, updated = ?
                              WHERE pool = ? AND ip = ? AND state = 'free']],
      {user, session, nas, now + 3 * INTERIM_SECONDS, now, pool4, ip})
  end
  if confirmed == 0 then
    log('ip pool', pool4, 'address', ip, 'reported by', nas, 'for', user, 'is held by someone else')
  end
end

if not fixed6 then
  -- 封包帶了前綴就要對得上；沒帶就只看用戶：prefix 是空字串時，條件 (? = '' OR prefix = ?) 恆成立。
  -- Interim-Update 只更新沒有 session 或同一個 session 的列；released 的列只有「沒收到 Interim 而過期」的（還帶著
  -- 同一個 session_id）能被它改回 active，Stop 與 On／Off 釋放的（session_id 已清掉）不算數；Start 可以。
  local confirmed = sql_execute([[UPDATE prefix_leases SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                  WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?)
                                    AND (? = 1 OR (state <> 'released' AND (session_id IS NULL OR session_id = ?)) OR (state = 'released' AND session_id = ?))]],
    {session, nas, now + 3 * INTERIM_SECONDS, now, pool6, user, prefix, prefix, status == 1 and 1 or 0, session, session})
  if confirmed == 0 then
    -- 預留已經過期刪掉了（NAS 的封包晚到），或 NAS 報的前綴不是這個用戶的：不會替它補回來，
    -- 因為文字不能反推出 n；記一行。這個前綴可能已經發給別人了，要看 NAS 的記錄。
    log('prefix pool', pool6, 'prefix', (prefix ~= '' and prefix or '(none)'), 'reported by', nas, 'for', user, 'is not in prefix_leases')
  end
end
return 'accept'
