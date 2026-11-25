-- Accounting: always reply. The rate attributes are returned at authentication by rate.lua; this example does not record accounting.
-- This is the top-level script; if the same server also has [tacacs], always accepting would authorize every command, so reject that first.
if protocol ~= 'radius' then
  return 'reject'
end
return 'accept'
