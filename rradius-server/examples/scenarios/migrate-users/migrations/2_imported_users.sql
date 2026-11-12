-- 示範：從舊系統匯入的帳號，密碼都是「<帳號>-password」。正式環境請刪掉這個檔案，
-- 換成自己匯出的資料。每一列的密碼值都是舊系統裡原本的雜湊，前面加上 {格式}。
INSERT INTO users (username, password, origin) VALUES
  -- 舊 LDAP 目錄的 userPassword，原樣複製
  ('ldap1', '{SSHA}ZvqZAfdyjd1PQiq8rurmdodN8ihkZW1vc2FsdA==', 'ldap'),
  ('ldap2', '{SSHA512}dgYg0PfH9neglxpRGmR3p4Crhif+xIPQ/AurUaFcjdsf8dogk1i2s9DiU+DeaMqnHZuZBZSExkKQ0pOuZpT9EmRlbW9zYWx0', 'ldap'),
  ('ldap3', '{PBKDF2-SHA256}10000$MDEyMzQ1Njc4OWFiY2RlZg$fesJONwdF0Xh3lhrasrq8MKhcVo5p23hOe4RkdXVGOI', 'ldap'),
  ('ldap4', '{X-PBKDF2}HMACSHA2+256:AAAnEA==:ZGVtb3NhbHQ=:9qxNguGzOOSXPzISiwG6JVifVK1DUTy95ow8P+VBTQI=', 'ldap'),
  -- /etc/shadow 的第二欄，前面加 {crypt}
  ('shell1', '{crypt}$1$demosalt$EhA8qeu4qSun8gh9DajmS1', 'shadow'),
  -- 舊郵件伺服器的目錄
  ('mail1', '{NS-MTA-MD5}0be637c7ffbaf0908fd571e4539b982dabcdefghijklmnopqrstuvwxyz012345', 'mail'),
  -- 舊資料庫存十六進位的 MD5：先轉成 Base64 再加 {MD5}（見手冊）
  ('old1', '{MD5}kfiBqWi4k+QJLsoSpSKT7w==', 'legacy-db'),
  -- 舊系統存 NT hash（MS-CHAPv2、PEAP 用得到的只有這種和明文）
  ('win1', '{nthash}9d232eea31049319fc8ab84300855a34', 'nt');

INSERT INTO user_reply (username, attribute, value) VALUES ('ldap1', 'Filter-Id', 'staff');
