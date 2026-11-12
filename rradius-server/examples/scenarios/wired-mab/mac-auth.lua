-- MAC authentication, for devices that cannot do 802.1X: printers, phones, cameras. The switch
-- sends the device's MAC address as User-Name and in Calling-Station-Id.
--
--   CREATE TABLE mac_devices (mac TEXT PRIMARY KEY, vlan TEXT NOT NULL);
--   INSERT INTO mac_devices VALUES ('001122aabbcc', '100');
--
-- Addresses in the table are 12 lowercase hex digits, however the switch writes them.
--
-- A MAC address is not a secret: anyone who can read one off a device or the wire can use it. Put
-- these devices on a VLAN that can reach only what they need. There is no password to check, so a
-- request is accepted on the address alone, and a client that does 802.1X for other users should
-- send them to another chain. In a chain this is a `lua` handler; with users who have passwords
-- as well, check the address after the handler that checks them does not know the name.

-- Before anything else: as the top-level script of a server with [tacacs] too, a blanket accept
-- would authorize every command.
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end

local function mac_address(text)
  local hex = string.gsub(string.lower(text or ''), '[^0-9a-f]', '')
  return #hex == 12 and hex or nil
end

-- The name alone can be anything a device says: it has to be the station that is asking.
local mac = mac_address(get_attr('Calling-Station-Id'))
if not mac or mac ~= mac_address(get_attr('User-Name')) then
  return 'reject'
end

local rows = sql_query('SELECT vlan FROM mac_devices WHERE mac = ?', {mac})
if #rows ~= 1 then
  return 'reject'
end

set_attr('Tunnel-Type:1', 'VLAN')
set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
set_attr('Tunnel-Private-Group-Id:1', rows[1].vlan)
return 'accept'
