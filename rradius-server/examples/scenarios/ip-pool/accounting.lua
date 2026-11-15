-- 計費：確認、延長、釋放位址。每個寫入都有條件，同一個封包處理兩次的結果和一次相同。
--
--   Accounting-Start、Interim-Update：把用戶的位址確認成 active，到期時間設成從現在起
--                                     3 個 Interim 間隔
--   Accounting-Stop：                  釋放位址，記下上一個用的人和時間（黏著用）
--   Accounting-On／Off：               NAS 重開機，這台 NAS 的 active 位址全部放回
--
-- 計費腳本回傳 'accept' 才會回覆 NAS；其他結果 NAS 會重送。
-- 下面的數字要和 allocate.lua 一致。

local INTERIM_SECONDS = 300
local REAP_SECONDS = 60

-- 有 [auth] 時認證不會來到這裡。萬一設定改了，也不要讓這個腳本放行任何人。
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- 請求觸發的回收，同 allocate.lua。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-ip-pool' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-ip-pool' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end

-- 哪一台 NAS：一個 client 可以是整個網段，所以再用 NAS 回報的位址或名稱分開。
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')

-- NAS 重開機：它之前回報在用的位址都結束了。
if status == 7 or status == 8 then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE nas = ? AND state = 'active']], {now, now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start、Stop、Interim-Update
  return 'accept'
end

local ip = get_attr('Framed-IP-Address')
local user = get_attr('User-Name')
local session = get_attr('Acct-Session-Id')
if not ip or ip == '' or not user or user == '' or not session or session == '' then
  return 'accept' -- 沒有位址可以對的封包（例如不是這個池的 session）；重送也不會比較好
end

-- 用戶的池：位址只在他的池裡找，不同的池可以有相同的位址。
local pool = sql_query("SELECT pool FROM subscribers WHERE username = ?", {user})
if #pool == 0 then
  return 'accept'
end
local pool_name = pool[1].pool

if status == 2 then
  -- Stop：只釋放這個 session 的，或是從沒被確認過的預留。晚到的舊 session 的 Stop 動不到新的綁定。
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')
                  AND (session_id = ? OR session_id IS NULL)]], {now, now, pool_name, ip, user, session})
  return 'accept'
end

-- Start、Interim-Update：確認成 active，到期時間從現在起 3 個 Interim 間隔。
local confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                                WHERE pool = ? AND ip = ? AND username = ? AND state IN ('reserved', 'active')]],
  {session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, ip, user})
if confirmed == 0 then
  -- 綁定已經過期放回了（NAS 的封包晚到）：如果這個位址還空著，把它拿回來。
  confirmed = sql_execute([[UPDATE ip_pool SET state = 'active', username = ?, session_id = ?, nas = ?, expires = ?, updated = ?
                            WHERE pool = ? AND ip = ? AND state = 'free']],
    {user, session, nas, now + 3 * INTERIM_SECONDS, now, pool_name, ip})
end
if confirmed == 0 then
  log('ip pool', pool_name, 'address', ip, 'reported by', nas, 'for', user, 'is held by someone else')
end
return 'accept'
