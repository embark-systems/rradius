-- 計費：確認、延長、釋放前綴。每個寫入都有條件，同一個封包處理兩次的結果和一次相同。
--
--   Accounting-Start、Interim-Update：把用戶的前綴確認成 active，到期時間設成從現在起 3 個 Interim 間隔
--   Accounting-Stop：                  釋放前綴：那一列改成 released，留給這個用戶 HOLD_SECONDS 秒
--   Accounting-On／Off：               NAS 重開機，這台 NAS 的 active 前綴全部改成 released
--
-- released 的前綴：用戶離線了，前綴暫時留給他，他回來登入先拿到同一個；別人只有在池裡找不到空的 n 時才會搶最舊的
-- released。過了 HOLD_SECONDS 才刪掉，刪掉才是空閒。
--
-- 前綴靠 Delegated-IPv6-Prefix 的文字找到那一列：那是伺服器把 NAS 送來的屬性解碼出來的文字
-- （壓縮、小寫），allocate.lua 存的 prefix 欄位是同一種形式，所以字串相等就是同一個前綴。
-- NAS 的計費封包沒帶前綴時，退回只用用戶名稱：一個用戶在一個池裡只有一個前綴。
--
-- 計費腳本回傳 'accept' 才會回覆 NAS；其他結果 NAS 會重送。
-- 下面的 INTERIM_SECONDS、HOLD_SECONDS 要和 allocate.lua 一致；REAP_SECONDS 是回收的間隔（allocate.lua 裡的 60）。

local INTERIM_SECONDS = 300
local HOLD_SECONDS = 86400    -- 釋放後留給原來的用戶多久
local REAP_SECONDS = 60

-- 有 [auth] 時認證不會來到這裡。萬一設定改了，也不要讓這個腳本放行任何人。
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- 請求觸發的回收，同 allocate.lua。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-prefix-pool' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-prefix-pool' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  -- 沒收到 Interim 而過期的 active：改成 released，留給用戶 HOLD_SECONDS 秒；session_id 留著，NAS 其實還在線、
  -- 只是 Interim 沒到的話，之後到的 Interim 可以讓它回到 active（Stop 與 On／Off 會清掉 session_id，不行）。過期的 reserved、released：刪掉就是空閒。
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

-- NAS 重開機：它之前回報在用的前綴都結束了。
if status == 7 or status == 8 then
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now + HOLD_SECONDS, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start、Stop、Interim-Update
  return 'accept'
end

local user = get_attr('User-Name')
local session = get_attr('Acct-Session-Id')
if not user or user == '' or not session or session == '' then
  return 'accept' -- 沒有用戶可以對的封包；重送也不會比較好
end

-- 用戶的池：前綴只在他的池裡找，不同的池可以有相同的前綴文字。
local pool = sql_query("SELECT pool FROM subscribers WHERE username = ?", {user})
if #pool == 0 then
  return 'accept'
end
local pool_name = pool[1].pool

-- 封包帶了前綴就要對得上；沒帶就只看用戶：prefix 是空字串時，下面的條件 (? = '' OR prefix = ?) 恆成立。
-- （SQL 的參數表裡不能放 nil，所以用空字串。）
local prefix = get_attr('Delegated-IPv6-Prefix') or ''

if status == 2 then
  -- Stop：只釋放這個 session 的（改成 released，留給用戶 HOLD_SECONDS 秒）。晚到的舊 session 的 Stop 動不到
  -- 新的綁定，也動不到還沒確認的預留（那個交給回收，預留過期就刪）。
  sql_execute([[UPDATE prefix_leases SET state = 'released', session_id = NULL, nas = NULL, expires = ?, updated = ?
                WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?) AND session_id = ? AND state = 'active']],
    {now + HOLD_SECONDS, now, pool_name, user, prefix, prefix, session})
  return 'accept'
end

-- Start、Interim-Update：確認成 active，到期時間從現在起 3 個 Interim 間隔。
-- Interim-Update 只更新沒有 session 或同一個 session 的列；released 的列只有「沒收到 Interim 而過期」的（還帶著
-- 同一個 session_id）能被它改回 active，Stop 與 On／Off 釋放的（session_id 已清掉）不算數；Start 可以：快速重連時，舊 session 的 Stop 先釋放，新 session 的 Start 才到，前綴還是他的。
local confirmed = sql_execute([[UPDATE prefix_leases SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                WHERE pool = ? AND username = ? AND (? = '' OR prefix = ?)
                                  AND (? = 1 OR (state <> 'released' AND (session_id IS NULL OR session_id = ?)) OR (state = 'released' AND session_id = ?))]],
  {session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, user, prefix, prefix, status == 1 and 1 or 0, session, session})
if confirmed == 0 then
  -- 預留已經過期刪掉了（NAS 的封包晚到），或 NAS 報的前綴不是這個用戶的：不會替它補回來，
  -- 因為文字不能反推出 n；記一行。這個前綴可能已經發給別人了，要看 NAS 的記錄。
  log('prefix pool', pool_name, 'prefix', (prefix ~= '' and prefix or '(none)'), 'reported by', nas, 'for', user, 'is not in prefix_leases')
end
return 'accept'
