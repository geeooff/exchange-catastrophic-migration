//! HTTP transport with NTLM/Negotiate authentication.
//!
//! The first implementation uses WinHTTP through Microsoft's `windows` crate, which
//! handles Negotiate and NTLM natively. Credentials come from the Windows Credential
//! Manager, never from files or the command line.
