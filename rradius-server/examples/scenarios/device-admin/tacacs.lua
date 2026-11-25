-- TACACS+ device administration: authorize shell and commands by role; accounting records every command.
-- Authentication is done by the sql handler in [auth]; this script only does authorization and accounting.
--
--   netadmin: privilege level 15, all commands
--   noc: privilege level 1, only the commands listed in READ_ONLY, and show only for the items listed in SHOW (the configuration file is off limits, it contains passwords)

-- This server only serves TACACS+. If [radius] is added later, this script still must not let anyone in.
if protocol ~= 'tacacs' then
  return 'reject'
end

local user = get_attr('user')

if phase == 'accounting' then
  -- Every record from the device has a task_id; stop_time means the end, start_time the start, anything else is an update in between.
  local event = (get_attr('stop_time') and 'stop') or (get_attr('start_time') and 'start') or 'update'
  -- A command record sends the command and its arguments separately; put them back together as one line.
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
-- No cmd: a shell is being opened (EXEC authorization); reply with the privilege level.
if not cmd or cmd == '' then
  set_attr('priv-lvl', role == 'netadmin' and 15 or 1)
  return 'accept'
end

if role == 'netadmin' then
  return 'accept'
end

-- noc: allow only the listed commands. Use an allow list rather than a deny list: there are many ways to view the configuration
-- (show running-config, show configuration, show archive ...) and they cannot all be listed.
local READ_ONLY = { show = true, ping = true, traceroute = true, exit = true, terminal = true }
-- Only these may follow show; everything else (including every way of viewing the configuration) is rejected.
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
