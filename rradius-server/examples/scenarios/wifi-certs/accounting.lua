-- 計費：每個 session 一列，Start 建立，Interim-Update 和 Stop 更新。
--
-- 計費腳本回傳 'accept' 才會回覆 NAS；其他結果 NAS 會重送，腳本會再跑一次，所以每個寫入都
-- 要能重複執行。同一個封包寫兩次，結果和寫一次相同。

-- 有 [auth] 時認證不會來到這裡。萬一設定改了，也不要讓這個腳本放行任何人。
if phase ~= 'accounting' then
  return 'reject'
end

-- 哪一台 NAS：一個 client 可以是整個網段，所以再用 NAS 回報的位址或名稱分開。
-- 這些值是 NAS 自己說的，但只在同一個 client 裡比較：同一個 client 的 NAS 共用密碼，本來就同樣可信。
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end

local status = get_attr('Acct-Status-Type')
-- NAS 重開機（Accounting-On／Off）：這台 NAS 之前的 session 都結束了。
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
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

-- 從計費暫存重放、或 NAS 晚送的封包，以 Acct-Delay-Time 還原發生的時間。
local seen = math.floor(now_unix()) - math.floor(get_attr('Acct-Delay-Time') or 0)
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
]], {client, nas, session, get_attr('User-Name') or '', seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
end
return 'accept'
