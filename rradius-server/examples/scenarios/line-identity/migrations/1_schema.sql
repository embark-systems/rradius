-- 方案和線路。套用過的檔案不能再改，要改結構就新增下一個編號的檔案。

-- 每個方案：回給 BNG 的位址池名稱、設定檔名稱（速率等由 BNG 上同名的設定檔決定）、Interim 間隔。
CREATE TABLE plans (
  plan TEXT PRIMARY KEY,
  pool TEXT NOT NULL,
  profile TEXT NOT NULL,
  interim INTEGER NOT NULL DEFAULT 300
);

-- 每條線一列。主鍵是 (remote_id, circuit_id)：circuit-id 通常只在同一個接入設備內唯一，
-- remote-id 常是那個設備的名稱；BNG 沒送 remote-id 時用空字串。
-- 兩個 id 都存正規化後的值（見 auth.lua）：小寫、去頭尾空白，含不可印字元的寫成 0x 加十六進位。
--   lock_mac   0：只記錄最後看到的 MAC 和時間；1：第一次登入把 MAC 綁起來，之後不同的 MAC 拒絕。
--              換設備時把 bound_mac 清成 NULL。bound_mac 是 12 個小寫十六進位字元，沒有冒號或橫線。
CREATE TABLE lines (
  remote_id TEXT NOT NULL DEFAULT '',
  circuit_id TEXT NOT NULL,
  subscriber TEXT NOT NULL,
  plan TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  lock_mac INTEGER NOT NULL DEFAULT 0,
  bound_mac TEXT,
  last_mac TEXT,
  last_seen INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (remote_id, circuit_id)
);
