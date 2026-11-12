-- Test policy for the integration tests: only "nemo" is accepted.
-- RADIUS replies to nemo also carry Filter-Id.
if phase == 'accounting' then
  return 'accept'
end

local user
if protocol == 'radius' then
  user = get_attr('User-Name')
else
  user = get_attr('user')
end

if user == 'nemo' then
  if protocol == 'radius' then
    set_attr('Filter-Id', 'std.ppp')
  end
  return 'accept'
end
return 'reject'
