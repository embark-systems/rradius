//! A CA with a server and a client certificate, written as PEM files.
use rcgen::{BasicConstraints, CertificateParams, CertificateRevocationListParams, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose, RevocationReason, RevokedCertParams, SerialNumber};
use std::path::Path;

/// Writes `<ca>.pem` and returns what signs further certificates.
pub fn ca(dir: &Path, ca: &str) -> Issuer<'static, KeyPair> {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    std::fs::write(dir.join(format!("{ca}.pem")), params.clone().self_signed(&key).unwrap().pem()).unwrap();
    Issuer::new(params, key)
}

/// Writes `<name>.pem` and `<name>.key`, valid for `dns_name`, with this serial number.
pub fn named_leaf(dir: &Path, name: &str, dns_name: &str, serial: u64, issuer: &Issuer<'_, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![dns_name.to_string()]).unwrap();
    params.serial_number = Some(SerialNumber::from(serial));
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}

/// Like `named_leaf`, with this common name.
pub fn named_leaf_with_common_name(dir: &Path, name: &str, dns_name: &str, common_name: &str, serial: u64, issuer: &Issuer<'_, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![dns_name.to_string()]).unwrap();
    params.serial_number = Some(SerialNumber::from(serial));
    params.distinguished_name.push(rcgen::DnType::CommonName, common_name);
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}

/// With this common name, and a subject alternative name extension that cannot be read.
pub fn leaf_with_unreadable_alternative_names(dir: &Path, name: &str, common_name: &str, serial: u64, issuer: &Issuer<'_, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.serial_number = Some(SerialNumber::from(serial));
    params.distinguished_name.push(rcgen::DnType::CommonName, common_name);
    params.custom_extensions.push(rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 17], vec![0x30, 3, 0x82, 1, 0xff]));
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}

/// Writes `<name>.pem` and `<name>.key`, valid for this long from now.
pub fn leaf_valid_for(dir: &Path, name: &str, validity: std::time::Duration, issuer: &Issuer<'_, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    params.not_before = rcgen::date_time_ymd(1970, 1, 1) + (now - std::time::Duration::from_secs(3600));
    params.not_after = rcgen::date_time_ymd(1970, 1, 1) + (now + validity);
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}

/// Writes `<name>`, a CRL listing nothing, whose next update was due long ago.
pub fn expired_crl(dir: &Path, name: &str, issuer: &Issuer<'_, KeyPair>) {
    let params = CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2020, 1, 1),
        next_update: rcgen::date_time_ymd(2021, 1, 1),
        crl_number: SerialNumber::from(1u64),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    };
    std::fs::write(dir.join(name), params.signed_by(issuer).unwrap().pem().unwrap()).unwrap();
}

/// Writes `<name>`, a CRL listing these serial numbers.
pub fn crl(dir: &Path, name: &str, revoked: &[u64], issuer: &Issuer<'_, KeyPair>) {
    let params = CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2020, 1, 1),
        next_update: rcgen::date_time_ymd(2030, 1, 1),
        crl_number: SerialNumber::from(1u64),
        issuing_distribution_point: None,
        revoked_certs: revoked
            .iter()
            .map(|serial| RevokedCertParams {
                serial_number: SerialNumber::from(*serial),
                revocation_time: rcgen::date_time_ymd(2021, 1, 1),
                reason_code: Some(RevocationReason::KeyCompromise),
                invalidity_date: None,
            })
            .collect(),
        key_identifier_method: KeyIdMethod::Sha256,
    };
    std::fs::write(dir.join(name), params.signed_by(issuer).unwrap().pem().unwrap()).unwrap();
}

/// Writes `<name>.pem` and `<name>.key`, valid for `localhost`.
pub fn leaf(dir: &Path, name: &str, issuer: &Issuer<'_, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    std::fs::write(dir.join(format!("{name}.pem")), params.signed_by(&key, issuer).unwrap().pem()).unwrap();
    std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).unwrap();
}
