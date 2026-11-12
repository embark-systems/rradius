-- Computes for about 30 ms in each accounting request and does nothing else; everyone
-- else is accepted at once. The loop count sets the time: adjust it for another machine.
if phase == 'accounting' then
  local total = 0
  for i = 1, 3400000 do
    total = total + i % 7
  end
  if total < 0 then
    return 'reject'
  end
end
return 'accept'
