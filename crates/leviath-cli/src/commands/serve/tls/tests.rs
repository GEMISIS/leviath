//! Tests for the HTTPS options.
//!
//! The fixture is a CA plus a leaf signed by it, not a single self-signed
//! certificate. That is the shape `mkcert` and `tailscale cert` produce, and
//! the only one rustls accepts: a self-signed `CA:TRUE` certificate presented
//! as a leaf is refused with `CaUsedAsEndEntity`, which is what the first
//! attempt at this fixture hit.
//!
//! These are *test* keys. They are meant to be public.

use super::*;

/// The leaf the test server presents, for `localhost` and `127.0.0.1`.
pub(in crate::commands::serve) const TEST_CERT: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIDQDCCAiigAwIBAgIUXd1FYsTOC1BJ/iYYlEpnrXlCieAwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPbGV2aWF0aC10ZXN0LWNhMCAXDTI2MDgwNzE3Mzk0OFoY\n\
DzIxMjYwNzE0MTczOTQ4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqG\n\
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQCgd9uh1tMXJluB9pgow9k7z+czIoQ6uyxj\n\
s6Az6qoGnCoWKi7bbBZqAXD5l+8XL2SJgWKpZK3jDBHxM2E1QcXHflcx9ckQ7srf\n\
KBi3U//gR50oDy89LbsnS+E4EqEJdnYjB8o8xO/NZEoMG7BQoNriBm2U1WwJc1wT\n\
Foo1Tn89dc3ElKNMhnn6yGMS4MSeO5ndmgswzC0b8QVRR3Q5+YLA6B0cxTCgJhgQ\n\
08fFy8eDC3Gfb2+Ngi5xM9s6y8cxyvN/txxpqGrxyUmyMfkuOHUrGCAJICYFEgJT\n\
xKro52uMWRFQWM/Y6og1PI7J5RewdIq46AS0qkPUW16rqA1Tbir3AgMBAAGjgYEw\n\
fzAMBgNVHRMBAf8EAjAAMBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATATBgNV\n\
HSUEDDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUswEtzuEJXpH9i/oi/DfN3flF63Aw\n\
HwYDVR0jBBgwFoAUmei4bzoEpByfyoDxT1M/rJj9AocwDQYJKoZIhvcNAQELBQAD\n\
ggEBACrXlonhJUaF6D+g/jRQLxBJDoU/XH0ozzHHApUtW2aKkjGq7h9aJvO31tRP\n\
GQdQ8SUdl76MjQtUpPMqMjvo2Z6nUfy3322rG5aSoNA/TIKr9lXVuPUhYNv4jAdy\n\
I2cdqk4QGa9VGAM/K0SiptN9yFgauJKcERvXIJS07h15AHYu5eaKgqgCIs6xoDE1\n\
RDsjyTAui8Q2yo4KYWd0Cl1cgNmYeD1Ah/EdodofqOCWw6CaamwXJKaBFRFbTu7r\n\
gtAVi+06SPY2st9XV6vVbEgiVO5/W+Sr+ZYbtuO1aPXy608fX2Uq5ZSVRMXe506f\n\
LQAU0yFPk6VjFvO3IkpsitaxhZY=\n\
-----END CERTIFICATE-----\n\
";

