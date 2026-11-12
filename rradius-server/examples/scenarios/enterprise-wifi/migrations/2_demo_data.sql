-- 示範資料，正式環境請刪掉這個檔案。
-- NT hash：printf '%s' 'staff1-password' | iconv -t UTF-16LE | openssl dgst -md4
-- （OpenSSL 3 要加 -provider legacy -provider default）
INSERT INTO accounts (name, kind, password, grp, enabled) VALUES
  ('staff1', 'user', '{nthash}eb046393145d207c556e275a62e46dfc', 'staff', 1),
  ('staff2', 'user', '{nthash}c3a36ac0282a6c9b466d1d12f4efea9b', 'staff', 0),
  ('laptop-001.corp.example.com', 'device', NULL, 'corp-devices', 1);

INSERT INTO group_reply (grp, attribute, value) VALUES
  ('staff', 'Tunnel-Type:1', 'VLAN'),
  ('staff', 'Tunnel-Medium-Type:1', 'IEEE-802'),
  ('staff', 'Tunnel-Private-Group-Id:1', '20'),
  ('corp-devices', 'Tunnel-Type:1', 'VLAN'),
  ('corp-devices', 'Tunnel-Medium-Type:1', 'IEEE-802'),
  ('corp-devices', 'Tunnel-Private-Group-Id:1', '10');
