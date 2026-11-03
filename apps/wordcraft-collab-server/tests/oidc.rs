//! C5.1: OIDC access tokens against a simulated identity provider (discovery + JWKS on a
//! loopback port), signed with throwaway test keys generated for this file only.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::routing::get;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use wordcraft_collab_server::auth::{self, oidc::Verifier};
use wordcraft_collab_server::persistence::{Database, repository};

/// The provider's signing key (PKCS#1 DER) and its public modulus as published in the JWKS.
const GOOD_KEY: &[&str] = &[
    "308204a402010002820101009f50d7e100c00176b7e49e72a3348b10d78d9f7cfd4380f11ca7d3f497457d31726912d7",
    "99ac092f6c97b744c1cb7de095a391025a5a54cc326a1bf8424d30ef4f9d2e8e634e4981a62273a46966d06b021a71f1",
    "c2780e87a884553923485c06ef95d436ab42f083e7827a2473b4400c5f5af2426b34cda7c5379157945711af1f012014",
    "80603f97ffd43183237f154d2f6e93fc35ea94bdcd5eba10df4e11f222a0cf82c0e6dbdf160d084a51cf985bbe3255bd",
    "de55096313274ef5995340184313471dea29d808d13548d29707fb50a665be8abece3cb2852c41a1abd867a8b8780da8",
    "64ab2d7628abc0fe03b6e780cebac3c96648963e20e4d8ff75ab437102030100010282010020597e946a1e12ce03f630",
    "b44f6e7e647b987ecf96efab918a038c16e8d68bdecc0008faed71d0e5a7d3c0bb7dec6f9178611fb34e1c7486882022",
    "b0cce9b6ab894b7de61cc50b48ffae9433c8d13f35d356025818685f6eed697d929e16f5d4d4b664427b911a9c761878",
    "ebc8f0cb6697c0345c073fd018a683ab88108f837fed043ec0a43a8c7c8964b308d77eab2b184c10ad4347b1f326e01d",
    "f4fde4eaf83d79c789e95f94ed3b6df5b59afd5978e116dfa32b79cb90b7eb3aca74785b56d85ef5942a465b09ae90e0",
    "f72199a73287ed50d130c66401b16a7c3ee1a54fd5c3807805dd321dca4cbb9977b48148a8c8de5e4d0b91bdba02faad",
    "48847b580902818100d815ab07f42710b0feac6493c2519bcee947fb2c5634762aa63336f1ef6e24d7ace33858bc1664",
    "ee424b5ccc20a9e8200bd37ecbb7b67d70f6a6759040d625fe612f02b18c7e9af21aae904205c24b143895ad59727e56",
    "519d7fb95d7c740be77eccfe9a51bef5db92c310177f8d3fd2208551faa1604e8fb525aefed6e8d02d02818100bcbea8",
    "767a67ef6d65fe224cb7e13bf043e0b06dd770b00ccda270b2f60849e1b60584b3c9c8ed6d3a4e85c473c937301872bc",
    "797e150e9d5a2b5d4600cc98128efe0e4987aec5b394d2465566e801bda72703d2926e2eff648a26f50913c891a795e3",
    "4142955c76cdc6a31ee6b72945dd9db788ebcf28f949d3ea818d8906d502818100d05b2c5c71ff0d282d995b2de2cc40",
    "19a67542eb40803b84ea572ddee148ea7b90785447b3ad404b415f5bae74bc80580e2f33ef09490cf1d4d3333f1255ab",
    "b2fa50931f52626dd38f08cd8eebf0a9e6689d62d68200f208a67e96e55ef0a2dde83590368b263ff9262db2a185db54",
    "33b029fd9f56e5ff3ab0a812722615bd4d028181008280c97672185f42231d5e95f126390d5dc183fbced4c9c570817a",
    "af22d284531e4a1e81396a465019daa53f3a57cc62e2e4a250d6ceb476341c7bb7a561d15c33a69236612c84e2306a5b",
    "db3cf5fa0e29ff1df380efd26b20172dec8b4434901171dfda05b1c46718ccd988ca39a6bdfa07cd194d02c5efe80e30",
    "bd3721a1a9028180181b2aec103598f7ef21db64fbef804df3d9b2d35dd8f0d5d796576ce372b500a69eed1039c34cbd",
    "8652fbf9dde7c4b08f2b5fcb633b86d4fb4f8f5cb2294fcd0372a37fd439c0ad596eae8403b9b0537057e3126e51154c",
    "59e0358658bf1c916f03af097e3559eb1124f70ed2d56c2dec560801ff7add487a414c6ca98ce359",
];
const GOOD_N: &str = "n1DX4QDAAXa35J5yozSLENeNn3z9Q4DxHKfT9JdFfTFyaRLXmawJL2yXt0TBy33glaORAlpaVMwyahv4Qk0w70-dLo5jTkmBpiJzpGlm0GsCGnHxwngOh6iEVTkjSFwG75XUNqtC8IPngnokc7RADF9a8kJrNM2nxTeRV5RXEa8fASAUgGA_l__UMYMjfxVNL26T_DXqlL3NXroQ304R8iKgz4LA5tvfFg0ISlHPmFu-MlW93lUJYxMnTvWZU0AYQxNHHeop2AjRNUjSlwf7UKZlvoq-zjyyhSxBoavYZ6i4eA2oZKstdiirwP4DtueAzrrDyWZIlj4g5Nj_datDcQ";
/// A key the provider never published.
const OTHER_KEY: &[&str] = &[
    "308204a20201000282010100b78eabbff35eb44e893f0a16d496d70ef1dab7f1a2ccf66b080da0f3fa961272999b01ea",
    "5b9273b98450e5aefabc128dbe88061415f377d06e08136682df456e5acb39ebabae0c7cbfe0de540feb2edf5429776e",
    "6f6d1448e32293f4234acc5d20fede613685d1afe3d72b47b4fe1dbd78a428d2d31c1dc5e4794e948e569c66fc4a0475",
    "9a7d0ced718b5adf5bac1097ffb6fae0078252c5d8011124d47e4eb328b99dddb9b081ff7e0e33b22d20e88fc6babfc7",
    "6851b98684d072672c5ceaa85e43b3062f04327812b74f23cee7215e641d5eb58fb34e7840ef26a5d4da32cd201a799d",
    "c9be082bc944e7a1145497d6854055f35fd56080164fe75f9d15f1d702030100010282010011c9baad24598621a23d40",
    "ae0bb5fe6b4d56c3c29fe33af20e5abbc14904680f41017b830992bcb23ab7583ac7c3980cb26d1e21ae2ae4ee86eb33",
    "6e18baa850d46da3544a53d893f24849635e138d7c48fca7bd45010e192e21b8b2cd04bf9f3ac65828958f0c4dfe4768",
    "c34f2fdd13f26ffe20c8228e64120b0b696c0dcaa42a431ed077a074b96bf0b41fee86cdddba28482fc9657348fd5bad",
    "3328ab49fc803f0531567bf213e15061ac892e2762592c892f8ff84f9a6aa2e6c3e9b5a755db513340993c01b5ff4aa2",
    "3138ca53b593360abfec9bdb48453175722ef43bb2014f0354f2dc4d693e5790c38e736cd1a536855684b32d70c825a7",
    "a9dfa3a33902818100dcf68559382c8dcb5f68b1d8d16f566b71afac6b72de48c375986fa4c6576c753c4438a28d2e37",
    "93a3d621eb78fdb1a47be4ec25111f5615300943c93b00ed979c05557f0bb8c48dce80dfd6fcb21ef3723b16c294f542",
    "770e8870f6cd924dc9983ad1ad60d0207e1507c28c477b3088eda32f725f0d2ce13d2a37bf3dbb9fb502818100d4a9c0",
    "fde4984ffa1924dfe677e84e20238866ad980463b7f2e3636e7c6b0f3c19b442598a0dc6b3247f4b95024ef788a27889",
    "be97364678c7528345c4cba43eb97e8dd361d4478036e2a76a5e3791adb2fa4429576161bc6945abd29083ceac57d337",
    "13976f72282bec3dbec5e068069b8460f327d02303d682719c4a024adb0281800834d7ab029d14ff12a20ec8e0ae63e5",
    "45a9a29bb2fe72715ff2b362c94301f230b4cfeda994a7d7c1e3f80a88c9d3492a98cbf46c785f7815faa21bdd2721ab",
    "d9cd90e9510421b8aa8cbac37f4b163f3064e56980a2c33f2cb900890a057f0217b747864aec2fe8bedb34ec84d31c70",
    "597310160d55adef079883cd847b6d69028180494be8d4994155ef6153304882f37259e9654dde6f12b43be65f311a17",
    "8fdec45c27a0e6b38a63fdc28a8d4662714d27aeb7d4e1f7c4212de7a52cd1932e8d01817578f46e37781d5ccd69de63",
    "72cba70e94eb814438b032fc25a33f03c82a27846dac01f8a3d4b8bbcac4cd47ee3c6c6a9589c4c4e81be96ab9b2e6a7",
    "2b7b19028180291bf6873e841accc8552652b32dcbf003bdfc33ac988670ca1f96ae629cc7d375a9d9f98566fa6671cd",
    "50c111c1afadc6a7f7892c9cd058ca1b2320d6e42a94b5c5e005a2d814c3d79e784b0e115fd2f2ae183fd2c8aa5eb425",
    "b568e28d225220eafa4558723d491afa6ff8e759a8947d26ddfe516cda50e1026ed31f74939d",
];

