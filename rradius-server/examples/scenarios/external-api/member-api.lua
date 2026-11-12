-- 用會員系統的 API 驗證帳號密碼。
--
-- 這份腳本假設的 API（請換成你的系統實際提供的）：
--   POST https://members.example.com/v1/radius/login
--   Authorization: Bearer <API 金鑰>
--   Content-Type: application/x-www-form-urlencoded
--   內容：username=<帳號>&password=<密碼>
--   200：帳號密碼正確，可以上網；回應標頭 X-Plan 是方案名稱（選填）
--   401、403：帳號密碼不對，或不能上網
-- 腳本沒有 JSON 函式，所以請 API 用狀態碼和標頭回答。
local API = 'https://members.example.com/v1/radius/login'
local API_KEY = 'member-api-key-change-me'   -- 示範值。這個檔案只讓執行 rradius 的帳號讀得到

local user = get_attr('User-Name') or ''
local password = get_attr('User-Password') or ''
-- 只有 PAP 帶得出明文密碼；CHAP、MS-CHAPv2 這裡無法驗證。空的名稱或密碼不必問 API。
if user == '' or password == '' then
  return 'reject'
end

local response, problem = http_request{
  url = API,
  method = 'POST',
  headers = {
    Authorization = 'Bearer ' .. API_KEY,
    ['Content-Type'] = 'application/x-www-form-urlencoded',
  },
  -- 名稱和密碼來自使用者，一律先編碼。
  body = 'username=' .. url_encode(user) .. '&password=' .. url_encode(password),
}

-- API 沒有回答（逾時、連不上、太忙）：拒絕，並留下一行記錄。不要記錄密碼。
if not response then
  log('member API gave no answer:', problem)
  return 'reject'
end
if response.status ~= 200 then
  if response.status ~= 401 and response.status ~= 403 then
    log('member API answered', response.status)
  end
  return 'reject'
end

-- 方案名稱交給 NAS 當 Filter-Id，由 NAS 上同名的規則決定速率。只收看起來像名稱的值。
local plan = response.headers['x-plan']
if plan and plan:match('^[%w_-]+$') then
  set_attr('Filter-Id', plan)
end
set_attr('Session-Timeout', 14400)
return 'accept'
