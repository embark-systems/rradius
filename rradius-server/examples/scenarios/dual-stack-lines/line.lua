-- 雙堆疊線路：用戶沒有密碼，BNG 轉送的 DHCP Option 82 說這是哪一條線（手冊第 16 章）。
-- 一個腳本做完四件事，順序是：
--   1. 線路身分：認得的、沒停用的、MAC 鎖（lock_mac）對得上的線路才往下（第 16 章）。
--   2. IPv4：線路有固定的 ipv4 就回固定的（第 17 章），沒有就從方案的 IPv4 池拿一個（第 14 章）。
--   3. IPv6：線路有固定的 wan_v6 或 pd_v6 就回固定的（第 17 章），沒有就從方案的前綴池拿一個（第 19 章）。
--   4. 前綴池用盡時，剛為這個請求預留的 IPv4 放回去，不留孤兒預留；然後預設拒絕（ALLOW_V4_ONLY 見下）。
--
-- 為什麼是一個腳本：認證串接的處理器之間不傳值（前一個處理器設的回覆屬性，後一個讀不到），兩個池腳本要
-- 各自重算「這是哪個用戶」，而且只有同一個腳本才能在前綴用盡時撤銷剛預留的 IPv4。
--
-- 池的鍵是 lines.subscriber（一條線一個用戶名稱）。回給 BNG 的 Class 也是它：BNG 在計費封包帶回 Class，
-- accounting.lua 靠它找到這條線，因為 IPoE 的計費封包沒有可靠的 User-Name。
--
-- 線路身分不是祕密：能讓 BNG 送出任意 Option 82 的人就能冒充任何一條線，所以 client 清單只列 BNG。

-- 前綴用盡時：false 整個登入拒絕（NAS 只看到 Access-Reject，原因寫在記錄）；
-- true 保留 IPv4、只回 IPv4 屬性，用戶沒有 IPv6 但能上網。
local ALLOW_V4_ONLY = false

-- 下面幾個數字要和 accounting.lua 一致。
local RESERVE_SECONDS = 120    -- 預留多久：Accounting-Start 沒來就放回
local HOLD_SECONDS = 86400     -- 黏著多久：位址放回後，原用戶在這段時間內優先拿回它
local REAP_SECONDS = 60        -- 請求觸發的回收間隔
-- 第 14 章（IPv4 池）：
local CANDIDATES = 8           -- 一次讀幾個空閒位址
local V4_ATTEMPTS = 4          -- 最多讀幾輪候選
-- 第 19 章（前綴池）：
local ATTEMPTS = 16            -- 搶輸了最多再試幾次
local WINDOW = 1024            -- 從起點往後最多看幾個 n 找空的
local STEAL_CHOICES = 8        -- 搶 released 時，第一次從最舊的幾列裡隨機挑

-- 這個最上層腳本之外沒有 [auth] 以外的處理器，所以先處理協定與階段：同一台伺服器若也開了 [tacacs]，
-- 一律接受會授權每個指令。
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end

local now = math.floor(now_unix())

---------------------------------------------------------------------------
-- 1. 線路身分（第 16 章）
---------------------------------------------------------------------------

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
  log('dual stack: no circuit-id and no NAS-Port-Id in the request')
  return 'reject'
end
local remote = line_id(get_attr('ADSL-Agent-Remote-Id')) or ''
local mac = mac_address(get_attr('Calling-Station-Id'))

local rows = sql_query([[SELECT l.subscriber, l.enabled, l.lock_mac, l.bound_mac,
                                l.ipv4, l.netmask, l.routed_v4, l.wan_v6, l.pd_v6,
                                p.pool, p.pool6, p.profile, p.interim
                         FROM lines l LEFT JOIN plans p ON p.plan = l.plan
                         WHERE l.remote_id = ? AND l.circuit_id = ?]], {remote, circuit})
if #rows ~= 1 then
  log('dual stack: unknown line', remote, circuit)
  return 'reject'
end
local line = rows[1]
if line.enabled ~= 1 then
  log('dual stack: line', remote, circuit, 'of', line.subscriber, 'is disabled')
  return 'reject'
end
if line.pool == nil then
  log('dual stack: the plan of line', remote, circuit, 'does not exist')
  return 'reject'
end
local user = line.subscriber

