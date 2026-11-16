-- 計費：一律回覆。位址屬性在認證時由 static.lua 回，這個範例不記錄計費。
-- 這是最上層腳本；同一台伺服器若也開了 [tacacs]，一律接受會授權每個指令，所以先擋掉。
if protocol ~= 'radius' then
  return 'reject'
end
return 'accept'
