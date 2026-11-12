-- Hands out addresses from a pool, never the same one twice at a time, across the cluster.
--
--   CREATE TABLE ip_pool (ip TEXT PRIMARY KEY, holder TEXT, expires INTEGER NOT NULL DEFAULT 0);
--   INSERT INTO ip_pool (ip) VALUES ('10.0.0.1'), ('10.0.0.2');
--
-- Writes go through the cluster one at a time, so each sees the result of the one before,
-- and of two requests after the same address only one takes it.
--
-- A NAS can send a Stop again, so accounting only counts when it comes from the address's
-- holder, and the NAS must put User-Name in it. Nothing tells a holder's old session from its
-- new one, so a Stop sent again after the same user got the same address back still frees it.
-- That includes a Stop coming back from the accounting spool. Acct-Delay-Time keeps an old
-- Interim-Update from extending a lease as if it were recent.
--
-- This checks policy only, not passwords. Use it as [lua].script, which accounting always
-- goes to, and also as a `lua` handler after a `sql` or `ldap` handler in a chain with
-- policy = "all_accept", so that a login has to pass both.

-- How long an address stays taken without news from the NAS, which must send
-- Interim-Updates more often than this.
local LEASE = 900

local now = math.floor(now_unix())

local user = get_attr('User-Name')
-- A NAS can send an attribute with nothing in it, which is as good as none.
if user == '' then
  user = nil
end

if phase == 'accounting' then
  local address = get_attr('Framed-IP-Address')
  local status = get_attr('Acct-Status-Type')
  if user and address then
    if status == 1 or status == 3 then -- Start, Interim-Update
      local seen = now - math.floor(get_attr('Acct-Delay-Time') or 0)
      sql_execute("UPDATE ip_pool SET expires = ? WHERE ip = ? AND holder = ?", {seen + LEASE, address, user})
    elseif status == 2 then -- Stop
      sql_execute("UPDATE ip_pool SET holder = NULL, expires = 0 WHERE ip = ? AND holder = ?", {address, user})
    end
  end
  return 'accept'
end

if phase ~= 'authentication' then
  return 'accept'
end

-- Accounting could not be matched to a request with no name.
if not user then
  log('no address for a request without a user name')
  return 'reject'
end

-- This node's copy of the database can be behind, so what looks free here may be taken
-- already. The UPDATE settles it, and says whether it took the address. Reading the row back
-- after the UPDATE would not work, as this node may not have that write yet. Random order
-- keeps requests at the same moment from all going after the same address.
for _ = 1, 3 do
  local free = sql_query("SELECT ip FROM ip_pool WHERE expires <= ? ORDER BY random() LIMIT 8", {now})
  if #free == 0 then
    break
  end
  for _, row in ipairs(free) do
    local taken = sql_execute("UPDATE ip_pool SET holder = ?, expires = ? WHERE ip = ? AND expires <= ?",
      {user, now + LEASE, row.ip, now})
    if taken > 0 then
      set_attr('Framed-IP-Address', row.ip)
      return 'accept'
    end
  end
end
log('no address free for', user)
return 'reject'
