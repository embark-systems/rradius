-- Authentication goes through the sql handler in rradius.toml. This script records
-- accounting. Scripts must accept only once the record is stored: see docs/usage.md.
if phase ~= 'accounting' then
  return 'reject'
end
sql_execute("INSERT INTO accounting (username, session_id, status, received) VALUES (?, ?, ?, ?)", {
  get_attr('User-Name') or '',
  get_attr('Acct-Session-Id') or '',
  tostring(get_attr('Acct-Status-Type') or ''),
  math.floor(now_unix()),
})
return 'accept'
