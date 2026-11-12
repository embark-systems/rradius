-- 認得的設備。MAC 位址一律 12 個小寫十六進位字元，不管交換器怎麼寫。
CREATE TABLE mac_devices (
  mac TEXT PRIMARY KEY,
  vlan TEXT NOT NULL,
  note TEXT                       -- 給人看的說明，腳本不讀
);
