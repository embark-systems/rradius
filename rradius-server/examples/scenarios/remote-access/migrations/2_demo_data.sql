-- 示範資料，正式環境請刪掉這個檔案。
-- JBSWY3DPEHPK3PXP 是常見的示範金鑰，把它加進驗證器 App 就能產生驗證碼。
INSERT INTO mfa_users (username, password, totp_secret) VALUES ('ops1', 'ops1-password', 'JBSWY3DPEHPK3PXP');