/// The private key for [`TEST_CERT`].
pub(in crate::commands::serve) const TEST_KEY: &str = "\
-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCgd9uh1tMXJluB\n\
9pgow9k7z+czIoQ6uyxjs6Az6qoGnCoWKi7bbBZqAXD5l+8XL2SJgWKpZK3jDBHx\n\
M2E1QcXHflcx9ckQ7srfKBi3U//gR50oDy89LbsnS+E4EqEJdnYjB8o8xO/NZEoM\n\
G7BQoNriBm2U1WwJc1wTFoo1Tn89dc3ElKNMhnn6yGMS4MSeO5ndmgswzC0b8QVR\n\
R3Q5+YLA6B0cxTCgJhgQ08fFy8eDC3Gfb2+Ngi5xM9s6y8cxyvN/txxpqGrxyUmy\n\
MfkuOHUrGCAJICYFEgJTxKro52uMWRFQWM/Y6og1PI7J5RewdIq46AS0qkPUW16r\n\
qA1Tbir3AgMBAAECggEAGzip7/be4U72+AGKh2PN3qkimdiRoNruqU0n8Jau2Cc2\n\
toLaZwubc8khzp15CDBYeEEUKRM0sk7yXj3ukBfDwtdKWGXPAYnYrWmCY9sijXvo\n\
i4qj41d2J7DmGFqEqfPID6I7KvrniSqpqwspakwXRX98qGJaDPJeXLiWontZ92VX\n\
IGJfrFS6y8nt093D0jO3+KEjnCfK87L6t62oFCaFZyBnAinzTW0Ur7pkbvfQIjhu\n\
bwbK1HAbuSD23XCR2jeX/MqlT52kSVNtmubNFZKcPVQ99bw+f+JQqJNCkL0CWXlf\n\
35R+MMdNTqmlMq7fVnvHhBs4E8FxKPTgxI9I3Ed/CQKBgQDZI1iY57jH6vUp4xxl\n\
KcNpOmu1UYX1pmwfwHLouOmDcog5CfK6kF13E4k03goh/qoNXEDn9ltCvFWVwljo\n\
+vNxd/B9ESADHFxEJA0SguPp9eqrbAnqMFuExLMyTuJtIDfjGlyAMYL7ov2E6Tmp\n\
AdQGg4zwniXy8zmro0ImIkhp3wKBgQC9MA7ZaT/5dLM8jZ8853Dxr44q1A//Zo2m\n\
GbFUwzFrXgmJlp3hXoPPkMC7YMeHDUvJ++6Hs0Tl2i3Fenzyb8bX5ll2M9c9bj/h\n\
G5mFOREmFkyRzPF6Q9ghr0Lzzm075IgIdaVp1zJa9f/PcIz9kQX+0SkywpteH0S6\n\
uaU0m0IR6QKBgEZcAbVqQKHnLJHqGaVeJwfN+mDCjdnPl3GidpmacXA6iJGS+6gg\n\
Z2jSV79dw4LIdmnl3tJLLb8uL71bQFweFQxLhQ3BotHfOraJyAKbjyacnPH3DC9q\n\
g/09j6NZlF0v92wLerW/VWYcpnGO8TQmd4G01tKRLFLRJXrMZ/7bVQOZAoGAZKVZ\n\
cP4WI66a397z1OHHazwa5Nv2OsgjGTdX6KEC/HyFlGXFTi0K8HSwo76jx0wigqz9\n\
Q8HyKFm+ue0k5ZDjdt47v69qlWq+nxIgxQgMAHgiefpOiN3o8FqdwriR0igM2ntD\n\
6Z+rUUrHsWLODuOFDf/V7AQtxY/a739tzSO/rWkCgYBJMVvtGA8RlK4m5FW0DOWn\n\
N54yKTNjjna8hlpU0OkDWr4ZwQE8/nA0Qjy5z+F5D9iJyMQXi9xYiIa9ujV3QLV3\n\
cos46d3Sft85iusSowLlsAYen9cJDD2/1ixWLexq/UGxZUhGgK1dczW6CaUzQNbr\n\
PBNcpgqZ15Ykb785enlsPA==\n\
-----END PRIVATE KEY-----\n\
";

/// The CA that signed [`TEST_CERT`]. A test client trusts this and
/// nothing else, so a successful handshake proves the server presented the
/// certificate rather than that verification was skipped.
pub(in crate::commands::serve) const TEST_CA: &str = "\
-----BEGIN CERTIFICATE-----\n\
MIIDFzCCAf+gAwIBAgIUanluj/0KYtGjT8cIEBr6AiU3N6cwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPbGV2aWF0aC10ZXN0LWNhMCAXDTI2MDgwNzE3Mzk0OFoY\n\
DzIxMjYwNzE0MTczOTQ4WjAaMRgwFgYDVQQDDA9sZXZpYXRoLXRlc3QtY2EwggEi\n\
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCpnMOJa+v8tC9WPYuDJRWJHdzu\n\
7OZWuLu/r9Ay7iVpzGIRcgjiswJxCBCvS2b1uWmmiJ+yYeX37WmiV3GczqeJ/UUK\n\
iLp132+5IWlJeVUDztPye+O1rePwMXh4IgwiulUvPqdprWNkwYaRodH2YJ494uyV\n\
Rd3hJjkaR6kGGE4uemWeaXMB+NhlLynZ47G80pL/u25byEKVTn5YVbf3tbJJZ5qH\n\
BgNWZfi3rZ9stJKaBVjn2UHqVQW3VRYLKQ2rCOHVelkHT+vnDII5gyZTJw85mzx/\n\
dzfMn07EZ+0ZYSdUwMs7YJOipFtfMVz/qXfbnvf/AYmZngrTwUTGJAJ7+65jAgMB\n\
AAGjUzBRMB0GA1UdDgQWBBSZ6LhvOgSkHJ/KgPFPUz+smP0ChzAfBgNVHSMEGDAW\n\
gBSZ6LhvOgSkHJ/KgPFPUz+smP0ChzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3\n\
DQEBCwUAA4IBAQBzV0LyKKAqYjtU4JEY+NXFC2yyXh+Fu/ziPKe9sHtuGaQbO6nn\n\
06mpJjLNexQTjll8yO76bpnkqqFGVrOr5+SeS02fcz/4mpTNT6tHDN3NNLCobEua\n\
vPKwTucTjrJBPs6m3DSOa3bDivnE03WC3kAE85TBFHjbZLo2QlPuy9GXFFyFoX/s\n\
vZJtL2jEkqivbF94pff+BrytJLQPllDc4bXfJwHN8b9QcWy/nSMqzRpJlypOw/qk\n\
sjq7fzKRZap1kLQ4zOmY05uCp68rHt1HzgW5FnPrpmANR5BAyv/bfxDsrA3luAuS\n\
TbB6I3BD/IpHkuDpC3XnWxvEU04E00uQRFxc\n\
-----END CERTIFICATE-----\n\
";

