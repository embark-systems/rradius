-- Administrators and roles. The role is interpreted by tacacs.lua: 'netadmin' has all privileges, 'noc' can only view.
CREATE TABLE admins (
  username TEXT PRIMARY KEY,
  password TEXT,
  role TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- Accounting records: logins, logouts and every command. The same record sent twice is kept once.
CREATE TABLE tacacs_log (
  device TEXT NOT NULL,          -- name of the [[tacacs.clients]] entry
  task_id TEXT NOT NULL,
  event TEXT NOT NULL,           -- 'start', 'stop' or 'update'
  username TEXT NOT NULL,
  rem_addr TEXT NOT NULL,        -- where the administrator connected to the device from
  cmd TEXT NOT NULL,             -- the full command; an empty string for login and logout
  logged INTEGER NOT NULL,
  PRIMARY KEY (device, task_id, event)
);
CREATE INDEX tacacs_log_user ON tacacs_log (username, logged);
