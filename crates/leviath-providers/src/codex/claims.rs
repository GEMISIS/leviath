//! The useful claims inside a ChatGPT OAuth token.
//!
//! Both the id token and the access token are JWTs. Only the payload is read,
//! and only the signature-independent facts are taken from it: when the access
//! token expires, and which ChatGPT plan and account the grant belongs to.
//! Verifying the signature is the issuer's job, and this code has no key to do
//! it with; nothing here is a security decision, it is a display and scheduling
//! decision that the server re-checks on every request anyway.
//!
//! Every function returns an `Option` rather than a rich error. A claim that
//! cannot be read is not a failure the caller can act on differently: the
//! expiry falls back to "refresh reactively" and the plan falls back to "cannot
//! say", both of which are already required paths.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What the id token says about the account behind a grant.
///
/// `Debug` is derived deliberately: none of these are secrets, and they are
/// exactly what a debug line about a grant should show. The tokens themselves
/// live on [`super::store::ProviderGrant`], whose `Debug` redacts them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexClaims {
    /// The signed-in address, for `lev auth status`.
    pub email: Option<String>,
    /// `free`, `plus`, `pro`, `business`, `enterprise`, `edu`. Gates which
    /// models the account can reach.
    pub plan_type: Option<String>,
    /// The workspace this grant acts in. Sent as the `ChatGPT-Account-Id`
    /// header on every request.
    pub account_id: Option<String>,
    /// The ChatGPT user behind the grant.
    pub user_id: Option<String>,
    /// When the subscription lapses, as the server reported it.
    pub subscription_active_until: Option<String>,
}

/// The payload of a JWT, or `None` if it is not one.
fn payload(jwt: &str) -> Option<serde_json::Value> {
    // Destructured rather than pulled off an iterator: `split` always yields at
    // least one item, so taking the first with `?` would be a branch nothing
    // can drive.
    let parts: Vec<&str> = jwt.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return None;
    };
    // All three segments must be non-empty. A two-segment string is not a JWT,
    // and an empty payload decodes to nothing useful.
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The `exp` claim as a Unix timestamp in seconds.
///
/// `None` covers both "not a JWT" and "no expiry claim". The caller treats
/// both the same way: it cannot schedule a proactive refresh and falls back to
/// refreshing when a request comes back 401.
pub fn expiry(jwt: &str) -> Option<u64> {
    payload(jwt)?.get("exp")?.as_u64()
}

/// The account facts carried in an id token.
///
/// The interesting claims are namespaced under `https://api.openai.com/auth`,
/// with the address sometimes at the top level and sometimes under
/// `https://api.openai.com/profile`. Both are checked because both have been
/// observed.
pub fn parse(id_token: &str) -> CodexClaims {
    let Some(claims) = payload(id_token) else {
        return CodexClaims::default();
    };
    let auth = claims.get("https://api.openai.com/auth");
    let profile = claims.get("https://api.openai.com/profile");
    let string = |node: Option<&serde_json::Value>, key: &str| -> Option<String> {
        node?.get(key)?.as_str().map(str::to_string)
    };

    CodexClaims {
        email: claims
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| string(profile, "email")),
        plan_type: string(auth, "chatgpt_plan_type"),
        account_id: string(auth, "chatgpt_account_id"),
        user_id: string(auth, "chatgpt_user_id").or_else(|| string(auth, "user_id")),
        subscription_active_until: string(auth, "chatgpt_subscription_active_until"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a JWT-shaped string around `payload`. The header and signature are
    /// never inspected, so they only have to be present and non-empty.
    fn jwt(payload: serde_json::Value) -> String {
        format!(
            "aGVhZGVy.{}.c2ln",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    #[test]
    fn reads_the_expiry() {
        assert_eq!(
            expiry(&jwt(serde_json::json!({ "exp": 1_788_119_515u64 }))),
            Some(1_788_119_515)
        );
    }

    #[test]
    fn a_token_without_an_expiry_claim_has_none() {
        assert_eq!(expiry(&jwt(serde_json::json!({ "sub": "x" }))), None);
    }

    #[test]
    fn a_non_numeric_expiry_is_not_an_expiry() {
        // Guards the `as_u64` arm: a string here would otherwise panic on a
        // careless unwrap, and the honest answer is "cannot say".
        assert_eq!(expiry(&jwt(serde_json::json!({ "exp": "soon" }))), None);
    }

    #[test]
    fn strings_that_are_not_jwts_yield_nothing() {
        // Each of these fails at a different point in `payload`, so all of the
        // early-return arms are exercised.
        for not_a_jwt in [
            "",                  // no segments
            "only-one-part",     // no separators
            "two.parts",         // missing the signature
            ".body.sig",         // empty header
            "head..sig",         // empty payload
            "head.body.",        // empty signature
            "head.!!!not64.sig", // payload is not base64url
            &format!("head.{}.sig", URL_SAFE_NO_PAD.encode("not json")),
        ] {
            assert_eq!(expiry(not_a_jwt), None, "expiry of {not_a_jwt:?}");
            assert_eq!(
                parse(not_a_jwt),
                CodexClaims::default(),
                "parse of {not_a_jwt:?}"
            );
        }
    }

    #[test]
    fn reads_the_account_claims_a_real_login_returns() {
        // Shaped exactly like a measured id token, namespaced claim and all.
        let token = jwt(serde_json::json!({
            "email": "someone@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "0c77f491-84f6-4f73-81d4-a2937c9e94fb",
                "chatgpt_plan_type": "plus",
                "chatgpt_user_id": "user-wDzFPh8Tzluz8U3vnP4SNlcM",
                "chatgpt_subscription_active_until": "2026-09-30T19:41:11+00:00",
            },
        }));
        assert_eq!(
            parse(&token),
            CodexClaims {
                email: Some("someone@example.com".to_string()),
                plan_type: Some("plus".to_string()),
                account_id: Some("0c77f491-84f6-4f73-81d4-a2937c9e94fb".to_string()),
                user_id: Some("user-wDzFPh8Tzluz8U3vnP4SNlcM".to_string()),
                subscription_active_until: Some("2026-09-30T19:41:11+00:00".to_string()),
            }
        );
    }

    #[test]
    fn falls_back_to_the_profile_address_and_the_legacy_user_id() {
        let token = jwt(serde_json::json!({
            "https://api.openai.com/profile": { "email": "profile@example.com" },
            "https://api.openai.com/auth": { "user_id": "user-legacy" },
        }));
        let claims = parse(&token);
        assert_eq!(claims.email.as_deref(), Some("profile@example.com"));
        assert_eq!(claims.user_id.as_deref(), Some("user-legacy"));
    }

    #[test]
    fn a_token_with_no_auth_claim_still_parses() {
        // An id token from an issuer that namespaces nothing is not an error;
        // it just says nothing about the plan.
        let claims = parse(&jwt(serde_json::json!({ "email": "a@b.c" })));
        assert_eq!(claims.email.as_deref(), Some("a@b.c"));
        assert_eq!(claims.plan_type, None);
        assert_eq!(claims.account_id, None);
    }

    #[test]
    fn a_claim_of_the_wrong_type_is_ignored() {
        // `as_str` on a number returns None rather than stringifying it, which
        // is the behaviour that keeps a nonsense plan out of the catalog.
        let claims = parse(&jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_plan_type": 7 },
        })));
        assert_eq!(claims.plan_type, None);
    }
}
