-- Policy for the three-node cluster test.
--   accounting:         record Acct-Session-Id through the cluster
--   User-Name "lookup": accept once the session written on node 2 is visible here
--   User-Name "lookup-quoted": the same for a session whose ID has a quote, written with a parameter
--   User-Name "lookup-later": the same for the session written while node 3 was down
--   User-Name "spooled-count": accept once the five sessions of the spool test are all here
--   User-Name "write":  accept only if a replicated write succeeds
if phase == 'accounting' then
  sql_execute("CREATE TABLE IF NOT EXISTS sessions (id TEXT)")
  sql_execute("INSERT INTO sessions VALUES (?)", {get_attr('Acct-Session-Id')})
  return 'accept'
end

local user = get_attr('User-Name')
if user == 'lookup' then
  local ok, rows = pcall(sql_query, "SELECT id FROM sessions WHERE id = 'from-node-2'")
  if ok and #rows > 0 then
    return 'accept'
  end
  return 'reject'
end
if user == 'lookup-later' then
  local ok, rows = pcall(sql_query, "SELECT id FROM sessions WHERE id = 'later-from-node-2'")
  if ok and #rows > 0 then
    return 'accept'
  end
  return 'reject'
end
if user == 'lookup-quoted' then
  local ok, rows = pcall(sql_query, "SELECT id FROM sessions WHERE id = ?", {"o'brien-from-node-2"})
  if ok and #rows > 0 then
    return 'accept'
  end
  return 'reject'
end
if user == 'spooled-count' then
  local ok, rows = pcall(sql_query, "SELECT count(*) AS n FROM sessions WHERE id LIKE 'spooled-%'")
  if ok and rows[1].n == 5 then
    return 'accept'
  end
  return 'reject'
end
if user == 'write' then
  sql_execute("CREATE TABLE IF NOT EXISTS probes (id TEXT)")
  return 'accept'
end
return 'reject'
