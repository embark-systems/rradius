-- 計費：記錄每個 session 的流量（寫法同 quick-start），並確認、延長、釋放用戶的連線位。
--
--   Accounting-Start：  把一個預留的連線位確認成 active，到期時間設成現在起 3 個 Interim 間隔
--   Interim-Update：    延長這個 session 的連線位；找不到就當作 Start
--   Accounting-Stop：   釋放這個 session 的連線位
--   Accounting-On／Off：NAS 重開機，它的 session 都結束，連線位全部放回
--
-- 計費腳本回傳 'accept' 才會回覆 NAS；其他結果 NAS 會重送，腳本會再跑一次，所以每個寫入都
-- 要能重複執行。下面的數字要和 limits.lua 一致。

local INTERIM_SECONDS = 300
local REAP_SECONDS = 60

-- 有 [auth] 時認證不會來到這裡。萬一設定改了，也不要讓這個腳本放行任何人。
if phase ~= 'accounting' then
  return 'reject'
end

local now = math.floor(now_unix())

-- 請求觸發的回收，同 limits.lua。
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'reap-session-slots' AND last_run < ?", {now - REAP_SECONDS})
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'reap-session-slots' AND last_run < ?", {now, now - REAP_SECONDS}) == 1 then
  sql_execute([[UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE rowid IN (SELECT rowid FROM session_slots WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now})
end

-- 哪一台 NAS：一個 client 可以是整個網段，所以再用 NAS 回報的位址或名稱分開。
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')
-- NAS 重開機（Accounting-On／Off）：這台 NAS 之前的 session 都結束了。
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL", {client, nas})
  sql_execute("UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ? WHERE nas = ? AND state = 'active'", {now, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then -- Start、Stop、Interim-Update
  return 'accept'
end

local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  log('accounting without Acct-Session-Id from', client)
  return 'accept' -- 無法記錄，但重送也不會比較好
end
local user = get_attr('User-Name') or ''

-- 從計費暫存重放、或 NAS 晚送的封包，以 Acct-Delay-Time 還原發生的時間。
local seen = now - math.floor(get_attr('Acct-Delay-Time') or 0)
local seconds = math.floor(get_attr('Acct-Session-Time') or 0)
-- 超過 4 GiB 的流量，NAS 會把進位放在 Gigawords。
local function octets(name)
  return math.floor(get_attr(name .. '-Octets') or 0) + math.floor(get_attr(name .. '-Gigawords') or 0) * 4294967296
end

-- 結束後才到的封包（晚送的 Interim-Update、重送的 Stop）不再改 updated，所以 updated 不會晚於 stopped。
sql_execute([[
  INSERT INTO accounting (client, nas, session_id, username, started, updated, session_time, input_octets, output_octets)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (client, nas, session_id) DO UPDATE SET
    updated = CASE WHEN stopped IS NULL THEN max(updated, excluded.updated) ELSE updated END,
    session_time = max(session_time, excluded.session_time),
    input_octets = max(input_octets, excluded.input_octets),
    output_octets = max(output_octets, excluded.output_octets)
]], {client, nas, session, user, seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
  -- 釋放這個 session 的連線位；重送的 Stop 找不到，什麼也不改。
  if user ~= '' then
    sql_execute("UPDATE session_slots SET state = 'free', session_id = NULL, nas = NULL, expires = 0, updated = ? WHERE username = ? AND session_id = ? AND state = 'active'",
      {now, user, session})
  end
  return 'accept'
end

-- Start、Interim-Update：連線位。
if user == '' then
  return 'accept'
end
local expires = now + 3 * INTERIM_SECONDS
-- 這個 session 已經有連線位：延長（重送的 Start、Interim-Update 都走這裡）。
local held = sql_execute("UPDATE session_slots SET expires = ?, updated = ? WHERE username = ? AND session_id = ? AND state = 'active'",
  {expires, now, user, session})
if held == 0 then
  -- 沒有：把一個預留的確認成這個 session 的；沒有預留的（預留已過期放回，NAS 的封包晚到），拿一個空的。
  for _, state in ipairs({'reserved', 'free'}) do
    held = sql_execute([[UPDATE session_slots SET state = 'active', session_id = ?, nas = ?, expires = ?, updated = ?
                         WHERE username = ? AND state = ?
                           AND slot = (SELECT min(slot) FROM session_slots WHERE username = ? AND state = ?)]],
      {session, nas, expires, now, user, state, user, state})
    if held == 1 then
      break
    end
  end
end
if held == 0 then
  log('usage control: no session slot for', user, 'session', session, 'from', nas)
end
return 'accept'
