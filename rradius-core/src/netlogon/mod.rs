//! Verifying MS-CHAPv2 responses with an Active Directory domain controller over a Netlogon
//! secure channel (MS-NRPC), for the `netlogon` authentication handler.
pub mod crypto;
pub mod epm;
pub mod ndr;
pub mod rpc;
pub mod channel;
pub mod logon;
pub mod handler;
