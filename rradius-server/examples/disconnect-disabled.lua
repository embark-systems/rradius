-- Ends the session of a user who has been disabled, when the NAS next reports on it.
--
--   CREATE TABLE disabled_users (username TEXT PRIMARY KEY);
--
-- Scripts only run on requests, so this takes effect at the next Interim-Update, not when
-- the row is added: how soon depends on the interim interval of the NAS.
--
-- `client` works when the client is configured with a single address. For a client
-- configured with a network, pass an address instead, such as get_attr('NAS-IP-Address').

-- A report coming back from the accounting spool is old news, about a session that may be gone.
if replayed or phase ~= 'accounting' or get_attr('Acct-Status-Type') ~= 3 then -- 3 is Interim-Update
  return 'accept'
end

local user = get_attr('User-Name')
local session = get_attr('Acct-Session-Id')
if not user or not session then
  return 'accept'
end

if #sql_query("SELECT 1 FROM disabled_users WHERE username = ?", {user}) > 0 then
  -- This holds up the Accounting-Response until the NAS answers, at worst for
  -- coa_timeout_ms * coa_attempts. Without an "ack" the next Interim-Update tries again,
  -- so the result needs no handling here.
  send_disconnect(client, { ['User-Name'] = user, ['Acct-Session-Id'] = session })
end

-- The accounting packet itself is still acknowledged: record it here as usual.
return 'accept'