fn der(hex: &[&str]) -> Vec<u8> {
    let hex: String = hex.concat();
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

struct Provider {
    issuer: String,
    key_fetches: Arc<AtomicUsize>,
}

/// A provider at `http://127.0.0.1:<port>/application/o/wordcraft/` (Authentik's layout),
/// publishing one RS256 key `k1`. `claimed_issuer` is what its discovery document says.
async fn provider(claimed_issuer: Option<&str>) -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let issuer = format!("{base}/application/o/wordcraft/");
    let key_fetches = Arc::new(AtomicUsize::new(0));
    let discovery = json!({"issuer": claimed_issuer.unwrap_or(&issuer), "jwks_uri": format!("{base}/application/o/wordcraft/jwks/")});
    let fetches = key_fetches.clone();
    let app =
        axum::Router::new().route("/application/o/wordcraft/.well-known/openid-configuration", get(move || async move { Json(discovery) })).route(
            "/application/o/wordcraft/jwks/",
            get(move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                Json(json!({"keys": [{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": GOOD_N, "e": "AQAB"}]}))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    Provider { issuer, key_fetches }
}

fn sign(claims: &Value, key: &[&str], kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = kid.map(str::to_string);
    encode(&header, claims, &EncodingKey::from_rsa_der(&der(key))).unwrap()
}

fn claims(issuer: &str) -> Value {
    json!({"iss": issuer, "aud": "wordcraft", "sub": "authentik-user-1", "exp": now() + 300, "iat": now(), "name": "Ana López"})
}

/// `authenticate` off the async threads (it may fetch keys with blocking HTTP).
async fn sign_in(db: &Arc<Database>, verifier: &Arc<Verifier>, token: &str) -> Option<wordcraft_collab_server::persistence::UserId> {
    let (db, verifier, header) = (db.clone(), verifier.clone(), format!("Bearer {token}"));
    tokio::task::spawn_blocking(move || auth::authenticate(&db, Some(&verifier), Some(&header)).unwrap()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_token_signs_in_and_creates_its_user_once() {
    let p = provider(None).await;
    let db = Arc::new(Database::open_in_memory().unwrap());
    let verifier = Arc::new(Verifier::new(&p.issuer, "wordcraft"));
    let token = sign(&claims(&p.issuer), GOOD_KEY, Some("k1"));
    let first = sign_in(&db, &verifier, &token).await.expect("signed in");
    assert_eq!(sign_in(&db, &verifier, &token).await, Some(first));
    let user = repository::user_by_subject(&db.lock().unwrap(), "authentik-user-1").unwrap().unwrap();
    assert_eq!((user.id, user.display_name.as_str()), (first, "Ana López"));
    // Keys were fetched once and then cached.
    assert_eq!(p.key_fetches.load(Ordering::SeqCst), 1);
    // Local tokens keep working beside OIDC.
    let (local, local_token) = auth::create_local_user(&db, "Bea").unwrap();
    assert_eq!(sign_in(&db, &verifier, &local_token).await, Some(local));
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_that_do_not_check_out_are_refused() {
    let p = provider(None).await;
    let db = Arc::new(Database::open_in_memory().unwrap());
    let verifier = Arc::new(Verifier::new(&p.issuer, "wordcraft"));
    let with = |change: &dyn Fn(&mut Value)| {
        let mut c = claims(&p.issuer);
        change(&mut c);
        sign(&c, GOOD_KEY, Some("k1"))
    };
    let refused = [
        ("another issuer", with(&|c| c["iss"] = json!("https://evil.example/"))),
        ("another audience", with(&|c| c["aud"] = json!("someone-else"))),
        ("expired", with(&|c| c["exp"] = json!(now() - 3600))),
        ("not valid yet", with(&|c| c["nbf"] = json!(now() + 3600))),
        (
            "no subject",
            with(&|c| {
                c.as_object_mut().unwrap().remove("sub");
            }),
        ),
        (
            "no expiry",
            with(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
        ),
        ("signed with a key the provider never published", sign(&claims(&p.issuer), OTHER_KEY, Some("k1"))),
        ("tampered payload", {
            let token = sign(&claims(&p.issuer), GOOD_KEY, Some("k1"));
            let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
            let forged = sign(&json!({"iss": p.issuer, "aud": "wordcraft", "sub": "admin", "exp": now() + 300}), GOOD_KEY, Some("k1"));
            parts[1] = forged.split('.').nth(1).unwrap().to_string();
            parts[2] = token.split('.').nth(2).unwrap().to_string();
            parts.join(".")
        }),
        // HMAC with the public modulus as the secret: algorithm confusion.
        ("an HMAC algorithm", encode(&Header::new(Algorithm::HS256), &claims(&p.issuer), &EncodingKey::from_secret(GOOD_N.as_bytes())).unwrap()),
        ("unsigned", format!("{}.{}.", "eyJhbGciOiJub25lIn0", sign(&claims(&p.issuer), GOOD_KEY, None).split('.').nth(1).unwrap())),
        ("junk", "not.a.jwt".into()),
        ("huge", "a".repeat(64 * 1024)),
    ];
    for (why, token) in refused {
        assert_eq!(sign_in(&db, &verifier, &token).await, None, "{why}");
    }
    assert!(repository::user_by_subject(&db.lock().unwrap(), "admin").unwrap().is_none());
    // No user was created along the way.
    assert!(repository::user_by_subject(&db.lock().unwrap(), "authentik-user-1").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_key_ids_refetch_at_most_once_a_minute() {
    let p = provider(None).await;
    let db = Arc::new(Database::open_in_memory().unwrap());
    let verifier = Arc::new(Verifier::new(&p.issuer, "wordcraft"));
    for kid in ["k9", "k10", "k11"] {
        assert_eq!(sign_in(&db, &verifier, &sign(&claims(&p.issuer), GOOD_KEY, Some(kid))).await, None);
    }
    assert_eq!(p.key_fetches.load(Ordering::SeqCst), 1, "forged key ids must not hammer the provider");
    // The real key is in the cache: no further fetch.
    assert!(sign_in(&db, &verifier, &sign(&claims(&p.issuer), GOOD_KEY, Some("k1"))).await.is_some());
    assert_eq!(p.key_fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_naming_another_issuer_or_no_oidc_signs_nobody_in() {
    let p = provider(Some("https://elsewhere.example/")).await;
    let db = Arc::new(Database::open_in_memory().unwrap());
    let verifier = Arc::new(Verifier::new(&p.issuer, "wordcraft"));
    let token = sign(&claims(&p.issuer), GOOD_KEY, Some("k1"));
    assert_eq!(sign_in(&db, &verifier, &token).await, None);
    assert_eq!(p.key_fetches.load(Ordering::SeqCst), 0, "keys are never taken from a mismatched discovery document");
    // Without OIDC configured, a JWT is just an unknown token.
    let header = format!("Bearer {token}");
    let db2 = db.clone();
    let user = tokio::task::spawn_blocking(move || auth::authenticate(&db2, None, Some(&header)).unwrap()).await.unwrap();
    assert_eq!(user, None);
}
