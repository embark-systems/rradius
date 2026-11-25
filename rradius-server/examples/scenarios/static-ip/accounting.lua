-- Accounting: always reply. The address attributes are returned at authentication by static.lua; this example does not record accounting.
-- This is the top-level script; if the same server also has [tacacs], always accepting would authorize every command, so reject that first.
if protocol ~= 'radius' then
  return 'reject'
end
return 'accept'
