-- 示範資料，正式環境請刪掉這個檔案，改用自己的資料。
-- laptop-002 在資料表裡還是啟用的：擋下它的是撤銷清單。
INSERT INTO devices (name, vlan, enabled) VALUES
  ('laptop-001.corp.example.com', '10', 1),
  ('laptop-002.corp.example.com', '10', 1);