if line.lock_mac == 1 then
  if not mac then
    log('dual stack: line', remote, circuit, 'is locked to a MAC and the request has none')
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
    log('dual stack: line', remote, circuit, 'is locked to', bound, 'and the request has', mac)
    return 'reject'
  end
end

---------------------------------------------------------------------------
-- 固定資料的檢查（第 17 章）：欄位的值不合法時拒絕並記錄，不悄悄退回動態，
-- 否則付費要固定位址的用戶會拿到一個會變的位址，沒有人知道。
---------------------------------------------------------------------------

-- set_attr 會依字典檢查值的格式；不合法時丟錯，這裡接住，記下是哪個欄位。
local function put(name, column, value)
  local ok, err = pcall(set_attr, name, value)
  if not ok then
    log('dual stack: bad', column, 'for', user, ':', value, '-', err)
  end
  return ok
end

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

local fixed_v4 = line.ipv4 ~= nil
local fixed_v6 = line.wan_v6 ~= nil or line.pd_v6 ~= nil
if not fixed_v4 and (line.routed_v4 ~= nil or line.netmask ~= nil) then
  log('dual stack: line of', user, 'has routed_v4 or netmask but no ipv4')
  return 'reject'
end
if line.netmask and not valid_netmask(line.netmask) then
  log('dual stack: bad netmask for', user, ':', line.netmask)
  return 'reject'
end
if line.routed_v4 and not valid_subnet(line.routed_v4) then
  log('dual stack: bad routed_v4 for', user, ':', line.routed_v4)
  return 'reject'
end

---------------------------------------------------------------------------
-- 請求觸發的回收（第 14、19 章）：到期還沒確認的預留、沒收到 Interim-Update 的 session，放回。
-- 先讀本機副本判斷該不該跑，再用 UPDATE 搶一次，只有搶到的那一個去做，所以多個節點每個週期只做一次。
---------------------------------------------------------------------------

local function due(name)
  local rows = sql_query('SELECT 1 FROM housekeeping WHERE name = ? AND last_run < ?', {name, now - REAP_SECONDS})
  return #rows > 0 and sql_execute('UPDATE housekeeping SET last_run = ? WHERE name = ? AND last_run < ?', {now, name, now - REAP_SECONDS}) == 1
end

if not fixed_v4 and due('reap-ip-pool') then
  sql_execute([[UPDATE ip_pool SET state = 'free', last_user = username, username = NULL, session_id = NULL, nas = NULL,
                  expires = 0, released = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM ip_pool WHERE state IN ('reserved', 'active') AND expires < ?
                                ORDER BY rowid LIMIT 1000)]], {now, now, now})
end
if not fixed_v6 and due('reap-prefix-pool') then
  -- 沒收到 Interim 而過期的 active：改成 released，留給用戶 HOLD_SECONDS 秒；過期的 reserved、released：刪掉就是空閒。
  sql_execute([[UPDATE prefix_leases SET state = 'released', expires = ?, updated = ?
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state = 'active' AND expires < ? LIMIT 1000)]],
    {now + HOLD_SECONDS, now, now})
  sql_execute([[DELETE FROM prefix_leases
                WHERE rowid IN (SELECT rowid FROM prefix_leases WHERE state IN ('reserved', 'released') AND expires < ?
                                LIMIT 1000)]], {now})
end

---------------------------------------------------------------------------
-- 2. IPv4 位址池（第 14 章 allocate.lua 的函式版）。回傳 (位址, 是不是這個請求剛預留的)；沒有位址回傳 nil。
---------------------------------------------------------------------------

