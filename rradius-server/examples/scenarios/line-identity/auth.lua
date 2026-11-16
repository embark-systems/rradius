-- 線路身分認證：用戶沒有密碼，BNG 轉送的 DHCP Option 82 說這是哪一條線。
--
-- 線路：ADSL-Agent-Circuit-Id（RFC 4679，BNG 轉送的 circuit-id），沒有的話用 NAS-Port-Id；
-- remote-id 是 ADSL-Agent-Remote-Id，沒有就當空字串。兩個 id 先正規化再查 lines 表：
--   - 含不可印字元（二進位）的值，寫成 0x 加小寫十六進位，不做其他處理；
--   - 其餘去頭尾空白、轉小寫。
--
-- 線路身分不是祕密：能讓 BNG 送出任意 Option 82 的人就能冒充任何一條線，所以 client 清單只列 BNG。
-- 要多一層，把 lock_mac 設成 1：第一次登入把 MAC 綁起來，之後換了 MAC 就拒絕。
--
-- 這個腳本是最上層腳本（沒有 [auth]），所以先處理協定與階段：同一台伺服器若也開了 [tacacs]，
-- 一律接受會授權每個指令。

if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end

local function line_id(value)
  if value == nil or value == '' then
    return nil
  end
  if string.find(value, '[^\32-\126]') then
    return '0x' .. (string.gsub(value, '.', function(c) return string.format('%02x', string.byte(c)) end))
  end
  local text = string.lower(string.match(value, '^%s*(.-)%s*$'))
  if text == '' then
    return nil
  end
  return text
end

local function mac_address(text)
  local hex = string.gsub(string.lower(text or ''), '[^0-9a-f]', '')
  return #hex == 12 and hex or nil
end

local circuit = line_id(get_attr('ADSL-Agent-Circuit-Id')) or line_id(get_attr('NAS-Port-Id'))
if not circuit then
  -- Access-Reject 不帶屬性，原因寫在記錄裡。
  log('line identity: no circuit-id and no NAS-Port-Id in the request')
  return 'reject'
end
local remote = line_id(get_attr('ADSL-Agent-Remote-Id')) or ''
local mac = mac_address(get_attr('Calling-Station-Id'))

local rows = sql_query([[SELECT l.subscriber, l.enabled, l.lock_mac, l.bound_mac, p.pool, p.profile, p.interim
                         FROM lines l LEFT JOIN plans p ON p.plan = l.plan
                         WHERE l.remote_id = ? AND l.circuit_id = ?]], {remote, circuit})
if #rows ~= 1 then
  log('line identity: unknown line', remote, circuit)
  return 'reject'
end
local line = rows[1]
if line.enabled ~= 1 then
  log('line identity: line', remote, circuit, 'of', line.subscriber, 'is disabled')
  return 'reject'
end
if line.pool == nil then
  log('line identity: the plan of line', remote, circuit, 'does not exist')
  return 'reject'
end

if line.lock_mac == 1 then
  if not mac then
    log('line identity: line', remote, circuit, 'is locked to a MAC and the request has none')
    return 'reject'
  end
  local bound = line.bound_mac
  if bound == nil then
    -- 第一次：只有 bound_mac 還是空的才寫得進去；兩個請求同時來，只有一個贏，另一個重讀後比較。
    local took = sql_execute('UPDATE lines SET bound_mac = ? WHERE remote_id = ? AND circuit_id = ? AND bound_mac IS NULL', {mac, remote, circuit})
    if took == 1 then
      bound = mac
    else
      local again = sql_query('SELECT bound_mac FROM lines WHERE remote_id = ? AND circuit_id = ?', {remote, circuit})
      bound = again[1] and again[1].bound_mac
    end
  end
  if bound ~= mac then
    log('line identity: line', remote, circuit, 'is locked to', bound, 'and the request has', mac)
    return 'reject'
  end
end

-- 記錄最後的 MAC 和時間；這一行每次登入寫一次，不需要時可以拿掉（手冊「已知限制」）。
-- 請求沒有 MAC 時只更新時間，不蓋掉上一次看到的 MAC。
if mac then
  sql_execute('UPDATE lines SET last_mac = ?, last_seen = ? WHERE remote_id = ? AND circuit_id = ?',
    {mac, math.floor(now_unix()), remote, circuit})
else
  sql_execute('UPDATE lines SET last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {math.floor(now_unix()), remote, circuit})
end

set_attr('Framed-Pool', line.pool)
set_attr('Filter-Id', line.profile)
set_attr('Acct-Interim-Interval', line.interim)
-- BNG 會在計費封包帶回 Class，計費紀錄就知道這條線是誰。
set_attr('Class', line.subscriber)
return 'accept'
