//! Content digests: SHA-256, SHA-1, and BLAKE3.
//!
//! REAPI identifies blobs by `(hash, size_bytes)` where `hash` is lowercase
//! hex. Newer clients also send an explicit `DigestFunction`; older ones
//! (and all ByteStream resource names) carry no function tag, so this module
//! resolves it by hash length and, where ambiguous (SHA-256 and BLAKE3 are
//! both 32 bytes), by trial verification against the bytes.

use std::fmt;

/// Supported digest functions. Wire values match
/// `build.bazel.remote.execution.v2.DigestFunction.Value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DigestFunction {
    Sha256,
    Sha1,
    Blake3,
}

impl DigestFunction {
    /// Longest-first ordered candidates for a lowercase-hex hash of `len` chars.
    pub fn by_hex_len(len: usize) -> &'static [DigestFunction] {
        match len {
            64 => &[DigestFunction::Sha256, DigestFunction::Blake3],
            40 => &[DigestFunction::Sha1],
            _ => &[],
        }
    }

    pub fn from_proto(v: i32) -> Option<DigestFunction> {
        match v {
            1 => Some(DigestFunction::Sha256),
            2 => Some(DigestFunction::Sha1),
            9 => Some(DigestFunction::Blake3),
            _ => None,
        }
    }

    pub fn to_proto(self) -> i32 {
        match self {
            DigestFunction::Sha256 => 1,
            DigestFunction::Sha1 => 2,
            DigestFunction::Blake3 => 9,
        }
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            DigestFunction::Sha256 => "sha256",
            DigestFunction::Sha1 => "sha1",
            DigestFunction::Blake3 => "blake3",
        }
    }

    pub fn from_dir_name(s: &str) -> Option<DigestFunction> {
        match s {
            "sha256" => Some(DigestFunction::Sha256),
            "sha1" => Some(DigestFunction::Sha1),
            "blake3" => Some(DigestFunction::Blake3),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<DigestFunction> {
        Self::from_dir_name(s)
    }

    pub fn hex_len(self) -> usize {
        match self {
            DigestFunction::Sha256 => 64,
            DigestFunction::Sha1 => 40,
            DigestFunction::Blake3 => 64,
        }
    }

    pub fn hexdigest(self, data: &[u8]) -> String {
        encode_hex(&raw_digest(self, data))
    }

    /// True iff `data` hashes to `expected_hex` under this function.
    /// Comparison runs over the decoded bytes with a constant-time equality
    /// check so a wrong guess reveals nothing about the stored hash.
    pub fn verify(self, data: &[u8], expected_hex: &str) -> bool {
        let Some(expected) = decode_hex(expected_hex) else {
            return false;
        };
        let actual = raw_digest(self, data);
        actual.len() == expected.len()
            && ring::constant_time::verify_slices_are_equal(&actual, &expected).is_ok()
    }

    /// Hex of the empty string, for synthesizing the conventional empty blob.
    pub fn empty_hex(self) -> String {
        self.hexdigest(&[])
    }
}

impl fmt::Display for DigestFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.dir_name())
    }
}

fn raw_digest(function: DigestFunction, data: &[u8]) -> Vec<u8> {
    match function {
        DigestFunction::Sha256 => ring::digest::digest(&ring::digest::SHA256, data)
            .as_ref()
            .to_vec(),
        DigestFunction::Sha1 => ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data)
            .as_ref()
            .to_vec(),
        DigestFunction::Blake3 => blake3::hash(data).as_bytes().to_vec(),
    }
}

/// Incremental hasher for ByteStream uploads, whose bytes arrive in chunks.
pub enum Hasher {
    Sha256(ring::digest::Context),
    Sha1(ring::digest::Context),
    Blake3(blake3::Hasher),
}

impl Hasher {
    pub fn new(function: DigestFunction) -> Hasher {
        match function {
            DigestFunction::Sha256 => Hasher::Sha256(ring::digest::Context::new(&ring::digest::SHA256)),
            DigestFunction::Sha1 => {
                Hasher::Sha1(ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY))
            }
            DigestFunction::Blake3 => Hasher::Blake3(blake3::Hasher::new()),
        }
    }

    pub fn function(&self) -> DigestFunction {
        match self {
            Hasher::Sha256(_) => DigestFunction::Sha256,
            Hasher::Sha1(_) => DigestFunction::Sha1,
            Hasher::Blake3(_) => DigestFunction::Blake3,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(ctx) => ctx.update(data),
            Hasher::Sha1(ctx) => ctx.update(data),
            Hasher::Blake3(hasher) => {
                hasher.update(data);
            }
        }
    }

    pub fn finish_hex(self) -> String {
        let raw: Vec<u8> = match self {
            Hasher::Sha256(ctx) => ctx.finish().as_ref().to_vec(),
            Hasher::Sha1(ctx) => ctx.finish().as_ref().to_vec(),
            Hasher::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
        };
        encode_hex(&raw)
    }

    pub fn verify(self, expected_hex: &str) -> bool {
        let Some(normalized) = normalize_hash(expected_hex) else {
            return false;
        };
        let actual = self.finish_hex();
        actual.len() == normalized.len()
            && ring::constant_time::verify_slices_are_equal(actual.as_bytes(), normalized.as_bytes())
                .is_ok()
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Decode hex (either case) to bytes. `None` on odd length or non-hex input.
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push(hi << 4 | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Normalize a client-supplied hash to lowercase hex, validating it.
pub fn normalize_hash(s: &str) -> Option<String> {
    let bytes = decode_hex(s)?;
    Some(encode_hex(&bytes))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// `digest_function` names something this server cannot hash.
    UnsupportedFunction(i32),
    /// The hash is not valid hex of a supported length.
    MalformedHash,
    /// The bytes do not hash to the claimed digest under any candidate.
    Mismatch,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnsupportedFunction(v) => write!(f, "unsupported digest function {v}"),
            ResolveError::MalformedHash => write!(f, "malformed hash"),
            ResolveError::Mismatch => write!(f, "digest mismatch"),
        }
    }
}

