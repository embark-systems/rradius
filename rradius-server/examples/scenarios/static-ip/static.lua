-- 認證串接的 lua 處理器：依用戶回固定的位址、路由的子網段和 IPv6 前綴。
-- 前面的 sql 處理器已經驗過密碼；all_accept 遇到拒絕就停，所以密碼錯的人不會來到這裡。
--
-- 有 static_assignments 那一列的用戶：有哪個欄位就回哪個屬性。
--   ipv4       -> Framed-IP-Address，加 Framed-IP-Netmask（沒有設定時 255.255.255.255）
--   routed_v4  -> Framed-Route：<子網段> 0.0.0.0 1
--   wan_v6     -> Framed-IPv6-Prefix
--   pd_v6      -> Delegated-IPv6-Prefix
-- 沒有那一列（或那一列沒有位址、網段、前綴）的用戶：回 Framed-Pool，位址由 BNG 的池決定。
-- 欄位的值不合法（不是位址或前綴；netmask 和 routed_v4 腳本自己檢查）時拒絕並把用戶、欄位、值寫進記錄；不悄悄退回動態，
-- 否則付費要固定位址的用戶會拿到一個會變的位址，沒有人知道。

local INTERIM_SECONDS = 300

if phase ~= 'authentication' then
  return 'accept'
end
if protocol ~= 'radius' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

local subscriber = sql_query('SELECT pool FROM subscribers WHERE username = ? AND enabled = 1', {user})
if #subscriber == 0 then
  log('static ip: unknown or disabled user', user)
  return 'reject'
end

local rows = sql_query('SELECT ipv4, netmask, routed_v4, wan_v6, pd_v6 FROM static_assignments WHERE username = ?', {user})

-- set_attr 會依字典檢查值的格式；不合法時丟錯，這裡接住，記下是哪個欄位，然後拒絕。
local function put(name, column, value)
  local ok, err = pcall(set_attr, name, value)
  if not ok then
    log('static ip: bad', column, 'for', user, ':', value, '-', err)
  end
  return ok
end

local row = rows[1]
-- 沒有那一列，或那一列沒有任何位址、網段、前綴（只有 netmask 沒有 ipv4 也算沒有）：和沒有固定資料一樣。
if row == nil or not (row.ipv4 or row.routed_v4 or row.wan_v6 or row.pd_v6) then
  set_attr('Framed-Pool', subscriber[1].pool)
  return 'accept'
end

-- Framed-Route 和 Framed-IP-Netmask 的字典型別是字串／位址，set_attr 擋不住內容不對的值
-- （'garbage'、'255.0.255.0'），所以這兩個自己檢查。
local function octets(text)
  local a, b, c, d = string.match(text, '^(%d+)%.(%d+)%.(%d+)%.(%d+)')
  a, b, c, d = tonumber(a), tonumber(b), tonumber(c), tonumber(d)
  if not (a and b and c and d) or a > 255 or b > 255 or c > 255 or d > 255 then
    return nil
  end
  return a * 16777216 + b * 65536 + c * 256 + d
end

-- 子網段：a.b.c.d/n，n 在 0 到 32，主機位元是 0。
local function valid_subnet(text)
  local address, length = string.match(text, '^([%d%.]+)/(%d+)$')
  local value, bits = address and octets(address), tonumber(length)
  if not value or string.find(address, '^%d+%.%d+%.%d+%.%d+$') == nil or bits > 32 then
    return false
  end
  local host = 2 ^ (32 - bits)
  return value % host == 0
end

-- 子網路遮罩：1 連續在前面，0 連續在後面。
local function valid_netmask(text)
  local value = string.find(text, '^%d+%.%d+%.%d+%.%d+$') and octets(text)
  if not value then
    return false
  end
  local inverse = 4294967295 - value
  return bit32.band(inverse, inverse + 1) == 0
end

if row.netmask and not valid_netmask(row.netmask) then
  log('static ip: bad netmask for', user, ':', row.netmask)
  return 'reject'
end
if row.routed_v4 and not valid_subnet(row.routed_v4) then
  log('static ip: bad routed_v4 for', user, ':', row.routed_v4)
  return 'reject'
end
if row.ipv4 and not (put('Framed-IP-Address', 'ipv4', row.ipv4) and put('Framed-IP-Netmask', 'netmask', row.netmask or '255.255.255.255')) then
  return 'reject'
end
if row.routed_v4 and not put('Framed-Route', 'routed_v4', row.routed_v4 .. ' 0.0.0.0 1') then
  return 'reject'
end
if row.wan_v6 and not put('Framed-IPv6-Prefix', 'wan_v6', row.wan_v6) then
  return 'reject'
end
if row.pd_v6 and not put('Delegated-IPv6-Prefix', 'pd_v6', row.pd_v6) then
  return 'reject'
end
set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
return 'accept'
