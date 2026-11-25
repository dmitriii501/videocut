//! T-Bank (Tinkoff Kassa) acquiring integration, API v2.
//!
//! Flow:
//! 1. `init_payment` creates a payment server-side and returns a pay URL.
//! 2. The user is redirected to that URL and pays on T-Bank's page.
//! 3. T-Bank POSTs a signed notification to our webhook; we verify the token,
//!    re-check the state via `get_state`, and only then credit the account.
//!
//! The request token is SHA-256 over the values of all parameters except
//! `Token`, sorted by key, with the terminal password appended. The same rule
//! verifies inbound notifications.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};

use crate::config;
use crate::http;
use crate::utils::log;

/// Result of a successful `Init`.
pub struct InitResult {
    pub payment_id: String,
    pub payment_url: String,
}

/// Build the signature token for a set of fields.
///
/// `Token` itself is never part of the input; any nested/array values are
/// excluded by the caller (we only pass scalars here).
pub fn sign(fields: &BTreeMap<String, String>) -> Result<String> {
    let password = config::tbank_password()
        .ok_or_else(|| anyhow!("TBANK_PASSWORD не задан"))?;
    Ok(sign_with(fields, &password))
}

fn sign_with(fields: &BTreeMap<String, String>, password: &str) -> String {
    let mut concatenated = String::new();
    for (k, v) in fields.iter() {
        if k == "Token" {
            continue;
        }
        concatenated.push_str(v);
    }
    concatenated.push_str(password);

    let mut hasher = Sha256::new();
    hasher.update(concatenated.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Constant-time comparison for the notification token.
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn terminal_key() -> Result<String> {
    config::tbank_terminal_key().ok_or_else(|| anyhow!("TBANK_TERMINAL_KEY не задан"))
}

fn post(method: &str, mut fields: BTreeMap<String, String>) -> Result<serde_json::Value> {
    let token = sign(&fields)?;
    fields.insert("Token".to_string(), token);

    let url = format!("{}/{}", config::tbank_api_url(), method);
    let body = serde_json::to_value(&fields)?;
    let resp = http::post_json(&url, &[], &body, 30)?;

    if resp.get("Success").and_then(|v| v.as_bool()) == Some(true) {
        Ok(resp)
    } else {
        let code = resp
            .get("ErrorCode")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let msg = resp
            .get("Message")
            .or_else(|| resp.get("Details"))
            .and_then(|v| v.as_str())
            .unwrap_or("неизвестная ошибка");
        bail!("T-Bank {method} ErrorCode={code}: {msg}");
    }
}

/// Create a payment. `amount_kopecks` is the charge in kopecks.
pub fn init_payment(
    order_id: &str,
    amount_kopecks: i64,
    description: &str,
    notification_url: &str,
    success_url: &str,
    fail_url: &str,
) -> Result<InitResult> {
    let mut fields = BTreeMap::new();
    fields.insert("TerminalKey".to_string(), terminal_key()?);
    fields.insert("Amount".to_string(), amount_kopecks.to_string());
    fields.insert("OrderId".to_string(), order_id.to_string());
    fields.insert("Description".to_string(), description.to_string());
    fields.insert(
        "NotificationURL".to_string(),
        notification_url.to_string(),
    );
    fields.insert("SuccessURL".to_string(), success_url.to_string());
    fields.insert("FailURL".to_string(), fail_url.to_string());

    let resp = post("Init", fields)?;
    let payment_id = resp
        .get("PaymentId")
        .map(|v| v.to_string())
        .filter(|s| s != "null")
        .ok_or_else(|| anyhow!("Init: нет PaymentId в ответе"))?;
    let payment_url = resp
        .get("PaymentURL")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Init: нет PaymentURL в ответе"))?
        .to_string();
    let status = resp
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or("NEW")
        .to_string();

    log(format!(
        "T-Bank Init ok: order={order_id} payment={payment_id} status={status}"
    ));
    Ok(InitResult {
        payment_id,
        payment_url,
    })
}

/// Query the authoritative payment state from T-Bank.
pub fn get_state(payment_id: &str) -> Result<(String, bool)> {
    let mut fields = BTreeMap::new();
    fields.insert("TerminalKey".to_string(), terminal_key()?);
    fields.insert("PaymentId".to_string(), payment_id.to_string());

    let resp = post("GetState", fields)?;
    let status = resp
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let success = resp.get("Success").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok((status, success))
}

/// Verify the `Token` of an inbound notification form.
pub fn verify_notification(fields: &BTreeMap<String, String>) -> bool {
    let provided = match fields.get("Token") {
        Some(t) => t.clone(),
        None => return false,
    };
    match sign(fields) {
        Ok(expected) => constant_eq(&provided, &expected),
        Err(_) => false,
    }
}

/// Statuses that mean the money is actually captured.
pub fn status_is_paid(status: &str) -> bool {
    matches!(status, "CONFIRMED" | "AUTHORIZED")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_sha256_of_sorted_values_plus_password() {
        let mut f = BTreeMap::new();
        f.insert("TerminalKey".to_string(), "T".to_string());
        f.insert("OrderId".to_string(), "o1".to_string());
        f.insert("Amount".to_string(), "1000".to_string());
        // Sorted keys: Amount, OrderId, TerminalKey -> "1000o1T" + "p".
        assert_eq!(
            sign_with(&f, "p"),
            "b1f4fdd8d249f4cad849ac3cd73b8c306c25a1212a2b31b66575f1c0f1ea4551"
        );
    }

    #[test]
    fn paid_statuses() {
        assert!(status_is_paid("CONFIRMED"));
        assert!(status_is_paid("AUTHORIZED"));
        assert!(!status_is_paid("NEW"));
        assert!(!status_is_paid("REJECTED"));
    }
}
