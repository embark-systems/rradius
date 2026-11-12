-- 管理員和角色。role 由 tacacs.lua 解讀：'netadmin' 全部權限，'noc' 只能查看。
CREATE TABLE admins (
  username TEXT PRIMARY KEY,
  password TEXT,
  role TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 計費記錄：登入、登出和每個指令。同一筆送兩次只留一筆。
CREATE TABLE tacacs_log (
  device TEXT NOT NULL,          -- [[tacacs.clients]] 的 name
  task_id TEXT NOT NULL,
  event TEXT NOT NULL,           -- 'start'、'stop' 或 'update'
  username TEXT NOT NULL,
  rem_addr TEXT NOT NULL,        -- 管理員從哪裡連進設備
  cmd TEXT NOT NULL,             -- 指令全文；登入、登出是空字串
  logged INTEGER NOT NULL,
  PRIMARY KEY (device, task_id, event)
);
CREATE INDEX tacacs_log_user ON tacacs_log (username, logged);
