//! SRP-6a client as Apple's `idmsa.apple.com` expects it (what the `srp` Python package does with
//! `rfc5054_enable()` + `no_username_in_x()`, SHA-256, 2048-bit group), with Apple's
//! `s2k` / `s2k_fo` password pre-hashing (SHA-256 then PBKDF2-HMAC-SHA256).

use hmac::{Hmac, KeyInit, Mac};
use num_bigint::BigUint;
use sha2::{Digest, Sha256};

const N_HEX: &str = "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";
const N_BYTES: usize = 256;

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("SRP group N")
}

fn g() -> BigUint {
    BigUint::from(2u32)
}

/// Minimal big-endian encoding (no leading zeros), like Python's `long_to_bytes`.
fn be(x: &BigUint) -> Vec<u8> {
    let v = x.to_bytes_be();
    if v == [0] { Vec::new() } else { v }
}

/// SHA-256 over the concatenation, each item left-padded with zeros to `width` when given.
fn h(parts: &[&[u8]], width: Option<usize>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for p in parts {
        if let Some(w) = width
            && p.len() < w
        {
            hasher.update(vec![0u8; w - p.len()]);
        }
        hasher.update(p);
    }
    hasher.finalize().to_vec()
}

/// PBKDF2-HMAC-SHA256.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(dk_len);
    let mut block: u32 = 1;
    while out.len() < dk_len {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(password).expect("hmac key");
        mac.update(salt);
        mac.update(&block.to_be_bytes());
        let mut u = mac.finalize().into_bytes().to_vec();
        let mut t = u.clone();
        for _ in 1..iterations {
            let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(password).expect("hmac key");
            mac.update(&u);
            u = mac.finalize().into_bytes().to_vec();
            for (a, b) in t.iter_mut().zip(&u) {
                *a ^= b;
            }
        }
        out.extend_from_slice(&t);
        block += 1;
    }
    out.truncate(dk_len);
    out
}

/// Apple's password derivation: `s2k` = PBKDF2(SHA256(pw)), `s2k_fo` = PBKDF2(hex(SHA256(pw))).
pub fn derive_password(password: &str, salt: &[u8], iterations: u32, protocol: &str) -> Result<Vec<u8>, String> {
    let digest = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = match protocol {
        "s2k" => digest.to_vec(),
        "s2k_fo" => digest.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes(),
        other => return Err(format!("unsupported SRP protocol `{other}`")),
    };
    Ok(pbkdf2_sha256(&input, salt, iterations, 32))
}

pub struct Client {
    username: String,
    a: BigUint,
    pub a_pub: BigUint,
}

pub struct Proof {
    pub m1: Vec<u8>,
    pub m2: Vec<u8>,
}

impl Client {
    /// Fresh client with a random 2048-bit private exponent (top bit set, like the Python library).
    pub fn new(username: &str) -> Result<Self, String> {
        let mut bytes = [0u8; N_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| format!("randomness unavailable: {e}"))?;
        bytes[0] |= 0x80;
        Ok(Self::with_secret(username, &bytes))
    }

    pub fn with_secret(username: &str, a_bytes: &[u8]) -> Self {
        let a = BigUint::from_bytes_be(a_bytes);
        let a_pub = g().modpow(&a, &n());
        Self { username: username.to_string(), a, a_pub }
    }

    /// Public ephemeral `A` as bytes.
    pub fn a_bytes(&self) -> Vec<u8> {
        be(&self.a_pub)
    }

