-- TACACS+ 設備管理：依角色授權 shell 和指令，計費記下每個指令。
-- 認證由 [auth] 的 sql 處理器負責，這裡只做授權和計費。
--
--   netadmin：權限等級 15，所有指令
--   noc：權限等級 1，只能用 READ_ONLY 列出的指令，show 只能看 SHOW 列出的項目（看不到設定檔，裡面有密碼）

-- 這個伺服器只開 TACACS+。萬一之後加了 [radius]，也不要讓這個腳本放行任何人。
if protocol ~= 'tacacs' then
  return 'reject'
end

local user = get_attr('user')

if phase == 'accounting' then
  -- 設備每筆記錄都有 task_id；stop_time 表示結束，start_time 表示開始，其他是中間的更新。
  local event = (get_attr('stop_time') and 'stop') or (get_attr('start_time') and 'start') or 'update'
  -- 指令記錄把指令和參數分開送，組回一行。
  local cmd = get_attr('cmd') or ''
  for _, arg in ipairs(get_attrs('cmd-arg')) do
    if arg ~= '<cr>' then
      cmd = cmd .. ' ' .. arg
    end
  end
  sql_execute([[INSERT OR IGNORE INTO tacacs_log (device, task_id, event, username, rem_addr, cmd, logged)
                VALUES (?, ?, ?, ?, ?, ?, ?)]],
    {client, get_attr('task_id') or '', event, user or '', get_attr('rem_addr') or '', cmd, math.floor(now_unix())})
  return 'accept'
end

if phase ~= 'authorization' or not user or user == '' then
  return 'reject'
end

local rows = sql_query("SELECT role FROM admins WHERE username = ? AND enabled = 1", {user})
local role = rows[1] and rows[1].role
if not role or get_attr('service') ~= 'shell' then
  return 'reject'
end

local cmd = get_attr('cmd')
-- 沒有 cmd：要開 shell（EXEC 授權），回覆權限等級。
if not cmd or cmd == '' then
  set_attr('priv-lvl', role == 'netadmin' and 15 or 1)
  return 'accept'
end

if role == 'netadmin' then
  return 'accept'
end

-- noc：只放行列出來的指令。用允許清單而不是禁止清單：設定檔有很多種看法
-- （show running-config、show configuration、show archive …），列不完。
local READ_ONLY = { show = true, ping = true, traceroute = true, exit = true, terminal = true }
-- show 後面只能接這些，其他（包括所有看設定檔的寫法）都拒絕。
local SHOW = { interfaces = true, ip = true, ipv6 = true, version = true, clock = true, inventory = true,
  vlan = true, mac = true, arp = true, cdp = true, lldp = true, users = true, processes = true,
  logging = true, ['spanning-tree'] = true, etherchannel = true }
if not READ_ONLY[cmd] then
  return 'reject'
end
if cmd == 'show' and not SHOW[get_attrs('cmd-arg')[1] or ''] then
  return 'reject'
end
return 'accept'
