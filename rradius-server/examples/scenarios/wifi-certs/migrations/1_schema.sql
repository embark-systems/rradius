-- 公司配發的設備。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。
CREATE TABLE devices (
  name TEXT PRIMARY KEY,             -- 憑證上的名稱，例如 laptop-001.corp.example.com
  vlan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1
);

-- 計費，和快速開始相同。
CREATE TABLE accounting (
  client TEXT NOT NULL,
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT,
  started INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  stopped INTEGER,
  session_time INTEGER NOT NULL DEFAULT 0,
  input_octets INTEGER NOT NULL DEFAULT 0,
  output_octets INTEGER NOT NULL DEFAULT 0,
  terminate_cause INTEGER,
  PRIMARY KEY (client, nas, session_id)
);
CREATE INDEX accounting_user ON accounting (username);