local function allocate_v4(pool)
  -- 已經有綁定（reserved 或 active）就沿用，不多佔一個。預留的延長；active 的由計費延長。
  local mine = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND username = ? AND state IN ('reserved', 'active') LIMIT 1", {pool, user})
  if #mine > 0 then
    sql_execute("UPDATE ip_pool SET expires = max(expires, ?), updated = ? WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved'",
      {now + RESERVE_SECONDS, now, pool, mine[1].ip, user})
    return mine[1].ip, false
  end

  -- 搶一個空閒的位址：只有它還是 free 才會改成功。
  local function claim(ip)
    return sql_execute([[UPDATE ip_pool SET state = 'reserved', username = ?, session_id = NULL, nas = NULL, expires = ?, updated = ?
                         WHERE pool = ? AND ip = ? AND state = 'free']], {user, now + RESERVE_SECONDS, now, pool, ip}) == 1
  end

  -- 黏著：他上次用過、現在空閒、而且放回不到 HOLD_SECONDS 的位址。
  local sticky = sql_query("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND last_user = ? AND released >= ? ORDER BY released DESC LIMIT 1",
    {pool, user, now - HOLD_SECONDS})
  if #sticky > 0 and claim(sticky[1].ip) then
    return sticky[1].ip, true
  end

  -- 從空閒的位址裡挑幾個來搶。起點在空閒位址的 n 範圍內隨機取，同時進來的請求才不會都搶同一批。
  -- LIMIT 直接寫進語句：它是這個腳本的常數，不是請求送來的值。
  local from_here = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' AND n >= ? ORDER BY n LIMIT %d", CANDIDATES)
  local from_start = string.format("SELECT ip FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT %d", CANDIDATES)
  for _ = 1, V4_ATTEMPTS do
    local lowest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n LIMIT 1", {pool})
    if #lowest == 0 then
      break
    end
    local highest = sql_query("SELECT n FROM ip_pool WHERE pool = ? AND state = 'free' ORDER BY n DESC LIMIT 1", {pool})
    local first = lowest[1].n
    local last = math.max(first, highest[1] and highest[1].n or first)
    local candidates = sql_query(from_here, {pool, first + math.random(0, last - first)})
    if #candidates == 0 then
      candidates = sql_query(from_start, {pool})
    end
    for _, candidate in ipairs(candidates) do
      if claim(candidate.ip) then
        return candidate.ip, true
      end
    end
  end

  log('ip pool', pool, 'is exhausted: no address for', user)
  return nil, false
end

-- 前綴池用盡時把剛預留的 IPv4 放回：只動這個請求剛預留、還沒被確認的那一列。
local function give_back_v4(pool, ip)
  sql_execute([[UPDATE ip_pool SET state = 'free', username = NULL, session_id = NULL, nas = NULL, expires = 0, updated = ?
                WHERE pool = ? AND ip = ? AND username = ? AND state = 'reserved' AND session_id IS NULL]], {now, pool, ip, user})
end

---------------------------------------------------------------------------
-- 3. IPv6 前綴池（第 19 章 allocate.lua 的函式版）。回傳前綴文字；沒有前綴回傳 nil。
---------------------------------------------------------------------------

