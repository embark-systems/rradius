-- 示範資料，正式環境請刪掉這個檔案。
-- 密碼欄是 ops1-password 的 SHA-512 crypt 雜湊（openssl passwd -6 產生）。
-- JBSWY3DPEHPK3PXP 是常見的示範金鑰，把它加進驗證器 App 就能產生驗證碼。
INSERT INTO mfa_users (username, password, totp_secret) VALUES ('ops1', '{crypt}$6$rradiusdemo$ieIZMv2GV9zanIl2gRuqVg1gQ6V8t41rAF2z4tPKeOpS3hw8kcJpsSxyTYM5VEaqDecWhZ0YG0yPYsQZtdBPi1', 'JBSWY3DPEHPK3PXP');