/// Write the fixture to disk and hand back the paths.
fn fixture(dir: &std::path::Path) -> TlsPaths {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, TEST_CERT).expect("write cert");
    std::fs::write(&key, TEST_KEY).expect("write key");
    TlsPaths { cert, key }
}

/// Both flags or neither. One alone is refused rather than falling back to
/// HTTP: falling back would start the server on a scheme the user did not ask
/// for, and they would find out from a mixed-content error in a browser on
/// another machine - the exact failure this feature exists to end.
#[test]
fn one_tls_flag_without_the_other_is_refused() {
    let cert = PathBuf::from("c.pem");
    let key = PathBuf::from("k.pem");

    assert_eq!(resolve(None, None).expect("neither is fine"), None);
    assert_eq!(
        resolve(Some(cert.clone()), Some(key.clone())).expect("both is fine"),
        Some(TlsPaths {
            cert: cert.clone(),
            key: key.clone()
        })
    );

    let missing_key = resolve(Some(cert), None).expect_err("cert alone is refused");
    let message = missing_key.to_string();
    assert!(message.contains("--tls-key"), "{message}");
    let missing_cert = resolve(None, Some(key)).expect_err("key alone is refused");
    let message = missing_cert.to_string();
    assert!(message.contains("--tls-cert"), "{message}");
}

/// The banner has to say what the server will actually answer on: the line it
/// prints is the URL a user copies into the console.
#[test]
fn the_scheme_follows_whether_tls_was_configured() {
    assert_eq!(scheme(None), "http");
    let paths = TlsPaths {
        cert: PathBuf::from("c"),
        key: PathBuf::from("k"),
    };
    assert_eq!(scheme(Some(&paths)), "https");
}

#[tokio::test]
async fn a_real_certificate_and_key_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    load(&fixture(dir.path())).await.expect("the fixture loads");
}

/// Failing at startup is the whole point: a server that binds and then rejects
/// every handshake looks like a network fault from the other machine.
#[tokio::test]
async fn an_unreadable_certificate_fails_before_the_server_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut paths = fixture(dir.path());
    paths.cert = dir.path().join("absent.pem");

    let err = load(&paths).await.expect_err("a missing certificate fails");
    // Names the file, because "invalid certificate" is unactionable when two
    // were supplied.
    let message = format!("{err:#}");
    assert!(message.contains("absent.pem"), "{message}");
}

#[tokio::test]
async fn a_malformed_certificate_fails_before_the_server_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = fixture(dir.path());
    std::fs::write(&paths.cert, "this is not a certificate").expect("write");

    let err = load(&paths)
        .await
        .expect_err("a malformed certificate fails");
    let message = format!("{err:#}");
    assert!(message.contains("cert.pem"), "{message}");
}

/// A key that is not the certificate's key is the failure that would otherwise
/// surface as a handshake error long after startup.
#[tokio::test]
async fn a_key_that_does_not_match_the_certificate_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = fixture(dir.path());
    // A syntactically valid PEM block that is not this certificate's key.
    std::fs::write(
        &paths.key,
        "-----BEGIN PRIVATE KEY-----\nbm90IGEga2V5\n-----END PRIVATE KEY-----\n",
    )
    .expect("write");

    assert!(load(&paths).await.is_err(), "a mismatched key must fail");
}