local function allocate_prefix(pool)
  local function mine()
    return sql_query("SELECT prefix, state FROM prefix_leases WHERE username = ? AND pool = ?", {user, pool})
  end
  -- 已經有前綴就沿用：reserved、active 照舊；released（他離線了，前綴暫時留給他）改回 reserved。
  local have = mine()
  if #have > 0 then
    sql_execute("UPDATE prefix_leases SET state = 'reserved', expires = ?, updated = ? WHERE username = ? AND pool = ? AND state = 'released'",
      {now + RESERVE_SECONDS, now, user, pool})
    sql_execute("UPDATE prefix_leases SET expires = max(expires, ?), updated = ? WHERE username = ? AND pool = ? AND state = 'reserved'",
      {now + RESERVE_SECONDS, now, user, pool})
    local current = mine()
    if #current > 0 then
      return current[1].prefix
    end
    -- 剛好被搶走了：當作沒有，往下重新找一個。
  end

  local capacity_row = sql_query("SELECT (1 << (dlen - plen)) AS capacity FROM prefix_pools WHERE pool = ?", {pool})
  if #capacity_row == 0 then
    log('prefix pool', pool, 'of', user, 'is not in prefix_pools')
    return nil
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

  -- [from, upper) 裡的空段（連續的空 n）的第一個 n，最多 count 個，由小到大。
  local GAP = [[SELECT a.n + 1 AS n FROM prefix_leases a
                WHERE a.pool = ? AND a.n >= ? AND a.n < ?
                  AND NOT EXISTS (SELECT 1 FROM prefix_leases b WHERE b.pool = a.pool AND b.n = a.n + 1)
                ORDER BY a.n LIMIT ?]]

  local function free_in(from, upper, count)
    local found = {}
    if from >= upper or count < 1 then
      return found
    end
    if not taken(from) then
      found[1] = from
    end
    for _, row in ipairs(sql_query(GAP, {pool, from, upper - 1, count - #found})) do
      found[#found + 1] = row.n
    end
    return found
  end

  -- 前綴的高 64 位元是 base | (n << (64 - dlen))，在 SQL 裡算：Luau 沒有 64 位元整數。
  -- 文字是伺服器解碼 Delegated-IPv6-Prefix 的形式（壓縮、小寫）。
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

  -- 整個池裡從起點往後最多 count 個空段的第一個 n；窗口裡沒有空的、也沒有 released 可搶時用。
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

  -- 最舊的 released 列（離線最久的用戶的前綴）。
  local OLDEST = "SELECT n, username, updated FROM prefix_leases WHERE pool = ? AND state = 'released' ORDER BY updated LIMIT ?"
  -- 搶它：條件是它還是那個用戶的、還是 released、還是那一次釋放（updated 沒變）。
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
          return nil
        end
        won = claim(anywhere[math.random(#anywhere)])
      end
    end
    if won then
      local got = mine()
      if #got > 0 then
        return got[1].prefix
      end
    end
    -- 搶輸了：這個 n 剛被別人拿走，或這個用戶同時的另一個請求已經拿到前綴。
    local again = mine()
    if #again > 0 then
      return again[1].prefix
    end
    width = math.min(2 ^ attempt, 64)
  end

  log('prefix pool', pool, ': no prefix for', user, 'after', ATTEMPTS, 'tries: others took the free ones as fast')
  return nil
end

---------------------------------------------------------------------------
-- 組合：先 IPv4 再 IPv6（IPv6 失敗時才需要撤銷 IPv4）。
---------------------------------------------------------------------------

local v4, v4_fresh
-- 拒絕前把剛為這個請求預留的 IPv4 放回去，不留孤兒預留。
local function refuse()
  if v4 and v4_fresh then
    give_back_v4(line.pool, v4)
  end
  return 'reject'
end

if fixed_v4 then
  if not (put('Framed-IP-Address', 'ipv4', line.ipv4) and put('Framed-IP-Netmask', 'netmask', line.netmask or '255.255.255.255')) then
    return 'reject'
  end
  if line.routed_v4 and not put('Framed-Route', 'routed_v4', line.routed_v4 .. ' 0.0.0.0 1') then
    return 'reject'
  end
else
  v4, v4_fresh = allocate_v4(line.pool)
  if not v4 then
    return 'reject'
  end
end

local v6
if fixed_v6 then
  if line.wan_v6 and not put('Framed-IPv6-Prefix', 'wan_v6', line.wan_v6) then
    return refuse()
  end
  if line.pd_v6 and not put('Delegated-IPv6-Prefix', 'pd_v6', line.pd_v6) then
    return refuse()
  end
else
  v6 = allocate_prefix(line.pool6)
  -- 固定的 IPv4 也算「有 IPv4」：只固定 IPv4 的線路，前綴池用盡時也依 ALLOW_V4_ONLY 放行。
  if not v6 and (not ALLOW_V4_ONLY or not (fixed_v4 or v4)) then
    return refuse()
  end
end

-- 固定的屬性上面已經設過；池來的在這裡設。
if v4 then
  set_attr('Framed-IP-Address', v4)
end
if v6 then
  set_attr('Delegated-IPv6-Prefix', v6)
  -- WAN 的 /64 不在這裡分配：回池的名稱，由 BNG 自己的池決定。
  set_attr('Framed-IPv6-Pool', line.pool6)
end
set_attr('Filter-Id', line.profile)
set_attr('Acct-Interim-Interval', line.interim)
-- BNG 會在計費封包帶回 Class，accounting.lua 靠它知道這條線是誰。
set_attr('Class', user)

-- 記錄最後的 MAC 和時間；這一行每次登入寫一次，不需要時可以拿掉（手冊第 16 章「已知限制」）。
if mac then
  sql_execute('UPDATE lines SET last_mac = ?, last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {mac, now, remote, circuit})
else
  sql_execute('UPDATE lines SET last_seen = ? WHERE remote_id = ? AND circuit_id = ?', {now, remote, circuit})
end
return 'accept'
