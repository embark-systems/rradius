-- Writes one row for each accounting request; everyone else is accepted.
if phase == 'accounting' then
  local id = get_attr('Acct-Session-Id') .. '-' .. tostring(get_attr('Acct-Status-Type'))
  sql_execute(string.format("INSERT INTO bench_acct (id, ts) VALUES ('%s', %d)", id, now_unix()))
end
return 'accept'