/// Resolve which function verifies `data` against `expected_hex`.
///
/// `proto_function` is the client's stated `DigestFunction` value, where 0
/// (unset/unknown) means "infer": candidates come from hash length and the
/// first one that verifies wins. An explicitly stated but unsupported
/// function is an error, never silently inferred.
pub fn resolve(
    data: &[u8],
    proto_function: i32,
    expected_hex: &str,
) -> Result<DigestFunction, ResolveError> {
    let Some(normalized) = normalize_hash(expected_hex) else {
        return Err(ResolveError::MalformedHash);
    };
    if proto_function != 0 {
        let Some(function) = DigestFunction::from_proto(proto_function) else {
            return Err(ResolveError::UnsupportedFunction(proto_function));
        };
        if normalized.len() != function.hex_len() {
            return Err(ResolveError::MalformedHash);
        }
        return match function.verify(data, &normalized) {
            true => Ok(function),
            false => Err(ResolveError::Mismatch),
        };
    }
    let candidates = DigestFunction::by_hex_len(normalized.len());
    if candidates.is_empty() {
        return Err(ResolveError::MalformedHash);
    }
    for candidate in candidates {
        if candidate.verify(data, &normalized) {
            return Ok(*candidate);
        }
    }
    Err(ResolveError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // echo -n "abc" | sha256sum / sha1sum / b3sum
        assert_eq!(
            DigestFunction::Sha256.hexdigest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            DigestFunction::Sha1.hexdigest(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            DigestFunction::Blake3.hexdigest(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn empty_digests_differ_per_function() {
        let empty = DigestFunction::Sha256.empty_hex();
        assert_eq!(empty, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(DigestFunction::Sha1.empty_hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            DigestFunction::Blake3.empty_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        // The empty blob is unambiguous even though sha256/blake3 share a length.
        assert_eq!(resolve(&[], 0, &empty), Ok(DigestFunction::Sha256));
    }

    #[test]
    fn hex_round_trip_and_case() {
        assert_eq!(encode_hex(&[0x0f, 0xa0]), "0fa0");
        assert_eq!(decode_hex("0Fa0"), Some(vec![0x0f, 0xa0]));
        assert_eq!(decode_hex("0fa"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(normalize_hash("AB"), Some("ab".to_string()));
    }

    #[test]
    fn resolve_explicit_and_inferred() {
        let sha256_abc = DigestFunction::Sha256.hexdigest(b"abc");
        let sha1_abc = DigestFunction::Sha1.hexdigest(b"abc");
        let blake3_abc = DigestFunction::Blake3.hexdigest(b"abc");
        assert_eq!(resolve(b"abc", 1, &sha256_abc), Ok(DigestFunction::Sha256));
        assert_eq!(resolve(b"abc", 2, &sha1_abc), Ok(DigestFunction::Sha1));
        assert_eq!(resolve(b"abc", 9, &blake3_abc), Ok(DigestFunction::Blake3));
        // Unspecified: sha1 unambiguous by length, sha256 wins over blake3 by order.
        assert_eq!(resolve(b"abc", 0, &sha1_abc), Ok(DigestFunction::Sha1));
        assert_eq!(resolve(b"abc", 0, &sha256_abc), Ok(DigestFunction::Sha256));
        assert_eq!(resolve(b"abc", 0, &blake3_abc), Ok(DigestFunction::Blake3));
        // Wrong bytes, wrong function, bad hash, unknown function.
        assert_eq!(resolve(b"abd", 0, &sha256_abc), Err(ResolveError::Mismatch));
        assert_eq!(resolve(b"abc", 1, &sha1_abc), Err(ResolveError::MalformedHash));
        assert_eq!(resolve(b"abc", 3, &sha256_abc), Err(ResolveError::UnsupportedFunction(3)));
        assert_eq!(resolve(b"abc", 0, "xyz"), Err(ResolveError::MalformedHash));
    }

    #[test]
    fn incremental_hasher_matches() {
        for function in [DigestFunction::Sha256, DigestFunction::Sha1, DigestFunction::Blake3] {
            let mut hasher = Hasher::new(function);
            hasher.update(b"a");
            hasher.update(b"bc");
            assert_eq!(hasher.finish_hex(), function.hexdigest(b"abc"));
        }
        let mut hasher = Hasher::new(DigestFunction::Sha256);
        hasher.update(b"abc");
        assert!(hasher.verify(&DigestFunction::Sha256.hexdigest(b"abc")));
    }

    #[test]
    fn proto_round_trip() {
        for function in [DigestFunction::Sha256, DigestFunction::Sha1, DigestFunction::Blake3] {
            assert_eq!(DigestFunction::from_proto(function.to_proto()), Some(function));
        }
        assert_eq!(DigestFunction::from_proto(0), None);
        assert_eq!(DigestFunction::from_proto(10), None);
    }
}