    /// Process the server challenge (salt, B) with the pre-derived password; returns M1 and the expected M2.
    pub fn proof(&self, salt: &[u8], b_bytes: &[u8], derived_password: &[u8]) -> Result<Proof, String> {
        let n = n();
        let g = g();
        let b = BigUint::from_bytes_be(b_bytes);
        if (&b % &n) == BigUint::ZERO {
            return Err("SRP safety check failed (B mod N == 0)".into());
        }
        let k = BigUint::from_bytes_be(&h(&[&be(&n), &be(&g)], Some(N_BYTES)));
        let u = BigUint::from_bytes_be(&h(&[&be(&self.a_pub), &be(&b)], Some(N_BYTES)));
        if u == BigUint::ZERO {
            return Err("SRP safety check failed (u == 0)".into());
        }
        // x = H(salt | H(":" | derived_password))  (no username in x, the colon stays)
        let mut inner = vec![b':'];
        inner.extend_from_slice(derived_password);
        let x = BigUint::from_bytes_be(&h(&[salt, &h(&[&inner], None)], None));
        let v = g.modpow(&x, &n);
        // S = (B - k v)^(a + u x) mod N
        let kv = (&k * &v) % &n;
        let base = (&b + &n - kv) % &n;
        let exp = &self.a + &u * &x;
        let s = base.modpow(&exp, &n);
        let key = h(&[&be(&s)], None);
        // M1 = H( H(N) xor H(pad(g)), H(I), salt, A, B, K )
        let hn = Sha256::digest(be(&n));
        let hg = h(&[&be(&g)], Some(N_BYTES));
        let hnxorg: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        let hi = Sha256::digest(self.username.as_bytes());
        let m1 = h(&[&hnxorg, &hi, salt, &be(&self.a_pub), &be(&b), &key], None);
        let m2 = h(&[&be(&self.a_pub), &m1, &key], None);
        Ok(Proof { m1, m2 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Vectors produced with the Python `srp._pysrp` (rfc5054 + no_username_in_x) + pyicloud password derivation (a = 0x42*256, b = 0x17*256).
    const SALT: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    const B_S2K: &str = "58db3cf799e70a90e29c64448c72b61255039bf6e0fbf700439b4b08256b6930cf176952c78dbb73c7be4273eb380f00623a3dee2480119771fde20aa9de0ebad16ca8b52231f9a7f67faa8f5068f396e9737a39a9282db52a110e5d26b412f626b94abe9c8960a00a7856e1f4e8a563ed6d9fe5d96d01c1c32e9303b42b411dffca15ac29ce56673cce9b7a2c672b7aa539a4c94134322f39a2e9b42799f6ed1177c31b965334198703510ae8efa4706a48c155ac6ef7f750aa7853aa00c1e2e92a5071c72fef55b87bcbfb15b70f32019fb5dab82d04b51307ad0be97d8217040a62f9511e188361913a85020c83f2d71cbc3105bed91d85fef2ed50cb6db2";
    const B_S2K_FO: &str = "08dfb2c5ce7a07b4d89eed35e6309f3e2bcf9cfba65f5697ec0b37d543f71809960c0a3cdcbb21c5afb9c3841169ec521ad13bc738c70b0f423d31b40f2ac42576db0f51ddd2baf4449a740791336ee3155ed70778795c7249008704c884f46d83fbc3eaa2a82a27e8b38e464bfc62419b14076ad1c00fba8a70fccfe78d91f0e2fae0467645b9176ae6061fff9f55c711bc11bfe4898b5be89c6ed0539e382742f8af7884515f37eff85d12eac4387055ee5c39eaf057c4faee8f15f3156f53d511cbdc1af914a77c96b56468521dd2724191a62db4d20688c089ce37c30b96dd78fc52239d492b4eed948368b56c4aa891d5e9e25b3dbf2b3063b2d10f0381";

    #[test]
    fn password_derivation_matches_python() {
        assert_eq!(hex(&derive_password("p4ssw0rd!", &SALT, 20309, "s2k").unwrap()), "aa004e4a1b7b17fd75152e029cdfc7c9a16b3584b3af7f9c86f8291fb44b8df7");
        assert_eq!(hex(&derive_password("p4ssw0rd!", &SALT, 20309, "s2k_fo").unwrap()), "557bc5928e26d6f7c3040bfa05a03b0f25e6ade0a847413c44e32a7b324d94f0");
    }

    #[test]
    fn proofs_match_python() {
        let c = Client::with_secret("user@example.com", &[0x42u8; 256]);
        assert!(hex(&c.a_bytes()).starts_with("6e339d9add6c150d64e1231d8a21a4a2"));
        let pw = derive_password("p4ssw0rd!", &SALT, 20309, "s2k").unwrap();
        let p = c.proof(&SALT, &unhex(B_S2K), &pw).unwrap();
        assert_eq!(hex(&p.m1), "17da85fcb85cec59b129e5895a68df11a3361476117b1d31fb7e212b3db65593");
        assert_eq!(hex(&p.m2), "36a9c99f5e43e008b1fe51bc37d705b555c983e56b292e3b9e5943bd5f61d560");
        let pw = derive_password("p4ssw0rd!", &SALT, 20309, "s2k_fo").unwrap();
        let p = c.proof(&SALT, &unhex(B_S2K_FO), &pw).unwrap();
        assert_eq!(hex(&p.m1), "f1e66dbe55399b40429186118eeb0155a98849ac8be6fe6b92653cc7d84fd318");
        assert_eq!(hex(&p.m2), "cb1f184c15375415890bb3871cf4ca4b6bfd8f616f8c17f173701d0f3e816d09");
    }
}
