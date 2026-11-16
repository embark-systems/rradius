-- 認證串接的 lua 處理器：依用戶的方案，回 BNG 看得懂的速率屬性。
-- 前面的 sql 處理器已經驗過密碼；all_accept 遇到拒絕就停，所以密碼錯的人不會來到這裡。
--
-- 同一個方案、不同廠商的 BNG 要的屬性不同。每個 BNG 是一個 RADIUS client，nas_styles 表說它用哪一種
-- 風格；沒有列的 client 用 profile。風格是下面 styles 表的一項，要支援別的廠商就加一項：
--   - 只送名稱的風格（profile、alc_sla、erx）：速率在 BNG 上、依名稱找到的設定檔裡設，單位和方向不在這裡。
--   - 送數值的風格（wispr、mikrotik）：單位和方向取自屬性的定義。請對照你的 BNG 的文件確認。
-- 下載（down）是往用戶的方向，上傳（up）是用戶往網路的方向；plans 的速率單位是 kbit/s。

local DEFAULT_STYLE = 'profile'
local INTERIM_SECONDS = 300

local styles = {
  -- 標準的 Filter-Id：值是 BNG 上的設定檔名稱。
  profile = function(plan)
    set_attr('Filter-Id', plan.profile)
  end,
  -- 無線網路營運商漫遊（WISPr）：兩個方向，單位 bit/s。屬性是 32 位元整數，最大 4294967295 bit/s
  -- （約 4.29 Gbit/s）；更快的方案送不出去，所以拒絕並記錄，不送一個被截斷或不合法的值。
  wispr = function(plan)
    local down, up = plan.down_kbps * 1000, plan.up_kbps * 1000
    if down > 4294967295 or up > 4294967295 then
      log('rate limit: plan', plan.plan, 'is faster than WISPr-Bandwidth can say (4294967295 bit/s)')
      return false
    end
    set_attr('WISPr-Bandwidth-Max-Down', down)
    set_attr('WISPr-Bandwidth-Max-Up', up)
  end,
  -- Mikrotik-Rate-Limit：「rx/tx」，從路由器看：rx 是用戶上傳、tx 是用戶下載；k 是 kbit/s。
  mikrotik = function(plan)
    set_attr('Mikrotik-Rate-Limit', string.format('%dk/%dk', plan.up_kbps, plan.down_kbps))
  end,
  -- SLA 設定檔名稱。
  alc_sla = function(plan)
    set_attr('Alc-SLA-Prof-Str', plan.profile)
  end,
  -- 進與出的政策名稱：進（Ingress）是用戶上傳，出（Egress）是用戶下載；名稱是設定檔名稱加 -up、-down。
  erx = function(plan)
    set_attr('ERX-Ingress-Policy-Name', plan.profile .. '-up')
    set_attr('ERX-Egress-Policy-Name', plan.profile .. '-down')
  end,
}

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

local rows = sql_query([[SELECT p.plan, p.down_kbps, p.up_kbps, p.profile
                         FROM subscribers s LEFT JOIN plans p ON p.plan = s.plan
                         WHERE s.username = ? AND s.enabled = 1]], {user})
if #rows ~= 1 then
  log('rate limit: unknown or disabled user', user)
  return 'reject'
end
local plan = rows[1]
if plan.plan == nil then
  log('rate limit: the plan of', user, 'does not exist')
  return 'reject'
end

local choice = sql_query('SELECT style FROM nas_styles WHERE client = ?', {client})
local style = choice[1] and choice[1].style or DEFAULT_STYLE
local apply = styles[style]
if not apply then
  log('rate limit: client', client, 'has the style', style, 'which this script does not know')
  return 'reject'
end

-- 風格函式回 false 表示這個方案送不出去；沒有回值就是送了。
if apply(plan) == false then
  return 'reject'
end
set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
return 'accept'
