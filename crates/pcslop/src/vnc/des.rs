//! DES encryption for classic VNC authentication. The tables and the
//! algorithm are those of FIPS PUB 46-3 (Data Encryption Standard).

#[rustfmt::skip]
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4,
    62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32, 24, 16, 8,
    57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
    61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

#[rustfmt::skip]
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31,
    38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21, 61, 29,
    36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];

#[rustfmt::skip]
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9,
    8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17,
    16, 17, 18, 19, 20, 21, 20, 21, 22, 23, 24, 25,
    24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];

#[rustfmt::skip]
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10,
    2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25,
];

#[rustfmt::skip]
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18,
    10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36,
    63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22,
    14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];

#[rustfmt::skip]
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10,
    23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2,
    41, 52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48,
    44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

#[rustfmt::skip]
const S: [[u8; 64]; 8] = [
    [14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7,
     0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8,
     4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0,
     15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13],
    [15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10,
     3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5,
     0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15,
     13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9],
    [10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8,
     13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1,
     13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7,
     1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12],
    [7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15,
     13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9,
     10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4,
     3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14],
    [2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9,
     14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6,
     4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14,
     11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3],
    [12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11,
     10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8,
     9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6,
     4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13],
    [4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1,
     13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6,
     1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2,
     6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12],
    [13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7,
     1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2,
     7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8,
     2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11],
];

/// Picks bits out of the `width`-bit `input`, numbered from 1 at the most
/// significant end as in the standard.
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &bit| (out << 1) | ((input >> (width - bit as u32)) & 1))
}

fn subkeys(key: u64) -> [u64; 16] {
    let cd = permute(key, 64, &PC1);
    let (mut c, mut d) = (cd >> 28, cd & 0xfff_ffff);
    let rotate = |v: u64, n: u32| ((v << n) | (v >> (28 - n))) & 0xfff_ffff;
    SHIFTS.map(|n| {
        c = rotate(c, n);
        d = rotate(d, n);
        permute((c << 28) | d, 56, &PC2)
    })
}

fn feistel(r: u64, k: u64) -> u64 {
    let x = permute(r, 32, &E) ^ k;
    let s = (0..8).fold(0, |out, i| {
        let b = ((x >> (42 - 6 * i)) & 0x3f) as usize;
        let row = ((b >> 4) & 2) | (b & 1);
        let col = (b >> 1) & 0xf;
        (out << 4) | S[i][row * 16 + col] as u64
    });
    permute(s, 32, &P)
}

/// Encrypts one 64-bit block with a 64-bit key (parity bits ignored).
pub fn encrypt(key: [u8; 8], block: [u8; 8]) -> [u8; 8] {
    let ks = subkeys(u64::from_be_bytes(key));
    let ip = permute(u64::from_be_bytes(block), 64, &IP);
    let (mut l, mut r) = (ip >> 32, ip & 0xffff_ffff);
    for k in ks {
        (l, r) = (r, l ^ feistel(r, k));
    }
    permute((r << 32) | l, 64, &FP).to_be_bytes()
}

/// The answer a client gives to a VNC authentication `challenge`: the
/// challenge DES-encrypted with the first 8 password bytes as the key. VNC
/// reverses the bit order of every key byte (RFC 6143 errata 4951).
pub fn vnc_auth_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, p) in key.iter_mut().zip(password) {
        *k = p.reverse_bits();
    }
    let mut out = [0; 16];
    for (o, c) in out.as_chunks_mut::<8>().0.iter_mut().zip(challenge.as_chunks::<8>().0) {
        *o = encrypt(key, *c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn matches_the_classic_worked_example() {
        let out = encrypt(hex("133457799bbcdff1").try_into().unwrap(), hex("0123456789abcdef").try_into().unwrap());
        assert_eq!(out.to_vec(), hex("85e813540f0ab405"));
    }

    #[test]
    fn matches_known_answer_vectors() {
        for (key, plain, cipher) in [
            ("0000000000000000", "0000000000000000", "8ca64de9c1b123a7"),
            ("ffffffffffffffff", "ffffffffffffffff", "7359b2163e4edc58"),
            ("3000000000000000", "1000000000000001", "958e6e627a05557b"),
            ("0123456789abcdef", "4e6f772069732074", "3fa40e8a984d4815"),
        ] {
            let out = encrypt(hex(key).try_into().unwrap(), hex(plain).try_into().unwrap());
            assert_eq!(out.to_vec(), hex(cipher), "key {key}");
        }
    }

    #[test]
    fn vnc_response_reverses_key_bits_and_pads_the_password() {
        let challenge: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        // DES-ECB of the challenge under key 0e86cece00000000, from `openssl enc -des-ecb`.
        assert_eq!(vnc_auth_response(b"pass", &challenge).to_vec(), hex("fab34049b743efecb6287ffb551dce01"));
    }

    #[test]
    fn vnc_response_uses_only_the_first_eight_password_bytes() {
        let c = [7u8; 16];
        assert_eq!(vnc_auth_response(b"12345678", &c), vnc_auth_response(b"123456789abc", &c));
        assert_ne!(vnc_auth_response(b"1234567", &c), vnc_auth_response(b"12345678", &c));
    }
}
